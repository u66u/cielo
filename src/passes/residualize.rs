// Pass 6/9: residualize (stage-erasure + residual metadata)
//
// Inputs:
// - BTA-classified program
//
// Outputs:
// - Residualized wrapper + residual side tables
//
// Invariants:
// - Function-level effect annotations are erased from function decls
// - Call sites keep effect rows via residual summaries keyed by callee FuncId
// - CT-evaluable expressions are replaced with literal forms from CT cache
// - Known `if`/`match` branches are rewired to live bodies
// - Residual metadata object always exists
//
// Diagnostics:
// - None in v0
//
// Complexity:
// - O(functions + reachable statements)

use std::collections::{HashMap, HashSet};

use crate::common::ids::{ExprId, FuncId, HandlerId, StmtId, VarId};
use crate::common::span::Span;
use crate::ir::core::{CoreProgram, ExprKind, Literal, MatchArm, StmtKind, StmtNode};
use crate::pipeline::phases::{
    BranchDecision, BtaClassified, BtaTables, CtPropagationTables, Knownness, ResidualTables,
    Residualized, Stage,
};
use crate::sema::effect::SortedEffectRow;

pub fn run(mut bta: BtaClassified) -> Residualized {
    let ct_tables = bta.ct().clone();
    let bta_tables = bta.bta().clone();
    let pre_residual_function_roots = bta
        .program()
        .functions()
        .iter()
        .map(|function| function.body)
        .collect::<Vec<_>>();
    apply_ct_residualization(bta.program_mut(), &ct_tables, &bta_tables);

    let function_effect_summary = collect_function_effect_summary(
        &pre_residual_function_roots,
        &bta.sema().effects_of_stmt,
    );
    rewrite_call_effect_rows(bta.program_mut(), &function_effect_summary);
    erase_function_effect_annotations(bta.program_mut());
    bta.into_residualized(ResidualTables {
        function_effect_summary,
    })
}

fn collect_function_effect_summary(
    function_roots: &[StmtId],
    stmt_effects: &[SortedEffectRow],
) -> HashMap<FuncId, SortedEffectRow> {
    function_roots
        .iter()
        .enumerate()
        .map(|(idx, root_stmt)| {
            let row = stmt_effects
                .get(root_stmt.index())
                .cloned()
                .unwrap_or_else(SortedEffectRow::empty);
            (FuncId::new(idx), row)
        })
        .collect()
}

fn rewrite_call_effect_rows(
    program: &mut CoreProgram,
    summaries: &HashMap<FuncId, SortedEffectRow>,
) {
    let reachable = collect_reachable_stmts(program);
    for stmt_id in reachable {
        let Some(stmt) = program.stmt_mut(stmt_id) else {
            continue;
        };
        if let StmtKind::Call {
            callee, effects, ..
        } = &mut stmt.kind
        {
            *effects = summaries
                .get(callee)
                .cloned()
                .unwrap_or_else(SortedEffectRow::empty);
        }
    }
}

fn apply_ct_residualization(
    program: &mut CoreProgram,
    ct: &CtPropagationTables,
    bta: &BtaTables,
) {
    for (expr_id, value) in ct.ct_cache.iter() {
        if !should_embed_ct_value(expr_id, bta) {
            continue;
        }
        let Some(expr) = program.expr_mut(expr_id) else {
            continue;
        };
        expr.kind = ExprKind::Literal(value.clone());
    }

    let var_let_defs = collect_let_value_defs(program);
    let mut rewriter = StmtRewriter {
        program,
        ct,
        memo: HashMap::new(),
        visiting: HashSet::new(),
        var_let_defs,
    };
    rewriter.rewrite_function_roots();
    rewriter.rewrite_handler_roots();
}

fn should_embed_ct_value(expr_id: ExprId, bta: &BtaTables) -> bool {
    matches!(bta.stage_of_expr.get(&expr_id), Some(Stage::Ct))
        && matches!(
            bta.knownness_of_expr.get(&expr_id),
            Some(Knownness::KnownPersistable)
        )
}

