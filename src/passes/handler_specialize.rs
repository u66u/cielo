// Pass 7/9: handler_specialize (bounded handler-call specialization groundwork)
//
// Inputs:
// - Residualized Core program
//
// Outputs:
// - Residualized Core program with specialized function copies for handle-wrapped direct calls
//
// Invariants:
// - A pair (callee, handler-shape) specializes at most once
// - Specialized functions preserve signatures and clone the original body graph
// - Recursive calls in specialized copies are retargeted to the specialized function id
// - Direct `handle { f(...) }` callsites are rewritten to direct calls to the specialized copy
// - Unreachable unspecialized copies are pruned after rewrite
//
// Complexity:
// - O(stmt_count + cloned_nodes + reachable_call_graph)

use std::collections::{HashMap, HashSet};

use crate::common::ids::{EffectLabelId, ExprId, FuncId, HandlerId, StmtId, SymbolId, VarId};
use crate::ir::core::{
    CoreProgram, ExprKind, ExprNode, FunctionDecl, HandlerDef, Literal, StmtKind, StmtNode,
};
use crate::passes::function_graph::{prune_unreachable_functions, remap_func_id};
use crate::pipeline::phases::{ResidualTables, Residualized};

pub fn run(residual: Residualized) -> Residualized {
    let (mut program, diagnostics, sema, mono, ct, bta, mut residual_tables) =
        residual.into_parts();
    specialize_handle_wrapped_calls(&mut program);
    let func_remap = prune_unreachable_functions(&mut program);
    remap_residual_tables(&mut residual_tables, &func_remap);
    Residualized::new(program, diagnostics, sema, mono, ct, bta, residual_tables)
}

#[derive(Clone)]
struct SpecializeCandidate {
    handler: HandlerId,
    shape: HandlerShapeKey,
    handle_stmt: StmtId,
    body_stmt: StmtId,
    callee: FuncId,
}

fn specialize_handle_wrapped_calls(program: &mut CoreProgram) {
    let candidates = collect_specialize_candidates(program);
    let mut specialized: HashMap<(FuncId, HandlerShapeKey), FuncId> = HashMap::new();

    for candidate in candidates {
        let specialized_callee = ensure_specialized(program, &candidate, &mut specialized);
        rewrite_direct_handle_callsite(program, &candidate, specialized_callee);
    }
}

fn remap_residual_tables(residual_tables: &mut ResidualTables, remap: &[Option<FuncId>]) {
    let summary = std::mem::take(&mut residual_tables.function_effect_summary);
    for (source_id, effects) in summary {
        let Some(mapped) = remap_func_id(remap, source_id) else {
            continue;
        };
        residual_tables
            .function_effect_summary
            .insert(mapped, effects);
    }
}

fn collect_specialize_candidates(program: &CoreProgram) -> Vec<SpecializeCandidate> {
    let mut out = Vec::new();
    let shapes = collect_handler_shapes(program);
    let mut wrapper_callee_cache = HashMap::new();
    let mut wrapper_callee_visiting = HashSet::new();
    for stmt_idx in 0..program.stmts().len() {
        let stmt_id = StmtId::new(stmt_idx);
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        let StmtKind::Handle {
            handler,
            body,
            next,
        } = &stmt.kind
        else {
            continue;
        };
        if next.is_some() {
            continue;
        }
        let Some(callee) = direct_handle_body_callee(
            program,
            *body,
            &mut wrapper_callee_cache,
            &mut wrapper_callee_visiting,
        ) else {
            continue;
        };
        let Some(shape) = shapes.get(handler.index()).cloned() else {
            continue;
        };
        out.push(SpecializeCandidate {
            handler: *handler,
            shape,
            handle_stmt: stmt_id,
            body_stmt: *body,
            callee,
        });
    }
    out
}

fn direct_handle_body_callee(
    program: &CoreProgram,
    body_stmt: StmtId,
    cache: &mut HashMap<StmtId, Option<FuncId>>,
    visiting: &mut HashSet<StmtId>,
) -> Option<FuncId> {
    wrapper_call_callee(program, body_stmt, cache, visiting)
}

