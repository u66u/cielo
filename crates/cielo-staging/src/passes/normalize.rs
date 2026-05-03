// Pass: Normalize (v1)
//
// Purpose:
// - Apply conservative shrinking rewrites to a fixpoint.
// - Run one speculative inline sweep for small/side-effect-free helper bodies.
// - Re-run shrinking to clean up post-inline residue.
//
// Inputs:
// - Staged Core program after Residualize+Specialize
//
// Outputs:
// - Staged Core program (mutated in place)
//
// Invariants:
// - Rewrites are semantics-preserving and bounded.
// - Recursive functions are never inlined.
// - Shrink-only phase does not add new callsites.
//
// Diagnostics:
// - None (cleanup-only pass in v1)
//
// Complexity:
// - O(program_size * fixpoint_iters) + O(program_size) speculative inline.

use std::collections::{HashMap, HashSet};

use crate::passes::constant_table;
use crate::pipeline::phases::StagedCore;
use cielo_base::span::Span;
use cielo_base::{ExprId, FuncId, HandlerId, StmtId, VarId};
use cielo_ir::core::{CoreProgram, ExprKind, ExprNode, MatchArm, StmtKind, StmtNode};
use cielo_ir::function_graph::collect_reachable_functions;
use cielo_sema::typecheck::typecheck_residual_core;

const MAX_SHRINK_ITERS: usize = 16;
const MAX_SPEC_INLINE_STMTS: usize = 6;
const MAX_SPEC_INLINE_EXPRS: usize = 24;

pub fn run(residual: StagedCore) -> StagedCore {
    let (mut program, mut diagnostics, _sema, mut facts, report) = residual.into_parts();
    shrink_to_fixpoint(&mut program);
    speculative_inline_once(&mut program);
    shrink_to_fixpoint(&mut program);
    let sema = typecheck_residual_core(&program, &mut diagnostics);
    facts.constant_table = constant_table::build_for_core(&program);
    StagedCore::new(program, diagnostics, sema, facts, report)
}

fn shrink_to_fixpoint(program: &mut CoreProgram) {
    for _ in 0..MAX_SHRINK_ITERS {
        let roots = collect_reachable_functions(program);
        let var_uses = collect_var_uses(program, &roots);
        let call_counts = collect_call_counts(program, &roots);
        let inline_once =
            collect_inline_candidates(program, &roots, &call_counts, InlineKind::Once);
        let mut rewriter = Rewriter::new(program, RewriteMode::Shrink, var_uses, inline_once);
        rewriter.rewrite_roots(&roots);
        if !rewriter.changed {
            break;
        }
    }
}

