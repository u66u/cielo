// Pass 8/9: linearize (Residual Core -> linear runtime IR)
//
// Inputs:
// - Residualized Core program
//
// Outputs:
// - Arena-backed LinearProgram with explicit expression/statement ids
//
// Invariants:
// - Variable identities are preserved
// - Function ids are remapped densely after reachability pruning
// - Core node sharing is preserved via memoized ID mapping
//
// Diagnostics:
// - `LINEARIZE_UNKNOWN_CALLEE` when a call target cannot be resolved
// - `LINEARIZE_UNKNOWN_HANDLER` when a handler id cannot be resolved
//
// Complexity:
// - O(expr_count + stmt_count)

use std::collections::{HashMap, HashSet};

use crate::common::diagnostics::DiagnosticBag;
use crate::common::ids::{ExprId, FuncId, LinearExprId, LinearStmtId, StmtId, SymbolId, VarId};
use crate::ir::core::{CoreProgram, ExprKind, HandlerClause, HandlerDef, StmtKind};
use crate::ir::linear::{
    CallConvention, LinearExpr, LinearFunction, LinearMatchArm, LinearProgram, LinearStmt,
};
use crate::pipeline::phases::{Residualized, SemanticTables};
use crate::sema::effect::is_thunkable;

pub fn run(mut residual: Residualized) -> Linearized {
    let sema = residual.sema().clone();
    let linear = {
        let (program, diagnostics) = residual.program_and_diagnostics_mut();
        lower_program(program, &sema, diagnostics)
    };
    Linearized { residual, linear }
}