fn wrapper_call_callee(
    program: &CoreProgram,
    stmt_id: StmtId,
    cache: &mut HashMap<StmtId, Option<FuncId>>,
    visiting: &mut HashSet<StmtId>,
) -> Option<FuncId> {
    if let Some(cached) = cache.get(&stmt_id).copied() {
        return cached;
    }
    if !visiting.insert(stmt_id) {
        cache.insert(stmt_id, None);
        return None;
    }

    let resolved = match program.stmt(stmt_id) {
        Some(stmt) => match &stmt.kind {
            StmtKind::Call { callee, .. } => Some(*callee),
            StmtKind::Let { next, .. } => wrapper_call_callee(program, *next, cache, visiting),
            StmtKind::Val {
                binding,
                value,
                next,
            } if is_return_of_var(program, *next, *binding) => {
                wrapper_call_callee(program, *value, cache, visiting)
            }
            StmtKind::If {
                then_branch,
                else_branch,
                ..
            } => same_callee(
                wrapper_call_callee(program, *then_branch, cache, visiting),
                wrapper_call_callee(program, *else_branch, cache, visiting),
            ),
            StmtKind::Match {
                arms,
                default: Some(default_stmt),
                ..
            } => {
                let Some(callee) = wrapper_call_callee(program, *default_stmt, cache, visiting)
                else {
                    return finish_wrapper_callee(cache, visiting, stmt_id, None);
                };
                for arm in arms {
                    let Some(arm_callee) = wrapper_call_callee(program, arm.body, cache, visiting)
                    else {
                        return finish_wrapper_callee(cache, visiting, stmt_id, None);
                    };
                    if arm_callee != callee {
                        return finish_wrapper_callee(cache, visiting, stmt_id, None);
                    }
                }
                Some(callee)
            }
            _ => None,
        },
        None => None,
    };
    finish_wrapper_callee(cache, visiting, stmt_id, resolved)
}

fn finish_wrapper_callee(
    cache: &mut HashMap<StmtId, Option<FuncId>>,
    visiting: &mut HashSet<StmtId>,
    stmt_id: StmtId,
    resolved: Option<FuncId>,
) -> Option<FuncId> {
    visiting.remove(&stmt_id);
    cache.insert(stmt_id, resolved);
    resolved
}

fn same_callee(lhs: Option<FuncId>, rhs: Option<FuncId>) -> Option<FuncId> {
    let lhs = lhs?;
    let rhs = rhs?;
    if lhs == rhs { Some(lhs) } else { None }
}

fn is_return_of_var(program: &CoreProgram, stmt_id: StmtId, var: VarId) -> bool {
    let Some(stmt) = program.stmt(stmt_id) else {
        return false;
    };
    let StmtKind::Return(expr_id) = stmt.kind else {
        return false;
    };
    let Some(expr) = program.expr(expr_id) else {
        return false;
    };
    matches!(expr.kind, ExprKind::Var(bound) if bound == var)
}

fn collect_handler_shapes(program: &CoreProgram) -> Vec<HandlerShapeKey> {
    program
        .handlers()
        .iter()
        .map(|handler| HandlerShapeKey::build(program, handler))
        .collect()
}

fn rewrite_direct_handle_callsite(
    program: &mut CoreProgram,
    candidate: &SpecializeCandidate,
    specialized_callee: FuncId,
) {
    let mut rewritten_cache = HashMap::new();
    let mut rewritten_visiting = HashSet::new();
    let Some(replacement) = build_rewritten_body(
        program,
        candidate.body_stmt,
        specialized_callee,
        &mut rewritten_cache,
        &mut rewritten_visiting,
    ) else {
        return;
    };

    let Some(handle_stmt) = program.stmt_mut(candidate.handle_stmt) else {
        return;
    };
    if matches!(handle_stmt.kind, StmtKind::Handle { next: None, .. }) {
        handle_stmt.kind = replacement;
    }
}