fn collect_let_value_defs(program: &CoreProgram) -> HashMap<VarId, ExprId> {
    program
        .stmts()
        .iter()
        .filter_map(|stmt| match stmt.kind {
            StmtKind::Let { binding, value, .. } => Some((binding, value)),
            _ => None,
        })
        .collect()
}

struct StmtRewriter<'a> {
    program: &'a mut CoreProgram,
    ct: &'a CtPropagationTables,
    memo: HashMap<StmtId, StmtId>,
    visiting: HashSet<StmtId>,
    var_let_defs: HashMap<VarId, ExprId>,
}

#[derive(Clone)]
struct MatchSelection {
    body: StmtId,
    bindings: Vec<(crate::common::ids::VarId, ExprId)>,
}

impl StmtRewriter<'_> {
    fn rewrite_function_roots(&mut self) {
        let roots = self
            .program
            .functions()
            .iter()
            .enumerate()
            .map(|(idx, function)| (FuncId::new(idx), function.body))
            .collect::<Vec<_>>();

        for (func_id, root) in roots {
            let rewritten = self.rewrite_stmt(root);
            if let Some(function) = self.program.function_mut(func_id) {
                function.body = rewritten;
            }
        }
    }

    fn rewrite_handler_roots(&mut self) {
        let handler_roots = self
            .program
            .handlers()
            .iter()
            .enumerate()
            .map(|(idx, handler)| {
                (
                    HandlerId::new(idx),
                    handler.return_body,
                    handler
                        .clauses
                        .iter()
                        .map(|clause| clause.body)
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();

        for (handler_id, return_body, clause_bodies) in handler_roots {
            let rewritten_return = self.rewrite_stmt(return_body);
            let rewritten_clauses = clause_bodies
                .into_iter()
                .map(|body| self.rewrite_stmt(body))
                .collect::<Vec<_>>();

            if let Some(handler) = self.program.handler_mut(handler_id) {
                handler.return_body = rewritten_return;
                for (clause, rewritten_body) in handler.clauses.iter_mut().zip(rewritten_clauses) {
                    clause.body = rewritten_body;
                }
            }
        }
    }

    fn rewrite_stmt(&mut self, stmt_id: StmtId) -> StmtId {
        if let Some(mapped) = self.memo.get(&stmt_id).copied() {
            return mapped;
        }
        if !self.visiting.insert(stmt_id) {
            return stmt_id;
        }

        let Some(stmt) = self.program.stmt(stmt_id).cloned() else {
            self.visiting.remove(&stmt_id);
            self.memo.insert(stmt_id, stmt_id);
            return stmt_id;
        };

        let rewritten = match stmt.kind {
            StmtKind::Return(_) | StmtKind::Hole { .. } | StmtKind::Error(_) => stmt_id,
            StmtKind::Let {
                binding,
                value,
                next,
            } => {
                let next = self.rewrite_stmt(next);
                self.set_stmt(
                    stmt_id,
                    StmtKind::Let {
                        binding,
                        value,
                        next,
                    },
                );
                stmt_id
            }
            StmtKind::Val {
                binding,
                value,
                next,
            } => {
                let value = self.rewrite_stmt(value);
                let next = self.rewrite_stmt(next);
                self.set_stmt(
                    stmt_id,
                    StmtKind::Val {
                        binding,
                        value,
                        next,
                    },
                );
                stmt_id
            }
            StmtKind::Call {
                result,
                callee,
                args,
                effects,
                next,
            } => {
                let next = self.rewrite_stmt(next);
                self.set_stmt(
                    stmt_id,
                    StmtKind::Call {
                        result,
                        callee,
                        args,
                        effects,
                        next,
                    },
                );
                stmt_id
            }
            StmtKind::Resume {
                result,
                resume,
                arg,
                next,
            } => {
                let next = self.rewrite_stmt(next);
                self.set_stmt(
                    stmt_id,
                    StmtKind::Resume {
                        result,
                        resume,
                        arg,
                        next,
                    },
                );
                stmt_id
            }
            StmtKind::Perform {
                result,
                effect,
                operation,
                args,
                next,
            } => {
                let next = self.rewrite_stmt(next);
                self.set_stmt(
                    stmt_id,
                    StmtKind::Perform {
                        result,
                        effect,
                        operation,
                        args,
                        next,
                    },
                );
                stmt_id
            }
            StmtKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
                let then_branch = self.rewrite_stmt(then_branch);
                let else_branch = self.rewrite_stmt(else_branch);
                if let Some(live) = self.pick_if_branch(cond, then_branch, else_branch) {
                    live
                } else {
                    self.set_stmt(
                        stmt_id,
                        StmtKind::If {
                            cond,
                            then_branch,
                            else_branch,
                        },
                    );
                    stmt_id
                }
            }
            StmtKind::Match {
                scrutinee,
                arms,
                default,
            } => {
                let arms = arms
                    .into_iter()
                    .map(|mut arm| {
                        arm.body = self.rewrite_stmt(arm.body);
                        arm
                    })
                    .collect::<Vec<_>>();
                let default = default.map(|stmt| self.rewrite_stmt(stmt));
                if let Some(selection) = self.pick_match_branch(scrutinee, &arms, default) {
                    self.materialize_match_selection(selection, stmt.span)
                } else {
                    self.set_stmt(
                        stmt_id,
                        StmtKind::Match {
                            scrutinee,
                            arms,
                            default,
                        },
                    );
                    stmt_id
                }
            }
            StmtKind::Handle {
                handler,
                body,
                next,
            } => {
                let body = self.rewrite_stmt(body);
                let next = next.map(|stmt| self.rewrite_stmt(stmt));
                self.set_stmt(
                    stmt_id,
                    StmtKind::Handle {
                        handler,
                        body,
                        next,
                    },
                );
                stmt_id
            }
            StmtKind::Stage { stage, body, next } => {
                let body = self.rewrite_stmt(body);
                let next = next.map(|stmt| self.rewrite_stmt(stmt));
                self.set_stmt(stmt_id, StmtKind::Stage { stage, body, next });
                stmt_id
            }
        };

        self.visiting.remove(&stmt_id);
        self.memo.insert(stmt_id, rewritten);
        rewritten
    }

    fn set_stmt(&mut self, stmt_id: StmtId, kind: StmtKind) {
        if let Some(stmt) = self.program.stmt_mut(stmt_id) {
            stmt.kind = kind;
        }
    }

    fn pick_if_branch(
        &self,
        cond: ExprId,
        then_branch: StmtId,
        else_branch: StmtId,
    ) -> Option<StmtId> {
        let decision = self
            .ct
            .branch_decisions
            .get(&cond)
            .copied()
            .or_else(|| {
                self.ct
                    .ct_cache
                    .get(&cond)
                    .and_then(|literal| match literal {
                        Literal::Bool(true) => Some(BranchDecision::LiveTrue),
                        Literal::Bool(false) => Some(BranchDecision::LiveFalse),
                        _ => None,
                    })
            })
            .or_else(|| {
                self.program.expr(cond).and_then(|expr| match &expr.kind {
                    ExprKind::Literal(Literal::Bool(true)) => Some(BranchDecision::LiveTrue),
                    ExprKind::Literal(Literal::Bool(false)) => Some(BranchDecision::LiveFalse),
                    _ => None,
                })
            })
            .or_else(|| {
                self.resolve_expr_kind(cond).and_then(|kind| match kind {
                    ExprKind::Literal(Literal::Bool(true)) => Some(BranchDecision::LiveTrue),
                    ExprKind::Literal(Literal::Bool(false)) => Some(BranchDecision::LiveFalse),
                    _ => None,
                })
            })?;

        match decision {
            BranchDecision::LiveTrue => Some(then_branch),
            BranchDecision::LiveFalse => Some(else_branch),
            BranchDecision::Unknown => None,
        }
    }

    fn pick_match_branch(
        &self,
        scrutinee: ExprId,
        arms: &[MatchArm],
        default: Option<StmtId>,
    ) -> Option<MatchSelection> {
        let (variant, fields) = match self.resolve_expr_kind(scrutinee)? {
            ExprKind::MakeEnum {
                variant, fields, ..
            } => Some((variant, fields)),
            _ => None,
        }?;
        if let Some(arm) = arms.iter().find(|arm| arm.tag == variant) {
            if arm.binders.len() != fields.len() {
                return None;
            }
            let bindings = arm.binders.iter().copied().zip(fields).collect::<Vec<_>>();
            return Some(MatchSelection {
                body: arm.body,
                bindings,
            });
        }
        default.map(|body| MatchSelection {
            body,
            bindings: Vec::new(),
        })
    }

    fn materialize_match_selection(&mut self, selection: MatchSelection, span: Span) -> StmtId {
        let mut next = selection.body;
        for (binder, value) in selection.bindings.into_iter().rev() {
            self.var_let_defs.insert(binder, value);
            next = self.program.push_stmt(StmtNode {
                span,
                kind: StmtKind::Let {
                    binding: binder,
                    value,
                    next,
                },
            });
        }
        next
    }

    fn resolve_expr_kind(&self, expr_id: ExprId) -> Option<ExprKind> {
        let mut current = expr_id;
        let mut seen_vars = HashSet::new();

        for _ in 0..32 {
            let expr = self.program.expr(current)?;
            match expr.kind.clone() {
                ExprKind::Var(var) => {
                    if !seen_vars.insert(var) {
                        return None;
                    }
                    let Some(next_expr) = self.var_let_defs.get(&var).copied() else {
                        return Some(ExprKind::Var(var));
                    };
                    current = next_expr;
                }
                kind => return Some(kind),
            }
        }

        None
    }
}

fn erase_function_effect_annotations(program: &mut CoreProgram) {
    for function in program.functions_mut() {
        function.declared_effects = SortedEffectRow::empty();
    }
}

fn collect_reachable_stmts(program: &CoreProgram) -> Vec<StmtId> {
    let mut seen_stmts = HashSet::new();
    let mut seen_handlers = HashSet::new();
    let mut stack = program
        .functions()
        .iter()
        .map(|f| f.body)
        .collect::<Vec<_>>();

    while let Some(stmt_id) = stack.pop() {
        if !seen_stmts.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        match &stmt.kind {
            StmtKind::Return(_) | StmtKind::Hole { .. } | StmtKind::Error(_) => {}
            StmtKind::Let { next, .. }
            | StmtKind::Val { next, .. }
            | StmtKind::Call { next, .. }
            | StmtKind::Resume { next, .. }
            | StmtKind::Perform { next, .. } => stack.push(*next),
            StmtKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                stack.push(*then_branch);
                stack.push(*else_branch);
            }
            StmtKind::Match { arms, default, .. } => {
                for arm in arms {
                    stack.push(arm.body);
                }
                if let Some(default_stmt) = default {
                    stack.push(*default_stmt);
                }
            }
            StmtKind::Handle {
                handler,
                body,
                next,
            } => {
                stack.push(*body);
                if let Some(next_stmt) = next {
                    stack.push(*next_stmt);
                }
                push_handler_bodies(program, *handler, &mut seen_handlers, &mut stack);
            }
            StmtKind::Stage { body, next, .. } => {
                stack.push(*body);
                if let Some(next_stmt) = next {
                    stack.push(*next_stmt);
                }
            }
        }
    }

    let mut out = seen_stmts.into_iter().collect::<Vec<_>>();
    out.sort_by_key(|id| id.index());
    out
}

fn push_handler_bodies(
    program: &CoreProgram,
    handler: HandlerId,
    seen_handlers: &mut HashSet<HandlerId>,
    stack: &mut Vec<StmtId>,
) {
    if !seen_handlers.insert(handler) {
        return;
    }
    let Some(def) = program.handlers().get(handler.index()) else {
        return;
    };
    stack.push(def.return_body);
    for clause in &def.clauses {
        stack.push(clause.body);
    }
}