#[derive(Clone, Debug)]
pub struct Linearized {
    pub residual: Residualized,
    pub linear: LinearProgram,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ResumeContext {
    resume_var: VarId,
    perform_result: Option<VarId>,
    continuation: StmtId,
    clause_convention: ClauseConvention,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ClauseConvention {
    Pure,
    Direct,
    Control,
}

fn lower_program(
    program: &CoreProgram,
    sema: &SemanticTables,
    diagnostics: &mut DiagnosticBag,
) -> LinearProgram {
    let fn_names: HashMap<FuncId, SymbolId> = program
        .functions()
        .iter()
        .enumerate()
        .map(|(idx, func)| (FuncId::new(idx), func.name))
        .collect();

    let mut linear = LinearProgram::default();
    let mut expr_map: Vec<Option<LinearExprId>> = vec![None; program.exprs().len()];
    let mut stmt_map: Vec<Option<LinearStmtId>> = vec![None; program.stmts().len()];
    let reachable = collect_reachable_functions(program);
    let dense_id_by_source: HashMap<FuncId, FuncId> = reachable
        .iter()
        .copied()
        .enumerate()
        .map(|(dense_idx, source_id)| (source_id, FuncId::new(dense_idx)))
        .collect();

    for source_id in reachable {
        let Some(function) = program.function(source_id) else {
            continue;
        };
        let body = lower_stmt(
            program,
            function.body,
            &fn_names,
            sema,
            diagnostics,
            &mut linear,
            &mut expr_map,
            &mut stmt_map,
        );
        let dense_id = dense_id_by_source
            .get(&source_id)
            .copied()
            .unwrap_or(FuncId::INVALID);
        linear.functions.push(LinearFunction {
            id: dense_id,
            name: function.name,
            params: function.params.clone(),
            body,
        });
    }

    linear.entrypoints = program
        .entrypoints()
        .iter()
        .filter_map(|source| dense_id_by_source.get(source).copied())
        .collect();
    linear
}

fn collect_reachable_functions(program: &CoreProgram) -> Vec<FuncId> {
    let mut seen = HashSet::new();
    let mut stack = program.entrypoints().to_vec();

    while let Some(func_id) = stack.pop() {
        if !seen.insert(func_id) {
            continue;
        }
        let Some(function) = program.function(func_id) else {
            continue;
        };
        for callee in collect_stmt_callees(program, function.body) {
            stack.push(callee);
        }
    }

    let mut out = seen.into_iter().collect::<Vec<_>>();
    out.sort_by_key(|id| id.index());
    out
}

fn collect_stmt_callees(program: &CoreProgram, root: StmtId) -> Vec<FuncId> {
    let mut callees = HashSet::new();
    let mut seen_stmts = HashSet::new();
    let mut seen_exprs = HashSet::new();
    let mut stack = vec![root];

    while let Some(stmt_id) = stack.pop() {
        if !seen_stmts.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if let StmtKind::Call { callee, .. } = stmt.kind {
            callees.insert(callee);
        }
        for expr_id in stmt.child_exprs() {
            collect_expr_callees(program, expr_id, &mut seen_exprs, &mut callees);
        }
        for child in stmt.child_stmts() {
            stack.push(child);
        }
    }

    let mut out = callees.into_iter().collect::<Vec<_>>();
    out.sort_by_key(|id| id.index());
    out
}

fn collect_expr_callees(
    program: &CoreProgram,
    expr_id: ExprId,
    seen_exprs: &mut HashSet<ExprId>,
    out: &mut HashSet<FuncId>,
) {
    if !seen_exprs.insert(expr_id) {
        return;
    }
    let Some(expr) = program.expr(expr_id) else {
        return;
    };
    match &expr.kind {
        ExprKind::Unary { expr, .. } => collect_expr_callees(program, *expr, seen_exprs, out),
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_expr_callees(program, *lhs, seen_exprs, out);
            collect_expr_callees(program, *rhs, seen_exprs, out);
        }
        ExprKind::PureCall { callee, args } => {
            out.insert(*callee);
            for arg in args {
                collect_expr_callees(program, *arg, seen_exprs, out);
            }
        }
        ExprKind::MakeStruct { fields, .. } | ExprKind::MakeEnum { fields, .. } => {
            for field in fields {
                collect_expr_callees(program, *field, seen_exprs, out);
            }
        }
        ExprKind::Var(_) | ExprKind::Literal(_) | ExprKind::Error(_) => {}
    }
}

fn lower_stmt(
    program: &CoreProgram,
    stmt_id: StmtId,
    fn_names: &HashMap<FuncId, SymbolId>,
    sema: &SemanticTables,
    diagnostics: &mut DiagnosticBag,
    linear: &mut LinearProgram,
    expr_map: &mut [Option<LinearExprId>],
    stmt_map: &mut [Option<LinearStmtId>],
) -> LinearStmtId {
    if let Some(id) = stmt_map.get(stmt_id.index()).copied().flatten() {
        return id;
    }

    let Some(stmt) = program.stmt(stmt_id) else {
        let id = linear.push_stmt(LinearStmt::Error);
        stmt_map[stmt_id.index()] = Some(id);
        return id;
    };

    let kind = match &stmt.kind {
        StmtKind::Return(expr) => {
            LinearStmt::Return(lower_expr(program, *expr, fn_names, linear, expr_map))
        }
        StmtKind::Let {
            binding,
            value,
            next,
        } => LinearStmt::Let {
            binding: *binding,
            value: lower_expr(program, *value, fn_names, linear, expr_map),
            next: lower_stmt(
                program,
                *next,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
            ),
        },
        StmtKind::Val {
            binding,
            value,
            next,
        } => LinearStmt::Val {
            binding: *binding,
            value: lower_stmt(
                program,
                *value,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
            ),
            next: lower_stmt(
                program,
                *next,
                fn_names,
                sema,
                diagnostics,
                linear,
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
            ..
        } => {
            let callee_name = fn_names.get(callee).copied().unwrap_or_else(|| {
                diagnostics.error(
                    "LINEARIZE_UNKNOWN_CALLEE",
                    "Could not resolve function id while lowering call to linear IR",
                    stmt.span,
                );
                SymbolId::INVALID
            });
            let lowered_args = args
                .iter()
                .copied()
                .map(|arg| lower_expr(program, arg, fn_names, linear, expr_map))
                .collect();
            let lowered_next = lower_stmt(
                program,
                *next,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
            );
            linear_call_stmt(
                *result,
                callee_name,
                lowered_args,
                lowered_next,
                classify_call_convention(effects, sema),
            )
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => LinearStmt::If {
            cond: lower_expr(program, *cond, fn_names, linear, expr_map),
            then_branch: lower_stmt(
                program,
                *then_branch,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
            ),
            else_branch: lower_stmt(
                program,
                *else_branch,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
            ),
        },
        StmtKind::Match {
            scrutinee,
            arms,
            default,
        } => LinearStmt::Match {
            scrutinee: lower_expr(program, *scrutinee, fn_names, linear, expr_map),
            arms: arms
                .iter()
                .map(|arm| LinearMatchArm {
                    tag: arm.tag,
                    binders: arm.binders.clone(),
                    body: lower_stmt(
                        program,
                        arm.body,
                        fn_names,
                        sema,
                        diagnostics,
                        linear,
                        expr_map,
                        stmt_map,
                    ),
                })
                .collect(),
            default: default.map(|default_stmt| {
                lower_stmt(
                    program,
                    default_stmt,
                    fn_names,
                    sema,
                    diagnostics,
                    linear,
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
        } => LinearStmt::Perform {
            result: *result,
            effect: *effect,
            operation: *operation,
            args: args
                .iter()
                .copied()
                .map(|arg| lower_expr(program, arg, fn_names, linear, expr_map))
                .collect(),
            next: lower_stmt(
                program,
                *next,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
            ),
        },
        StmtKind::Resume { next, .. } => {
            diagnostics.error(
                "LINEARIZE_RESUME_OUTSIDE_HANDLER",
                "Encountered `resume` outside a handler-lowering context",
                stmt.span,
            );
            return lower_stmt(
                program,
                *next,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
            );
        }
        StmtKind::Handle {
            handler,
            body,
            next,
        } => {
            if let Some(handler_def) = program.handlers().get(handler.index()) {
                if next.is_none() {
                    let lowered = lower_stmt_under_handler(
                        program,
                        *body,
                        handler_def,
                        fn_names,
                        sema,
                        diagnostics,
                        linear,
                        expr_map,
                        stmt_map,
                        None,
                    );
                    stmt_map[stmt_id.index()] = Some(lowered);
                    return lowered;
                }

                let effect = handler_def.effect;
                LinearStmt::Handle {
                    effect,
                    body: lower_stmt(
                        program,
                        *body,
                        fn_names,
                        sema,
                        diagnostics,
                        linear,
                        expr_map,
                        stmt_map,
                    ),
                    next: next.map(|next_stmt| {
                        lower_stmt(
                            program,
                            next_stmt,
                            fn_names,
                            sema,
                            diagnostics,
                            linear,
                            expr_map,
                            stmt_map,
                        )
                    }),
                }
            } else {
                diagnostics.error(
                    "LINEARIZE_UNKNOWN_HANDLER",
                    "Could not resolve handler id while lowering to linear IR",
                    stmt.span,
                );
                let effect = crate::common::ids::EffectLabelId::INVALID;
                LinearStmt::Handle {
                    effect,
                    body: lower_stmt(
                        program,
                        *body,
                        fn_names,
                        sema,
                        diagnostics,
                        linear,
                        expr_map,
                        stmt_map,
                    ),
                    next: next.map(|next_stmt| {
                        lower_stmt(
                            program,
                            next_stmt,
                            fn_names,
                            sema,
                            diagnostics,
                            linear,
                            expr_map,
                            stmt_map,
                        )
                    }),
                }
            }
        }
        StmtKind::Stage { stage, body, next } => LinearStmt::Stage {
            stage: *stage,
            body: lower_stmt(
                program,
                *body,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
            ),
            next: next.map(|next_stmt| {
                lower_stmt(
                    program,
                    next_stmt,
                    fn_names,
                    sema,
                    diagnostics,
                    linear,
                    expr_map,
                    stmt_map,
                )
            }),
        },
        StmtKind::Hole { .. } => LinearStmt::Hole,
        StmtKind::Error(_) => LinearStmt::Error,
    };

    let id = linear.push_stmt(kind);
    stmt_map[stmt_id.index()] = Some(id);
    id
}

#[allow(clippy::too_many_arguments)]
fn lower_stmt_under_handler(
    program: &CoreProgram,
    stmt_id: StmtId,
    handler: &HandlerDef,
    fn_names: &HashMap<FuncId, SymbolId>,
    sema: &SemanticTables,
    diagnostics: &mut DiagnosticBag,
    linear: &mut LinearProgram,
    expr_map: &mut [Option<LinearExprId>],
    stmt_map: &mut [Option<LinearStmtId>],
    resume_ctx: Option<ResumeContext>,
) -> LinearStmtId {
    let Some(stmt) = program.stmt(stmt_id) else {
        return linear.push_stmt(LinearStmt::Error);
    };

    match &stmt.kind {
        StmtKind::Return(expr) => {
            let ret_value = lower_expr(program, *expr, fn_names, linear, expr_map);
            let lowered_return = lower_stmt(
                program,
                handler.return_body,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
            );
            linear.push_stmt(LinearStmt::Let {
                binding: handler.return_param,
                value: ret_value,
                next: lowered_return,
            })
        }
        StmtKind::Perform {
            result,
            effect,
            operation,
            args,
            next,
            ..
        } if *effect == handler.effect => {
            if let Some(clause) = handler
                .clauses
                .iter()
                .find(|candidate| candidate.operation == *operation)
            {
                let clause_convention = classify_clause_convention(program, clause);
                let clause_resume_ctx = clause.resume_param.map(|resume_var| ResumeContext {
                    resume_var,
                    perform_result: *result,
                    continuation: *next,
                    clause_convention,
                });
                lower_matching_clause(
                    program,
                    clause,
                    args,
                    handler,
                    clause_resume_ctx,
                    fn_names,
                    sema,
                    diagnostics,
                    linear,
                    expr_map,
                    stmt_map,
                )
            } else {
                diagnostics.error(
                    "LINEARIZE_MISSING_HANDLER_CLAUSE",
                    "Missing handler clause for performed operation",
                    stmt.span,
                );
                lower_stmt_under_handler(
                    program,
                    *next,
                    handler,
                    fn_names,
                    sema,
                    diagnostics,
                    linear,
                    expr_map,
                    stmt_map,
                    resume_ctx,
                )
            }
        }
        StmtKind::Resume {
            result,
            resume,
            arg,
            next,
        } => {
            let Some(active_ctx) = resume_ctx.filter(|ctx| *resume == ctx.resume_var) else {
                diagnostics.error(
                    "LINEARIZE_RESUME_OUTSIDE_CLAUSE",
                    "`resume` used outside the active handler clause context",
                    stmt.span,
                );
                return lower_stmt_under_handler(
                    program,
                    *next,
                    handler,
                    fn_names,
                    sema,
                    diagnostics,
                    linear,
                    expr_map,
                    stmt_map,
                    resume_ctx,
                );
            };

            let continuation = lower_stmt_under_handler(
                program,
                active_ctx.continuation,
                handler,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
                None,
            );
            let continuation = if let Some(perform_var) = active_ctx.perform_result {
                let arg_expr = lower_expr(program, *arg, fn_names, linear, expr_map);
                linear.push_stmt(LinearStmt::Let {
                    binding: perform_var,
                    value: arg_expr,
                    next: continuation,
                })
            } else {
                continuation
            };

            if is_identity_return_of_var(program, *next, *result) {
                return continuation;
            }

            if matches!(active_ctx.clause_convention, ClauseConvention::Direct) {
                // Direct clauses must resume in tail position. Preserve a conservative fallback
                // if earlier rewrites invalidate the syntactic guarantee.
                diagnostics.error(
                    "LINEARIZE_DIRECT_RESUME_NON_TAIL",
                    "Direct handler clause resume is not in tail position; lowering as control path",
                    stmt.span,
                );
            }

            let lowered_next = lower_stmt_under_handler(
                program,
                *next,
                handler,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
                None,
            );
            linear.push_stmt(LinearStmt::Val {
                binding: *result,
                value: continuation,
                next: lowered_next,
            })
        }
        StmtKind::Let {
            binding,
            value,
            next,
        } => {
            let lowered_next = lower_stmt_under_handler(
                program,
                *next,
                handler,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
                resume_ctx,
            );
            let lowered_value = lower_expr(program, *value, fn_names, linear, expr_map);
            linear.push_stmt(LinearStmt::Let {
                binding: *binding,
                value: lowered_value,
                next: lowered_next,
            })
        }
        StmtKind::Val {
            binding,
            value,
            next,
        } => {
            let lowered_value = lower_stmt_under_handler(
                program,
                *value,
                handler,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
                resume_ctx,
            );
            let lowered_next = lower_stmt_under_handler(
                program,
                *next,
                handler,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
                resume_ctx,
            );
            linear.push_stmt(LinearStmt::Val {
                binding: *binding,
                value: lowered_value,
                next: lowered_next,
            })
        }
        StmtKind::Call {
            result,
            callee,
            args,
            effects,
            next,
            ..
        } => {
            let callee_name = fn_names.get(callee).copied().unwrap_or_else(|| {
                diagnostics.error(
                    "LINEARIZE_UNKNOWN_CALLEE",
                    "Could not resolve function id while lowering call to linear IR",
                    stmt.span,
                );
                SymbolId::INVALID
            });
            let lowered_next = lower_stmt_under_handler(
                program,
                *next,
                handler,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
                resume_ctx,
            );
            let lowered_args = args
                .iter()
                .copied()
                .map(|arg| lower_expr(program, arg, fn_names, linear, expr_map))
                .collect();
            linear.push_stmt(linear_call_stmt(
                *result,
                callee_name,
                lowered_args,
                lowered_next,
                classify_call_convention(effects, sema),
            ))
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            let then_lowered = lower_stmt_under_handler(
                program,
                *then_branch,
                handler,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
                resume_ctx,
            );
            let else_lowered = lower_stmt_under_handler(
                program,
                *else_branch,
                handler,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
                resume_ctx,
            );
            let lowered_cond = lower_expr(program, *cond, fn_names, linear, expr_map);
            linear.push_stmt(LinearStmt::If {
                cond: lowered_cond,
                then_branch: then_lowered,
                else_branch: else_lowered,
            })
        }
        StmtKind::Match {
            scrutinee,
            arms,
            default,
        } => {
            let lowered_arms = arms
                .iter()
                .map(|arm| LinearMatchArm {
                    tag: arm.tag,
                    binders: arm.binders.clone(),
                    body: lower_stmt_under_handler(
                        program,
                        arm.body,
                        handler,
                        fn_names,
                        sema,
                        diagnostics,
                        linear,
                        expr_map,
                        stmt_map,
                        resume_ctx,
                    ),
                })
                .collect();
            let lowered_default = default.map(|default_stmt| {
                lower_stmt_under_handler(
                    program,
                    default_stmt,
                    handler,
                    fn_names,
                    sema,
                    diagnostics,
                    linear,
                    expr_map,
                    stmt_map,
                    resume_ctx,
                )
            });
            let lowered_scrutinee = lower_expr(program, *scrutinee, fn_names, linear, expr_map);
            linear.push_stmt(LinearStmt::Match {
                scrutinee: lowered_scrutinee,
                arms: lowered_arms,
                default: lowered_default,
            })
        }
        StmtKind::Perform {
            result,
            effect,
            operation,
            args,
            next,
        } => {
            let lowered_next = lower_stmt_under_handler(
                program,
                *next,
                handler,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
                resume_ctx,
            );
            let lowered_args = args
                .iter()
                .copied()
                .map(|arg| lower_expr(program, arg, fn_names, linear, expr_map))
                .collect();
            linear.push_stmt(LinearStmt::Perform {
                result: *result,
                effect: *effect,
                operation: *operation,
                args: lowered_args,
                next: lowered_next,
            })
        }
        StmtKind::Handle { .. } => lower_stmt(
            program,
            stmt_id,
            fn_names,
            sema,
            diagnostics,
            linear,
            expr_map,
            stmt_map,
        ),
        StmtKind::Stage { stage, body, next } => {
            let lowered_body = lower_stmt_under_handler(
                program,
                *body,
                handler,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
                resume_ctx,
            );
            let lowered_next = next.map(|next_stmt| {
                lower_stmt_under_handler(
                    program,
                    next_stmt,
                    handler,
                    fn_names,
                    sema,
                    diagnostics,
                    linear,
                    expr_map,
                    stmt_map,
                    resume_ctx,
                )
            });
            linear.push_stmt(LinearStmt::Stage {
                stage: *stage,
                body: lowered_body,
                next: lowered_next,
            })
        }
        StmtKind::Hole { .. } => linear.push_stmt(LinearStmt::Hole),
        StmtKind::Error(_) => linear.push_stmt(LinearStmt::Error),
    }
}

fn is_identity_return_of_var(program: &CoreProgram, stmt_id: StmtId, var: VarId) -> bool {
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

fn classify_clause_convention(program: &CoreProgram, clause: &HandlerClause) -> ClauseConvention {
    let Some(resume_var) = clause.resume_param else {
        return ClauseConvention::Pure;
    };
    if is_tail_resumptive_clause(program, clause.body, resume_var) {
        ClauseConvention::Direct
    } else {
        ClauseConvention::Control
    }
}

fn is_tail_resumptive_clause(program: &CoreProgram, stmt_id: StmtId, resume_var: VarId) -> bool {
    let mut seen_stmts = HashSet::new();
    is_tail_resumptive_stmt(program, stmt_id, resume_var, &mut seen_stmts)
}

fn is_tail_resumptive_stmt(
    program: &CoreProgram,
    stmt_id: StmtId,
    resume_var: VarId,
    seen_stmts: &mut HashSet<StmtId>,
) -> bool {
    if !seen_stmts.insert(stmt_id) {
        return true;
    }

    let Some(stmt) = program.stmt(stmt_id) else {
        return false;
    };

    match &stmt.kind {
        StmtKind::Return(_) => false,
        StmtKind::Let { value, next, .. } => {
            !expr_mentions_var(program, *value, resume_var)
                && is_tail_resumptive_stmt(program, *next, resume_var, seen_stmts)
        }
        StmtKind::Val {
            binding,
            value,
            next,
        } => {
            if is_identity_return_of_var(program, *next, *binding) {
                is_tail_resumptive_stmt(program, *value, resume_var, seen_stmts)
            } else {
                !stmt_mentions_var(program, *value, resume_var)
                    && is_tail_resumptive_stmt(program, *next, resume_var, seen_stmts)
            }
        }
        StmtKind::Call { args, next, .. } | StmtKind::Perform { args, next, .. } => {
            !args
                .iter()
                .copied()
                .any(|arg| expr_mentions_var(program, arg, resume_var))
                && is_tail_resumptive_stmt(program, *next, resume_var, seen_stmts)
        }
        StmtKind::Resume {
            result,
            resume,
            arg,
            next,
        } => {
            *resume == resume_var
                && !expr_mentions_var(program, *arg, resume_var)
                && is_identity_return_of_var(program, *next, *result)
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            !expr_mentions_var(program, *cond, resume_var)
                && is_tail_resumptive_stmt(program, *then_branch, resume_var, seen_stmts)
                && is_tail_resumptive_stmt(program, *else_branch, resume_var, seen_stmts)
        }
        StmtKind::Match {
            scrutinee,
            arms,
            default,
        } => {
            !expr_mentions_var(program, *scrutinee, resume_var)
                && arms
                    .iter()
                    .all(|arm| is_tail_resumptive_stmt(program, arm.body, resume_var, seen_stmts))
                && default.map_or(true, |stmt| {
                    is_tail_resumptive_stmt(program, stmt, resume_var, seen_stmts)
                })
        }
        StmtKind::Handle { .. } | StmtKind::Stage { .. } => false,
        StmtKind::Hole { .. } | StmtKind::Error(_) => true,
    }
}

fn stmt_mentions_var(program: &CoreProgram, root: StmtId, var: VarId) -> bool {
    let mut stack = vec![root];
    let mut seen_stmts = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen_stmts.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        match &stmt.kind {
            StmtKind::Return(expr) => {
                if expr_mentions_var(program, *expr, var) {
                    return true;
                }
            }
            StmtKind::Let { value, next, .. } => {
                if expr_mentions_var(program, *value, var) {
                    return true;
                }
                stack.push(*next);
            }
            StmtKind::Val { value, next, .. } => {
                stack.push(*next);
                stack.push(*value);
            }
            StmtKind::Call { args, next, .. } | StmtKind::Perform { args, next, .. } => {
                if args
                    .iter()
                    .copied()
                    .any(|arg| expr_mentions_var(program, arg, var))
                {
                    return true;
                }
                stack.push(*next);
            }
            StmtKind::Resume {
                resume, arg, next, ..
            } => {
                if *resume == var || expr_mentions_var(program, *arg, var) {
                    return true;
                }
                stack.push(*next);
            }
            StmtKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
                if expr_mentions_var(program, *cond, var) {
                    return true;
                }
                stack.push(*else_branch);
                stack.push(*then_branch);
            }
            StmtKind::Match {
                scrutinee,
                arms,
                default,
            } => {
                if expr_mentions_var(program, *scrutinee, var) {
                    return true;
                }
                if let Some(default_stmt) = default {
                    stack.push(*default_stmt);
                }
                for arm in arms {
                    stack.push(arm.body);
                }
            }
            StmtKind::Handle { body, next, .. } | StmtKind::Stage { body, next, .. } => {
                stack.push(*body);
                if let Some(next_stmt) = next {
                    stack.push(*next_stmt);
                }
            }
            StmtKind::Hole { .. } | StmtKind::Error(_) => {}
        }
    }
    false
}

fn expr_mentions_var(program: &CoreProgram, root: ExprId, var: VarId) -> bool {
    let mut stack = vec![root];
    let mut seen_exprs = HashSet::new();
    while let Some(expr_id) = stack.pop() {
        if !seen_exprs.insert(expr_id) {
            continue;
        }
        let Some(expr) = program.expr(expr_id) else {
            continue;
        };
        match &expr.kind {
            ExprKind::Var(found) => {
                if *found == var {
                    return true;
                }
            }
            ExprKind::Unary { expr, .. } => stack.push(*expr),
            ExprKind::Binary { lhs, rhs, .. } => {
                stack.push(*rhs);
                stack.push(*lhs);
            }
            ExprKind::PureCall { args, .. }
            | ExprKind::MakeStruct { fields: args, .. }
            | ExprKind::MakeEnum { fields: args, .. } => {
                for arg in args {
                    stack.push(*arg);
                }
            }
            ExprKind::Literal(_) | ExprKind::Error(_) => {}
        }
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn lower_matching_clause(
    program: &CoreProgram,
    clause: &HandlerClause,
    args: &[ExprId],
    handler: &HandlerDef,
    resume_ctx: Option<ResumeContext>,
    fn_names: &HashMap<FuncId, SymbolId>,
    sema: &SemanticTables,
    diagnostics: &mut DiagnosticBag,
    linear: &mut LinearProgram,
    expr_map: &mut [Option<LinearExprId>],
    stmt_map: &mut [Option<LinearStmtId>],
) -> LinearStmtId {
    let clause_body = lower_stmt_under_handler(
        program,
        clause.body,
        handler,
        fn_names,
        sema,
        diagnostics,
        linear,
        expr_map,
        stmt_map,
        resume_ctx,
    );
    let mut current = clause_body;

    for (param, arg) in clause
        .params
        .iter()
        .copied()
        .zip(args.iter().copied())
        .rev()
    {
        let arg_expr = lower_expr(program, arg, fn_names, linear, expr_map);
        current = linear.push_stmt(LinearStmt::Let {
            binding: param,
            value: arg_expr,
            next: current,
        });
    }

    current
}

fn lower_expr(
    program: &CoreProgram,
    expr_id: ExprId,
    fn_names: &HashMap<FuncId, SymbolId>,
    linear: &mut LinearProgram,
    expr_map: &mut [Option<LinearExprId>],
) -> LinearExprId {
    if let Some(id) = expr_map.get(expr_id.index()).copied().flatten() {
        return id;
    }

    let Some(expr) = program.expr(expr_id) else {
        let id = linear.push_expr(LinearExpr::Error);
        expr_map[expr_id.index()] = Some(id);
        return id;
    };

    let kind = match &expr.kind {
        ExprKind::Var(var) => LinearExpr::Var(*var),
        ExprKind::Literal(lit) => LinearExpr::Literal(lit.clone()),
        ExprKind::Unary { op, expr } => LinearExpr::Unary {
            op: *op,
            expr: lower_expr(program, *expr, fn_names, linear, expr_map),
        },
        ExprKind::Binary { op, lhs, rhs } => LinearExpr::Binary {
            op: *op,
            lhs: lower_expr(program, *lhs, fn_names, linear, expr_map),
            rhs: lower_expr(program, *rhs, fn_names, linear, expr_map),
        },
        ExprKind::PureCall { callee, args } => LinearExpr::PureCall {
            callee: fn_names.get(callee).copied().unwrap_or(SymbolId::INVALID),
            args: args
                .iter()
                .copied()
                .map(|arg| lower_expr(program, arg, fn_names, linear, expr_map))
                .collect(),
        },
        ExprKind::MakeStruct { ty, fields } => LinearExpr::MakeStruct {
            ty: *ty,
            fields: fields
                .iter()
                .copied()
                .map(|field| lower_expr(program, field, fn_names, linear, expr_map))
                .collect(),
        },
        ExprKind::MakeEnum {
            ty,
            variant,
            fields,
        } => LinearExpr::MakeEnum {
            ty: *ty,
            variant: *variant,
            fields: fields
                .iter()
                .copied()
                .map(|field| lower_expr(program, field, fn_names, linear, expr_map))
                .collect(),
        },
        ExprKind::Error(_) => LinearExpr::Error,
    };

    let id = linear.push_expr(kind);
    expr_map[expr_id.index()] = Some(id);
    id
}

fn classify_call_convention(
    effects: &crate::sema::effect::SortedEffectRow,
    sema: &SemanticTables,
) -> CallConvention {
    if effects.is_empty() {
        return CallConvention::Pure;
    }
    if is_thunkable(effects, &sema.effect_properties) {
        CallConvention::Direct
    } else {
        CallConvention::Control
    }
}

fn linear_call_stmt(
    result: VarId,
    callee: SymbolId,
    args: Vec<LinearExprId>,
    next: LinearStmtId,
    convention: CallConvention,
) -> LinearStmt {
    match convention {
        CallConvention::Pure => LinearStmt::PureCall {
            result,
            callee,
            args,
            next,
        },
        CallConvention::Direct => LinearStmt::DirectCall {
            result,
            callee,
            args,
            next,
        },
        CallConvention::Control => LinearStmt::ControlCall {
            result,
            callee,
            args,
            next,
        },
    }
}