fn build_rewritten_body(
    program: &mut CoreProgram,
    body_stmt: StmtId,
    specialized_callee: FuncId,
    cache: &mut HashMap<StmtId, Option<StmtId>>,
    visiting: &mut HashSet<StmtId>,
) -> Option<StmtKind> {
    let stmt = program.stmt(body_stmt)?.clone();
    match stmt.kind {
        StmtKind::Call {
            result,
            args,
            effects,
            next,
            ..
        } => Some(StmtKind::Call {
            result,
            callee: specialized_callee,
            args,
            effects,
            next,
        }),
        StmtKind::Let {
            binding,
            value,
            next,
        } => {
            let rewritten_next =
                build_rewritten_stmt(program, next, specialized_callee, cache, visiting)?;
            Some(StmtKind::Let {
                binding,
                value,
                next: rewritten_next,
            })
        }
        StmtKind::Val {
            binding,
            value,
            next,
        } if is_return_of_var(program, next, binding) => {
            let rewritten_call =
                build_rewritten_stmt(program, value, specialized_callee, cache, visiting)?;
            Some(StmtKind::Val {
                binding,
                value: rewritten_call,
                next,
            })
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            let rewritten_then =
                build_rewritten_stmt(program, then_branch, specialized_callee, cache, visiting)?;
            let rewritten_else =
                build_rewritten_stmt(program, else_branch, specialized_callee, cache, visiting)?;
            Some(StmtKind::If {
                cond,
                then_branch: rewritten_then,
                else_branch: rewritten_else,
            })
        }
        StmtKind::Match {
            scrutinee,
            arms,
            default: Some(default_stmt),
        } => {
            let mut rewritten_arms = Vec::with_capacity(arms.len());
            for arm in arms {
                rewritten_arms.push(crate::ir::core::MatchArm {
                    tag: arm.tag,
                    binders: arm.binders,
                    body: build_rewritten_stmt(
                        program,
                        arm.body,
                        specialized_callee,
                        cache,
                        visiting,
                    )?,
                    span: arm.span,
                });
            }
            let rewritten_default =
                build_rewritten_stmt(program, default_stmt, specialized_callee, cache, visiting)?;
            Some(StmtKind::Match {
                scrutinee,
                arms: rewritten_arms,
                default: Some(rewritten_default),
            })
        }
        _ => None,
    }
}

fn build_rewritten_stmt(
    program: &mut CoreProgram,
    stmt_id: StmtId,
    specialized_callee: FuncId,
    cache: &mut HashMap<StmtId, Option<StmtId>>,
    visiting: &mut HashSet<StmtId>,
) -> Option<StmtId> {
    if let Some(cached) = cache.get(&stmt_id).copied() {
        return cached;
    }
    if !visiting.insert(stmt_id) {
        cache.insert(stmt_id, None);
        return None;
    }

    let resolved = match program.stmt(stmt_id) {
        Some(stmt) => {
            let span = stmt.span;
            let kind = build_rewritten_body(program, stmt_id, specialized_callee, cache, visiting)?;
            Some(program.push_stmt(StmtNode { span, kind }))
        }
        None => None,
    };
    visiting.remove(&stmt_id);
    cache.insert(stmt_id, resolved);
    resolved
}