fn speculative_inline_once(program: &mut CoreProgram) {
    let roots = collect_reachable_functions(program);
    let call_counts = collect_call_counts(program, &roots);
    let inline_many = collect_inline_candidates(program, &roots, &call_counts, InlineKind::Many);
    if inline_many.is_empty() {
        return;
    }
    let mut rewriter = Rewriter::new(
        program,
        RewriteMode::InlineOnly,
        HashMap::new(),
        inline_many,
    );
    rewriter.rewrite_roots(&roots);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RewriteMode {
    Shrink,
    InlineOnly,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InlineKind {
    Once,
    Many,
}

#[derive(Clone)]
struct InlineCandidate {
    params: Vec<VarId>,
    expr: ExprId,
}

struct Rewriter<'a> {
    program: &'a mut CoreProgram,
    mode: RewriteMode,
    changed: bool,
    var_uses: HashMap<VarId, usize>,
    inline_candidates: HashMap<FuncId, InlineCandidate>,
    var_let_defs: HashMap<VarId, ExprId>,
    memo: HashMap<StmtId, StmtId>,
    visiting: HashSet<StmtId>,
    expr_memo: HashMap<ExprId, ExprId>,
    expr_visiting: HashSet<ExprId>,
}

impl<'a> Rewriter<'a> {
    fn new(
        program: &'a mut CoreProgram,
        mode: RewriteMode,
        var_uses: HashMap<VarId, usize>,
        inline_candidates: HashMap<FuncId, InlineCandidate>,
    ) -> Self {
        Self {
            var_uses,
            inline_candidates,
            var_let_defs: collect_let_value_defs(program),
            program,
            mode,
            changed: false,
            memo: HashMap::new(),
            visiting: HashSet::new(),
            expr_memo: HashMap::new(),
            expr_visiting: HashSet::new(),
        }
    }

    fn rewrite_roots(&mut self, roots: &[FuncId]) {
        for func_id in roots {
            let Some(body) = self
                .program
                .function(*func_id)
                .map(|function| function.body)
            else {
                continue;
            };
            let rewritten = self.rewrite_stmt(body);
            if let Some(function) = self.program.function_mut(*func_id) {
                function.body = rewritten;
            }
        }

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
        for (handler_id, return_body, clauses) in handler_roots {
            let rewritten_return = self.rewrite_stmt(return_body);
            let rewritten_clauses = clauses
                .into_iter()
                .map(|stmt_id| self.rewrite_stmt(stmt_id))
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
            self.memo.insert(stmt_id, stmt_id);
            self.visiting.remove(&stmt_id);
            return stmt_id;
        };

        let rewritten = match stmt.kind {
            StmtKind::Return(expr) => {
                let expr = self.rewrite_expr(expr);
                self.set_stmt(stmt_id, StmtKind::Return(expr));
                stmt_id
            }
            StmtKind::Hole { .. } | StmtKind::Error(_) => stmt_id,
            StmtKind::Let {
                binding,
                value,
                next,
            } => {
                let value = self.rewrite_expr(value);
                let next = self.rewrite_stmt(next);
                // An expression statement lowers to a let with an unused
                // binding, so dropping on use count alone would delete every
                // `print(..)`. A builtin's output is the point of the call.
                if self.mode == RewriteMode::Shrink
                    && self.var_use_count(binding) == 0
                    && !expr_has_observable_effect(self.program, value)
                {
                    self.changed = true;
                    next
                } else {
                    self.var_let_defs.insert(binding, value);
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
            }
            StmtKind::Val {
                binding,
                value,
                next,
            } => {
                let value = self.rewrite_stmt(value);
                let next = self.rewrite_stmt(next);
                if self.mode == RewriteMode::Shrink
                    && let Some(value_expr) = self.return_expr(value)
                {
                    self.changed = true;
                    self.var_let_defs.insert(binding, value_expr);
                    self.set_stmt(
                        stmt_id,
                        StmtKind::Let {
                            binding,
                            value: value_expr,
                            next,
                        },
                    );
                    stmt_id
                } else if self.mode == RewriteMode::Shrink {
                    let nested_val =
                        self.program
                            .stmt(value)
                            .and_then(|value_stmt| match value_stmt.kind {
                                StmtKind::Val {
                                    binding,
                                    value,
                                    next,
                                } => Some((binding, value, next)),
                                _ => None,
                            });
                    if let Some((inner_binding, inner_value, inner_next)) = nested_val {
                        if !stmt_mentions_var(self.program, next, inner_binding) {
                            self.changed = true;
                            let chained = self.program.push_stmt(StmtNode {
                                span: stmt.span,
                                kind: StmtKind::Val {
                                    binding,
                                    value: inner_next,
                                    next,
                                },
                            });
                            self.set_stmt(
                                stmt_id,
                                StmtKind::Val {
                                    binding: inner_binding,
                                    value: inner_value,
                                    next: chained,
                                },
                            );
                            stmt_id
                        } else {
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
                    } else {
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
                } else {
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
            }
            StmtKind::Call {
                result,
                callee,
                args,
                effects,
                next,
            } => {
                let args = args
                    .into_iter()
                    .map(|arg| self.rewrite_expr(arg))
                    .collect::<Vec<_>>();
                let next = self.rewrite_stmt(next);
                let can_inline = self.inline_candidates.get(&callee).cloned();
                if let Some(candidate) = can_inline {
                    if effects.is_empty() {
                        if let Some(inline_expr) = self.inline_call_expr(&candidate, &args) {
                            self.changed = true;
                            let inline_expr = self.rewrite_expr(inline_expr);
                            self.var_let_defs.insert(result, inline_expr);
                            self.set_stmt(
                                stmt_id,
                                StmtKind::Let {
                                    binding: result,
                                    value: inline_expr,
                                    next,
                                },
                            );
                            stmt_id
                        } else {
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
                    } else {
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
                } else {
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
            }
            StmtKind::Resume {
                result,
                resume,
                arg,
                next,
            } => {
                let arg = self.rewrite_expr(arg);
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
                let args = args.into_iter().map(|arg| self.rewrite_expr(arg)).collect();
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
                let cond = self.rewrite_expr(cond);
                let then_branch = self.rewrite_stmt(then_branch);
                let else_branch = self.rewrite_stmt(else_branch);
                if self.mode == RewriteMode::Shrink
                    && let Some(take_then) = self.resolve_bool(cond)
                {
                    self.changed = true;
                    if take_then { then_branch } else { else_branch }
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
                let scrutinee = self.rewrite_expr(scrutinee);
                let arms = arms
                    .into_iter()
                    .map(|mut arm| {
                        arm.body = self.rewrite_stmt(arm.body);
                        arm
                    })
                    .collect::<Vec<_>>();
                let default = default.map(|stmt| self.rewrite_stmt(stmt));
                if self.mode == RewriteMode::Shrink
                    && let Some(selection) = self.pick_match_branch(scrutinee, &arms, default)
                {
                    self.changed = true;
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

        self.memo.insert(stmt_id, rewritten);
        self.visiting.remove(&stmt_id);
        rewritten
    }

    fn set_stmt(&mut self, stmt_id: StmtId, kind: StmtKind) {
        if let Some(stmt) = self.program.stmt_mut(stmt_id) {
            stmt.kind = kind;
        }
    }

    fn rewrite_expr(&mut self, expr_id: ExprId) -> ExprId {
        if let Some(mapped) = self.expr_memo.get(&expr_id).copied() {
            return mapped;
        }
        if !self.expr_visiting.insert(expr_id) {
            return expr_id;
        }

        let Some(expr) = self.program.expr(expr_id).cloned() else {
            self.expr_memo.insert(expr_id, expr_id);
            self.expr_visiting.remove(&expr_id);
            return expr_id;
        };

        let rewritten = match expr.kind {
            ExprKind::Var(_) | ExprKind::Literal(_) | ExprKind::Error(_) => expr_id,
            ExprKind::Unary { op, expr } => {
                let expr = self.rewrite_expr(expr);
                self.set_expr(expr_id, ExprKind::Unary { op, expr });
                expr_id
            }
            ExprKind::Field { base, field } => {
                let base = self.rewrite_expr(base);
                self.set_expr(expr_id, ExprKind::Field { base, field });
                expr_id
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let lhs = self.rewrite_expr(lhs);
                let rhs = self.rewrite_expr(rhs);
                self.set_expr(expr_id, ExprKind::Binary { op, lhs, rhs });
                expr_id
            }
            ExprKind::PureCall { callee, args } => {
                let args = args
                    .into_iter()
                    .map(|arg| self.rewrite_expr(arg))
                    .collect::<Vec<_>>();
                if let Some(candidate) = self.inline_candidates.get(&callee).cloned() {
                    if let Some(inline_expr) = self.inline_call_expr(&candidate, &args) {
                        self.changed = true;
                        self.rewrite_expr(inline_expr)
                    } else {
                        self.set_expr(expr_id, ExprKind::PureCall { callee, args });
                        expr_id
                    }
                } else {
                    self.set_expr(expr_id, ExprKind::PureCall { callee, args });
                    expr_id
                }
            }
            ExprKind::BuiltinCall { builtin, args } => {
                let args = args
                    .into_iter()
                    .map(|arg| self.rewrite_expr(arg))
                    .collect::<Vec<_>>();
                self.set_expr(expr_id, ExprKind::BuiltinCall { builtin, args });
                expr_id
            }
            ExprKind::MakeStruct { ty, fields } => {
                let fields = fields
                    .into_iter()
                    .map(|field| self.rewrite_expr(field))
                    .collect::<Vec<_>>();
                self.set_expr(expr_id, ExprKind::MakeStruct { ty, fields });
                expr_id
            }
            ExprKind::MakeEnum {
                ty,
                variant,
                fields,
            } => {
                let fields = fields
                    .into_iter()
                    .map(|field| self.rewrite_expr(field))
                    .collect::<Vec<_>>();
                self.set_expr(
                    expr_id,
                    ExprKind::MakeEnum {
                        ty,
                        variant,
                        fields,
                    },
                );
                expr_id
            }
        };

        self.expr_memo.insert(expr_id, rewritten);
        self.expr_visiting.remove(&expr_id);
        rewritten
    }

    fn inline_call_expr(&mut self, candidate: &InlineCandidate, args: &[ExprId]) -> Option<ExprId> {
        if candidate.params.len() != args.len() {
            return None;
        }
        let param_bindings = candidate
            .params
            .iter()
            .copied()
            .zip(args.iter().copied())
            .collect::<HashMap<_, _>>();
        let mut memo = HashMap::new();
        self.clone_expr_with_subst(candidate.expr, &param_bindings, &mut memo)
    }

    fn clone_expr_with_subst(
        &mut self,
        expr_id: ExprId,
        param_bindings: &HashMap<VarId, ExprId>,
        memo: &mut HashMap<ExprId, ExprId>,
    ) -> Option<ExprId> {
        if let Some(mapped) = memo.get(&expr_id).copied() {
            return Some(mapped);
        }
        let expr = self.program.expr(expr_id)?.clone();
        let span = expr.span;
        let cloned = match expr.kind {
            ExprKind::Var(var) => param_bindings.get(&var).copied(),
            ExprKind::Literal(literal) => Some(self.program.push_expr(ExprNode {
                span,
                kind: ExprKind::Literal(literal),
            })),
            ExprKind::Field { base, field } => {
                let base = self.clone_expr_with_subst(base, param_bindings, memo)?;
                Some(self.program.push_expr(ExprNode {
                    span,
                    kind: ExprKind::Field { base, field },
                }))
            }
            ExprKind::Unary { op, expr } => {
                let subexpr = self.clone_expr_with_subst(expr, param_bindings, memo)?;
                Some(self.program.push_expr(ExprNode {
                    span,
                    kind: ExprKind::Unary { op, expr: subexpr },
                }))
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let lhs = self.clone_expr_with_subst(lhs, param_bindings, memo)?;
                let rhs = self.clone_expr_with_subst(rhs, param_bindings, memo)?;
                Some(self.program.push_expr(ExprNode {
                    span,
                    kind: ExprKind::Binary { op, lhs, rhs },
                }))
            }
            ExprKind::MakeStruct { ty, fields } => {
                let mut cloned_fields = Vec::with_capacity(fields.len());
                for field in fields {
                    cloned_fields.push(self.clone_expr_with_subst(field, param_bindings, memo)?);
                }
                Some(self.program.push_expr(ExprNode {
                    span,
                    kind: ExprKind::MakeStruct {
                        ty,
                        fields: cloned_fields,
                    },
                }))
            }
            ExprKind::MakeEnum {
                ty,
                variant,
                fields,
            } => {
                let mut cloned_fields = Vec::with_capacity(fields.len());
                for field in fields {
                    cloned_fields.push(self.clone_expr_with_subst(field, param_bindings, memo)?);
                }
                Some(self.program.push_expr(ExprNode {
                    span,
                    kind: ExprKind::MakeEnum {
                        ty,
                        variant,
                        fields: cloned_fields,
                    },
                }))
            }
            ExprKind::PureCall { .. } | ExprKind::BuiltinCall { .. } | ExprKind::Error(_) => None,
        }?;
        memo.insert(expr_id, cloned);
        Some(cloned)
    }

    fn set_expr(&mut self, expr_id: ExprId, kind: ExprKind) {
        if let Some(expr) = self.program.expr_mut(expr_id) {
            expr.kind = kind;
        }
    }

    fn var_use_count(&self, var: VarId) -> usize {
        self.var_uses.get(&var).copied().unwrap_or(0)
    }

    fn return_expr(&self, stmt_id: StmtId) -> Option<ExprId> {
        let stmt = self.program.stmt(stmt_id)?;
        match stmt.kind {
            StmtKind::Return(expr_id) => Some(expr_id),
            _ => None,
        }
    }

    fn resolve_bool(&self, expr_id: ExprId) -> Option<bool> {
        let kind = self.resolve_expr_kind(expr_id)?;
        match kind {
            ExprKind::Literal(cielo_ir::core::Literal::Bool(value)) => Some(value),
            _ => None,
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

#[derive(Clone)]
struct MatchSelection {
    body: StmtId,
    bindings: Vec<(VarId, ExprId)>,
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

fn stmt_mentions_var(program: &CoreProgram, root: StmtId, var: VarId) -> bool {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if stmt
            .child_exprs()
            .into_iter()
            .any(|expr| expr_mentions_var(program, expr, var))
        {
            return true;
        }
        stack.extend(stmt.child_stmts());
    }
    false
}

fn expr_mentions_var(program: &CoreProgram, root: ExprId, var: VarId) -> bool {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(expr_id) = stack.pop() {
        if !seen.insert(expr_id) {
            continue;
        }
        let Some(expr) = program.expr(expr_id) else {
            continue;
        };
        match &expr.kind {
            ExprKind::Var(bound) => {
                if *bound == var {
                    return true;
                }
            }
            ExprKind::Unary { expr, .. } | ExprKind::Field { base: expr, .. } => stack.push(*expr),
            ExprKind::Binary { lhs, rhs, .. } => {
                stack.push(*lhs);
                stack.push(*rhs);
            }
            ExprKind::PureCall { args, .. }
            | ExprKind::BuiltinCall { args, .. }
            | ExprKind::MakeStruct { fields: args, .. }
            | ExprKind::MakeEnum { fields: args, .. } => {
                stack.extend(args.iter().copied());
            }
            ExprKind::Literal(_) | ExprKind::Error(_) => {}
        }
    }
    false
}

/// True when evaluating `expr` produces output. Such an expression cannot be
/// deleted for being unused, nor duplicated by inlining.
fn expr_has_observable_effect(program: &CoreProgram, expr_id: ExprId) -> bool {
    let mut stack = vec![expr_id];
    let mut seen = HashSet::new();
    while let Some(expr_id) = stack.pop() {
        if !seen.insert(expr_id) {
            continue;
        }
        let Some(expr) = program.expr(expr_id) else {
            continue;
        };
        match &expr.kind {
            ExprKind::BuiltinCall { builtin, args } => {
                if builtin.is_observable() {
                    return true;
                }
                stack.extend(args.iter().copied());
            }
            ExprKind::Unary { expr, .. } | ExprKind::Field { base: expr, .. } => stack.push(*expr),
            ExprKind::Binary { lhs, rhs, .. } => {
                stack.push(*lhs);
                stack.push(*rhs);
            }
            ExprKind::PureCall { args, .. }
            | ExprKind::MakeStruct { fields: args, .. }
            | ExprKind::MakeEnum { fields: args, .. } => stack.extend(args.iter().copied()),
            ExprKind::Var(_) | ExprKind::Literal(_) | ExprKind::Error(_) => {}
        }
    }
    false
}

fn collect_var_uses(program: &CoreProgram, roots: &[FuncId]) -> HashMap<VarId, usize> {
    let mut uses: HashMap<VarId, usize> = HashMap::new();
    let mut seen_stmts = HashSet::new();
    let mut seen_exprs = HashSet::new();
    let mut stack = collect_analysis_roots(program, roots);

    while let Some(stmt_id) = stack.pop() {
        if !seen_stmts.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        for expr_id in stmt.child_exprs() {
            collect_expr_var_uses(program, expr_id, &mut seen_exprs, &mut uses);
        }
        stack.extend(stmt.child_stmts());
    }
    uses
}

fn collect_expr_var_uses(
    program: &CoreProgram,
    expr_id: ExprId,
    seen_exprs: &mut HashSet<ExprId>,
    uses: &mut HashMap<VarId, usize>,
) {
    if !seen_exprs.insert(expr_id) {
        return;
    }
    let Some(expr) = program.expr(expr_id) else {
        return;
    };
    match &expr.kind {
        ExprKind::Var(var) => {
            let next = uses.get(var).copied().unwrap_or(0).saturating_add(1);
            uses.insert(*var, next);
        }
        ExprKind::Unary { expr, .. } | ExprKind::Field { base: expr, .. } => {
            collect_expr_var_uses(program, *expr, seen_exprs, uses)
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_expr_var_uses(program, *lhs, seen_exprs, uses);
            collect_expr_var_uses(program, *rhs, seen_exprs, uses);
        }
        ExprKind::PureCall { args, .. }
        | ExprKind::BuiltinCall { args, .. }
        | ExprKind::MakeStruct { fields: args, .. }
        | ExprKind::MakeEnum { fields: args, .. } => {
            for arg in args {
                collect_expr_var_uses(program, *arg, seen_exprs, uses);
            }
        }
        ExprKind::Literal(_) | ExprKind::Error(_) => {}
    }
}

fn collect_call_counts(program: &CoreProgram, roots: &[FuncId]) -> HashMap<FuncId, usize> {
    let mut calls: HashMap<FuncId, usize> = HashMap::new();
    let mut seen_stmts = HashSet::new();
    let mut seen_exprs = HashSet::new();
    let mut stack = collect_analysis_roots(program, roots);

    while let Some(stmt_id) = stack.pop() {
        if !seen_stmts.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if let StmtKind::Call { callee, .. } = &stmt.kind {
            let next = calls.get(callee).copied().unwrap_or(0).saturating_add(1);
            calls.insert(*callee, next);
        }
        for expr_id in stmt.child_exprs() {
            collect_expr_call_counts(program, expr_id, &mut seen_exprs, &mut calls);
        }
        stack.extend(stmt.child_stmts());
    }
    calls
}

fn collect_expr_call_counts(
    program: &CoreProgram,
    expr_id: ExprId,
    seen_exprs: &mut HashSet<ExprId>,
    calls: &mut HashMap<FuncId, usize>,
) {
    if !seen_exprs.insert(expr_id) {
        return;
    }
    let Some(expr) = program.expr(expr_id) else {
        return;
    };
    match &expr.kind {
        ExprKind::PureCall { callee, args } => {
            let next = calls.get(callee).copied().unwrap_or(0).saturating_add(1);
            calls.insert(*callee, next);
            for arg in args {
                collect_expr_call_counts(program, *arg, seen_exprs, calls);
            }
        }
        ExprKind::BuiltinCall { args, .. } => {
            for arg in args {
                collect_expr_call_counts(program, *arg, seen_exprs, calls);
            }
        }
        ExprKind::Unary { expr, .. } | ExprKind::Field { base: expr, .. } => {
            collect_expr_call_counts(program, *expr, seen_exprs, calls)
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_expr_call_counts(program, *lhs, seen_exprs, calls);
            collect_expr_call_counts(program, *rhs, seen_exprs, calls);
        }
        ExprKind::MakeStruct { fields, .. } | ExprKind::MakeEnum { fields, .. } => {
            for field in fields {
                collect_expr_call_counts(program, *field, seen_exprs, calls);
            }
        }
        ExprKind::Var(_) | ExprKind::Literal(_) | ExprKind::Error(_) => {}
    }
}

fn collect_analysis_roots(program: &CoreProgram, roots: &[FuncId]) -> Vec<StmtId> {
    let mut stack = roots
        .iter()
        .filter_map(|func_id| program.function(*func_id).map(|function| function.body))
        .collect::<Vec<_>>();
    for handler in program.handlers() {
        stack.push(handler.return_body);
        stack.extend(handler.clauses.iter().map(|clause| clause.body));
    }
    stack
}

fn collect_inline_candidates(
    program: &CoreProgram,
    roots: &[FuncId],
    call_counts: &HashMap<FuncId, usize>,
    kind: InlineKind,
) -> HashMap<FuncId, InlineCandidate> {
    let reachable = roots.iter().copied().collect::<HashSet<_>>();
    let recursive = collect_direct_recursive_functions(program, &reachable);
    let mut out = HashMap::new();

    for func_id in reachable {
        if recursive.contains(&func_id) {
            continue;
        }
        let Some(function) = program.function(func_id) else {
            continue;
        };
        if !function.declared_effects.is_empty() {
            continue;
        }

        let call_count = call_counts.get(&func_id).copied().unwrap_or(0);
        let should_inline = match kind {
            InlineKind::Once => call_count == 1,
            InlineKind::Many => call_count > 1,
        };
        if !should_inline {
            continue;
        }

        let Some((ret_expr, stmt_size, expr_size)) = function_inline_shape(program, function.body)
        else {
            continue;
        };
        if kind == InlineKind::Many
            && (stmt_size > MAX_SPEC_INLINE_STMTS || expr_size > MAX_SPEC_INLINE_EXPRS)
        {
            continue;
        }
        let allowed_vars = function.params.iter().copied().collect::<HashSet<_>>();
        if !expr_is_inlineable(program, ret_expr, &allowed_vars) {
            continue;
        }
        out.insert(
            func_id,
            InlineCandidate {
                params: function.params.clone(),
                expr: ret_expr,
            },
        );
    }

    out
}

fn function_inline_shape(program: &CoreProgram, root: StmtId) -> Option<(ExprId, usize, usize)> {
    let stmt_size = count_stmt_nodes(program, root);
    let stmt = program.stmt(root)?;
    match stmt.kind {
        StmtKind::Return(expr_id) => Some((expr_id, stmt_size, count_expr_nodes(program, expr_id))),
        _ => None,
    }
}

fn count_stmt_nodes(program: &CoreProgram, root: StmtId) -> usize {
    let mut count = 0usize;
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        if let Some(stmt) = program.stmt(stmt_id) {
            count = count.saturating_add(1);
            stack.extend(stmt.child_stmts());
        }
    }
    count
}

fn count_expr_nodes(program: &CoreProgram, root: ExprId) -> usize {
    let mut count = 0usize;
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(expr_id) = stack.pop() {
        if !seen.insert(expr_id) {
            continue;
        }
        if let Some(expr) = program.expr(expr_id) {
            count = count.saturating_add(1);
            match &expr.kind {
                ExprKind::Unary { expr, .. } | ExprKind::Field { base: expr, .. } => {
                    stack.push(*expr)
                }
                ExprKind::Binary { lhs, rhs, .. } => {
                    stack.push(*lhs);
                    stack.push(*rhs);
                }
                ExprKind::PureCall { args, .. } | ExprKind::BuiltinCall { args, .. } => {
                    stack.extend(args.iter().copied())
                }
                ExprKind::MakeStruct { fields, .. } | ExprKind::MakeEnum { fields, .. } => {
                    stack.extend(fields.iter().copied());
                }
                ExprKind::Var(_) | ExprKind::Literal(_) | ExprKind::Error(_) => {}
            }
        }
    }
    count
}

fn expr_is_inlineable(
    program: &CoreProgram,
    expr_id: ExprId,
    allowed_vars: &HashSet<VarId>,
) -> bool {
    let mut stack = vec![expr_id];
    let mut seen = HashSet::new();
    while let Some(current) = stack.pop() {
        if !seen.insert(current) {
            continue;
        }
        let Some(expr) = program.expr(current) else {
            return false;
        };
        match &expr.kind {
            ExprKind::Var(var) => {
                if !allowed_vars.contains(var) {
                    return false;
                }
            }
            // A builtin is never duplicated or dropped by inlining: its output
            // is observable, so copying the call would double it.
            ExprKind::PureCall { .. }
            | ExprKind::BuiltinCall { .. }
            | ExprKind::Field { .. }
            | ExprKind::Error(_) => {
                return false;
            }
            ExprKind::Literal(_) => {}
            ExprKind::Unary { expr, .. } => stack.push(*expr),
            ExprKind::Binary { lhs, rhs, .. } => {
                stack.push(*lhs);
                stack.push(*rhs);
            }
            ExprKind::MakeStruct { fields, .. } | ExprKind::MakeEnum { fields, .. } => {
                stack.extend(fields.iter().copied());
            }
        }
    }
    true
}

fn collect_direct_recursive_functions(
    program: &CoreProgram,
    reachable: &HashSet<FuncId>,
) -> HashSet<FuncId> {
    let mut recursive = HashSet::new();
    for func_id in reachable {
        let Some(function) = program.function(*func_id) else {
            continue;
        };
        if function_calls_target(program, function.body, *func_id) {
            recursive.insert(*func_id);
        }
    }
    recursive
}

fn function_calls_target(program: &CoreProgram, root: StmtId, target: FuncId) -> bool {
    let mut stack = vec![root];
    let mut seen_stmts = HashSet::new();
    let mut seen_exprs = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen_stmts.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if matches!(stmt.kind, StmtKind::Call { callee, .. } if callee == target) {
            return true;
        }
        for expr_id in stmt.child_exprs() {
            if expr_calls_target(program, expr_id, target, &mut seen_exprs) {
                return true;
            }
        }
        stack.extend(stmt.child_stmts());
    }
    false
}

fn expr_calls_target(
    program: &CoreProgram,
    root: ExprId,
    target: FuncId,
    seen: &mut HashSet<ExprId>,
) -> bool {
    if !seen.insert(root) {
        return false;
    }
    let Some(expr) = program.expr(root) else {
        return false;
    };
    match &expr.kind {
        ExprKind::PureCall { callee, args } => {
            if *callee == target {
                return true;
            }
            args.iter()
                .copied()
                .any(|arg| expr_calls_target(program, arg, target, seen))
        }
        ExprKind::BuiltinCall { args, .. } => args
            .iter()
            .copied()
            .any(|arg| expr_calls_target(program, arg, target, seen)),
        ExprKind::Field { base, .. } => expr_calls_target(program, *base, target, seen),
        ExprKind::Unary { expr, .. } => expr_calls_target(program, *expr, target, seen),
        ExprKind::Binary { lhs, rhs, .. } => {
            expr_calls_target(program, *lhs, target, seen)
                || expr_calls_target(program, *rhs, target, seen)
        }
        ExprKind::MakeStruct { fields, .. } | ExprKind::MakeEnum { fields, .. } => fields
            .iter()
            .copied()
            .any(|field| expr_calls_target(program, field, target, seen)),
        ExprKind::Var(_) | ExprKind::Literal(_) | ExprKind::Error(_) => false,
    }
}