fn ensure_specialized(
    program: &mut CoreProgram,
    candidate: &SpecializeCandidate,
    specialized: &mut HashMap<(FuncId, HandlerShapeKey), FuncId>,
) -> FuncId {
    let key = (candidate.callee, candidate.shape.clone());
    if let Some(existing) = specialized.get(&key).copied() {
        return existing;
    }

    let Some(source_decl) = program.function(candidate.callee).cloned() else {
        return candidate.callee;
    };

    // Add a placeholder copy first to obtain the stable specialized FuncId.
    let placeholder = FunctionDecl {
        body: source_decl.body,
        ..source_decl.clone()
    };
    let specialized_id = program.add_function(placeholder);

    let mut expr_map = HashMap::new();
    let mut stmt_map = HashMap::new();
    let cloned_body = clone_stmt_graph(
        program,
        source_decl.body,
        candidate.callee,
        specialized_id,
        &mut expr_map,
        &mut stmt_map,
    );
    let wrapped_body = program.push_stmt(StmtNode {
        span: source_decl.span,
        kind: StmtKind::Handle {
            handler: candidate.handler,
            body: cloned_body,
            next: None,
        },
    });

    if let Some(function) = program.function_mut(specialized_id) {
        function.body = wrapped_body;
    }

    specialized.insert(key, specialized_id);
    specialized_id
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct HandlerShapeKey {
    effect: EffectLabelId,
    return_body: String,
    clauses: Vec<ClauseShapeKey>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ClauseShapeKey {
    operation: SymbolId,
    param_count: usize,
    has_resume: bool,
    body: String,
}

impl HandlerShapeKey {
    fn build(program: &CoreProgram, handler: &HandlerDef) -> Self {
        let mut ret_scope = ScopeCanon::default();
        let _ = ret_scope.bind(handler.return_param);
        let return_body = stmt_shape_text(program, handler.return_body, &mut ret_scope);

        let clauses = handler
            .clauses
            .iter()
            .map(|clause| {
                let mut clause_scope = ScopeCanon::default();
                for param in &clause.params {
                    let _ = clause_scope.bind(*param);
                }
                if let Some(resume) = clause.resume_param {
                    let _ = clause_scope.bind(resume);
                }
                ClauseShapeKey {
                    operation: clause.operation,
                    param_count: clause.params.len(),
                    has_resume: clause.resume_param.is_some(),
                    body: stmt_shape_text(program, clause.body, &mut clause_scope),
                }
            })
            .collect();

        Self {
            effect: handler.effect,
            return_body,
            clauses,
        }
    }
}

#[derive(Default)]
struct ScopeCanon {
    map: HashMap<VarId, u32>,
    next: u32,
}

impl ScopeCanon {
    fn bind(&mut self, var: VarId) -> u32 {
        if let Some(existing) = self.map.get(&var).copied() {
            return existing;
        }
        let id = self.next;
        self.next += 1;
        self.map.insert(var, id);
        id
    }

    fn render_var(&self, var: VarId) -> String {
        if let Some(bound) = self.map.get(&var).copied() {
            return format!("b{bound}");
        }
        format!("f{}", var.as_u32())
    }
}

fn stmt_shape_text(program: &CoreProgram, stmt_id: StmtId, scope: &mut ScopeCanon) -> String {
    let Some(stmt) = program.stmt(stmt_id) else {
        return "stmt:missing".to_owned();
    };

    match &stmt.kind {
        StmtKind::Return(expr) => format!("ret({})", expr_shape_text(program, *expr, scope)),
        StmtKind::Let {
            binding,
            value,
            next,
        } => {
            let value_repr = expr_shape_text(program, *value, scope);
            let bind = scope.bind(*binding);
            let next_repr = stmt_shape_text(program, *next, scope);
            format!("let(b{bind},{value_repr},{next_repr})")
        }
        StmtKind::Val {
            binding,
            value,
            next,
        } => {
            let value_repr = stmt_shape_text(program, *value, scope);
            let bind = scope.bind(*binding);
            let next_repr = stmt_shape_text(program, *next, scope);
            format!("val(b{bind},{value_repr},{next_repr})")
        }
        StmtKind::Call {
            result,
            callee,
            args,
            effects,
            next,
        } => {
            let args_repr = args
                .iter()
                .map(|arg| expr_shape_text(program, *arg, scope))
                .collect::<Vec<_>>()
                .join(",");
            let effects_repr = effects
                .iter()
                .map(|effect| effect.as_u32().to_string())
                .collect::<Vec<_>>()
                .join(",");
            let result_id = scope.bind(*result);
            let next_repr = stmt_shape_text(program, *next, scope);
            format!(
                "call(f{},b{result_id},[{args_repr}],[{effects_repr}],{next_repr})",
                callee.as_u32()
            )
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            let cond_repr = expr_shape_text(program, *cond, scope);
            let then_repr = stmt_shape_text(program, *then_branch, scope);
            let else_repr = stmt_shape_text(program, *else_branch, scope);
            format!("if({cond_repr},{then_repr},{else_repr})")
        }
        StmtKind::Match {
            scrutinee,
            arms,
            default,
        } => {
            let scrutinee_repr = expr_shape_text(program, *scrutinee, scope);
            let arms_repr = arms
                .iter()
                .map(|arm| {
                    let mut arm_scope = ScopeCanon {
                        map: scope.map.clone(),
                        next: scope.next,
                    };
                    let binders = arm
                        .binders
                        .iter()
                        .map(|var| {
                            let id = arm_scope.bind(*var);
                            format!("b{id}")
                        })
                        .collect::<Vec<_>>()
                        .join(",");
                    let body = stmt_shape_text(program, arm.body, &mut arm_scope);
                    format!("arm(t{},[{binders}],{body})", arm.tag.as_u32())
                })
                .collect::<Vec<_>>()
                .join(",");
            let default_repr = default
                .map(|stmt| stmt_shape_text(program, stmt, scope))
                .unwrap_or_else(|| "none".to_owned());
            format!("match({scrutinee_repr},[{arms_repr}],{default_repr})")
        }
        StmtKind::Perform {
            result,
            effect,
            operation,
            args,
            next,
        } => {
            let result_repr = result
                .map(|var| format!("b{}", scope.bind(var)))
                .unwrap_or_else(|| "none".to_owned());
            let args_repr = args
                .iter()
                .map(|arg| expr_shape_text(program, *arg, scope))
                .collect::<Vec<_>>()
                .join(",");
            let next_repr = stmt_shape_text(program, *next, scope);
            format!(
                "perform(e{},op{}, {result_repr},[{args_repr}],{next_repr})",
                effect.as_u32(),
                operation.as_u32()
            )
        }
        StmtKind::Resume {
            result,
            resume,
            arg,
            next,
        } => {
            let result_bind = scope.bind(*result);
            let arg_repr = expr_shape_text(program, *arg, scope);
            let next_repr = stmt_shape_text(program, *next, scope);
            format!(
                "resume(v{},b{result_bind},{arg_repr},{next_repr})",
                resume.as_u32()
            )
        }
        StmtKind::Handle {
            handler,
            body,
            next,
        } => {
            let body_repr = stmt_shape_text(program, *body, scope);
            let next_repr = next
                .map(|stmt| stmt_shape_text(program, stmt, scope))
                .unwrap_or_else(|| "none".to_owned());
            format!("handle(h{}, {body_repr}, {next_repr})", handler.as_u32())
        }
        StmtKind::Stage { stage, body, next } => {
            let body_repr = stmt_shape_text(program, *body, scope);
            let next_repr = next
                .map(|stmt| stmt_shape_text(program, stmt, scope))
                .unwrap_or_else(|| "none".to_owned());
            format!("stage({stage:?}, {body_repr}, {next_repr})")
        }
        StmtKind::Hole { ty } => format!("hole(t{})", ty.as_u32()),
        StmtKind::Error(_) => "stmt:error".to_owned(),
    }
}

fn expr_shape_text(program: &CoreProgram, expr_id: ExprId, scope: &ScopeCanon) -> String {
    let Some(expr) = program.expr(expr_id) else {
        return "expr:missing".to_owned();
    };

    match &expr.kind {
        ExprKind::Var(var) => format!("var({})", scope.render_var(*var)),
        ExprKind::Literal(literal) => format!("lit({})", literal_shape_text(literal)),
        ExprKind::Unary { op, expr } => {
            let nested = expr_shape_text(program, *expr, scope);
            format!("un({op:?},{nested})")
        }
        ExprKind::Binary { op, lhs, rhs } => {
            let lhs_repr = expr_shape_text(program, *lhs, scope);
            let rhs_repr = expr_shape_text(program, *rhs, scope);
            format!("bin({op:?},{lhs_repr},{rhs_repr})")
        }
        ExprKind::PureCall { callee, args } => {
            let args_repr = args
                .iter()
                .map(|arg| expr_shape_text(program, *arg, scope))
                .collect::<Vec<_>>()
                .join(",");
            format!("pcall(f{},[{args_repr}])", callee.as_u32())
        }
        ExprKind::MakeStruct { ty, fields } => {
            let fields_repr = fields
                .iter()
                .map(|field| expr_shape_text(program, *field, scope))
                .collect::<Vec<_>>()
                .join(",");
            format!("mkstruct(t{},[{fields_repr}])", ty.as_u32())
        }
        ExprKind::MakeEnum {
            ty,
            variant,
            fields,
        } => {
            let fields_repr = fields
                .iter()
                .map(|field| expr_shape_text(program, *field, scope))
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "mkenum(t{},v{},[{fields_repr}])",
                ty.as_u32(),
                variant.as_u32()
            )
        }
        ExprKind::Error(_) => "expr:error".to_owned(),
    }
}

fn literal_shape_text(literal: &Literal) -> String {
    match literal {
        Literal::Unit => "unit".to_owned(),
        Literal::Bool(value) => format!("bool({value})"),
        Literal::Int(value) => format!("int({value})"),
        Literal::Float(value) => format!("float({value})"),
        Literal::Char(value) => format!("char({value})"),
        Literal::String(value) => format!("str({value})"),
    }
}

fn clone_stmt_graph(
    program: &mut CoreProgram,
    source: StmtId,
    source_func: FuncId,
    specialized_func: FuncId,
    expr_map: &mut HashMap<ExprId, ExprId>,
    stmt_map: &mut HashMap<StmtId, StmtId>,
) -> StmtId {
    if let Some(existing) = stmt_map.get(&source).copied() {
        return existing;
    }

    let Some(node) = program.stmt(source).cloned() else {
        return source;
    };

    let kind = match node.kind {
        StmtKind::Return(expr) => StmtKind::Return(clone_expr_graph(
            program,
            expr,
            source_func,
            specialized_func,
            expr_map,
        )),
        StmtKind::Let {
            binding,
            value,
            next,
        } => StmtKind::Let {
            binding,
            value: clone_expr_graph(program, value, source_func, specialized_func, expr_map),
            next: clone_stmt_graph(
                program,
                next,
                source_func,
                specialized_func,
                expr_map,
                stmt_map,
            ),
        },
        StmtKind::Val {
            binding,
            value,
            next,
        } => StmtKind::Val {
            binding,
            value: clone_stmt_graph(
                program,
                value,
                source_func,
                specialized_func,
                expr_map,
                stmt_map,
            ),
            next: clone_stmt_graph(
                program,
                next,
                source_func,
                specialized_func,
                expr_map,
                stmt_map,
            ),
        },
        StmtKind::Call {
            result,
            callee,
            args,
            effects,
            next,
        } => StmtKind::Call {
            result,
            callee: if callee == source_func {
                specialized_func
            } else {
                callee
            },
            args: args
                .into_iter()
                .map(|arg| clone_expr_graph(program, arg, source_func, specialized_func, expr_map))
                .collect(),
            effects,
            next: clone_stmt_graph(
                program,
                next,
                source_func,
                specialized_func,
                expr_map,
                stmt_map,
            ),
        },
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => StmtKind::If {
            cond: clone_expr_graph(program, cond, source_func, specialized_func, expr_map),
            then_branch: clone_stmt_graph(
                program,
                then_branch,
                source_func,
                specialized_func,
                expr_map,
                stmt_map,
            ),
            else_branch: clone_stmt_graph(
                program,
                else_branch,
                source_func,
                specialized_func,
                expr_map,
                stmt_map,
            ),
        },
        StmtKind::Match {
            scrutinee,
            arms,
            default,
        } => StmtKind::Match {
            scrutinee: clone_expr_graph(
                program,
                scrutinee,
                source_func,
                specialized_func,
                expr_map,
            ),
            arms: arms
                .into_iter()
                .map(|arm| crate::ir::core::MatchArm {
                    tag: arm.tag,
                    binders: arm.binders,
                    body: clone_stmt_graph(
                        program,
                        arm.body,
                        source_func,
                        specialized_func,
                        expr_map,
                        stmt_map,
                    ),
                    span: arm.span,
                })
                .collect(),
            default: default.map(|stmt| {
                clone_stmt_graph(
                    program,
                    stmt,
                    source_func,
                    specialized_func,
                    expr_map,
                    stmt_map,
                )
            }),
        },
        StmtKind::Perform {
            result,
            effect,
            operation,
            args,
            next,
        } => StmtKind::Perform {
            result,
            effect,
            operation,
            args: args
                .into_iter()
                .map(|arg| clone_expr_graph(program, arg, source_func, specialized_func, expr_map))
                .collect(),
            next: clone_stmt_graph(
                program,
                next,
                source_func,
                specialized_func,
                expr_map,
                stmt_map,
            ),
        },
        StmtKind::Resume {
            result,
            resume,
            arg,
            next,
        } => StmtKind::Resume {
            result,
            resume,
            arg: clone_expr_graph(program, arg, source_func, specialized_func, expr_map),
            next: clone_stmt_graph(
                program,
                next,
                source_func,
                specialized_func,
                expr_map,
                stmt_map,
            ),
        },
        StmtKind::Handle {
            handler,
            body,
            next,
        } => StmtKind::Handle {
            handler,
            body: clone_stmt_graph(
                program,
                body,
                source_func,
                specialized_func,
                expr_map,
                stmt_map,
            ),
            next: next.map(|stmt| {
                clone_stmt_graph(
                    program,
                    stmt,
                    source_func,
                    specialized_func,
                    expr_map,
                    stmt_map,
                )
            }),
        },
        StmtKind::Stage { stage, body, next } => StmtKind::Stage {
            stage,
            body: clone_stmt_graph(
                program,
                body,
                source_func,
                specialized_func,
                expr_map,
                stmt_map,
            ),
            next: next.map(|stmt| {
                clone_stmt_graph(
                    program,
                    stmt,
                    source_func,
                    specialized_func,
                    expr_map,
                    stmt_map,
                )
            }),
        },
        StmtKind::Hole { ty } => StmtKind::Hole { ty },
        StmtKind::Error(error) => StmtKind::Error(error),
    };

    let cloned = program.push_stmt(StmtNode {
        span: node.span,
        kind,
    });
    stmt_map.insert(source, cloned);
    cloned
}

fn clone_expr_graph(
    program: &mut CoreProgram,
    source: ExprId,
    source_func: FuncId,
    specialized_func: FuncId,
    expr_map: &mut HashMap<ExprId, ExprId>,
) -> ExprId {
    if let Some(existing) = expr_map.get(&source).copied() {
        return existing;
    }

    let Some(node) = program.expr(source).cloned() else {
        return source;
    };

    let kind = match node.kind {
        ExprKind::Var(var) => ExprKind::Var(var),
        ExprKind::Literal(literal) => ExprKind::Literal(literal),
        ExprKind::Unary { op, expr } => ExprKind::Unary {
            op,
            expr: clone_expr_graph(program, expr, source_func, specialized_func, expr_map),
        },
        ExprKind::Binary { op, lhs, rhs } => ExprKind::Binary {
            op,
            lhs: clone_expr_graph(program, lhs, source_func, specialized_func, expr_map),
            rhs: clone_expr_graph(program, rhs, source_func, specialized_func, expr_map),
        },
        ExprKind::PureCall { callee, args } => ExprKind::PureCall {
            callee: if callee == source_func {
                specialized_func
            } else {
                callee
            },
            args: args
                .into_iter()
                .map(|arg| clone_expr_graph(program, arg, source_func, specialized_func, expr_map))
                .collect(),
        },
        ExprKind::MakeStruct { ty, fields } => ExprKind::MakeStruct {
            ty,
            fields: fields
                .into_iter()
                .map(|field| {
                    clone_expr_graph(program, field, source_func, specialized_func, expr_map)
                })
                .collect(),
        },
        ExprKind::MakeEnum {
            ty,
            variant,
            fields,
        } => ExprKind::MakeEnum {
            ty,
            variant,
            fields: fields
                .into_iter()
                .map(|field| {
                    clone_expr_graph(program, field, source_func, specialized_func, expr_map)
                })
                .collect(),
        },
        ExprKind::Error(error) => ExprKind::Error(error),
    };

    let cloned = program.push_expr(ExprNode {
        span: node.span,
        kind,
    });
    expr_map.insert(source, cloned);
    cloned
}
