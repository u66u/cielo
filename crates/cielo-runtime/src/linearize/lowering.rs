use std::collections::HashMap;

use cielo_base::diagnostics::DiagnosticBag;
use cielo_base::ids::{
    ExprId, FuncId, LinearExprId, LinearFuncId, LinearStmtId, StmtId, SymbolId, VarId,
};
use cielo_ir::core::{CoreProgram, ExprKind, HandlerClause, HandlerDef, StmtKind};
use cielo_ir::effect::{SortedEffectRow, is_thunkable};
use cielo_ir::function_graph::collect_reachable_functions;
use cielo_ir::linear::{
    CallConvention, LinearExpr, LinearFunction, LinearMatchArm, LinearProgram, LinearStmt,
};
use cielo_sema::SemanticTables;

use super::analysis::{
    analyze_clause_resume, classify_clause_convention, is_identity_handler_return_clause,
    is_identity_return_of_var, linear_stmt_contains_perform_effect, resume_convention_reason,
    stmt_effect_row_contains,
};
use super::types::{ClauseConvention, ResumeContext, ResumeQualifier};

struct LoweringInput<'a> {
    program: &'a CoreProgram,
    fn_names: &'a HashMap<FuncId, SymbolId>,
    sema: &'a SemanticTables,
}

struct LoweringState<'a> {
    diagnostics: &'a mut DiagnosticBag,
    linear: &'a mut LinearProgram,
    expr_map: &'a mut [Option<LinearExprId>],
    stmt_map: &'a mut [Option<LinearStmtId>],
}

pub(super) fn lower_program(
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
    let dense_id_by_source: HashMap<FuncId, LinearFuncId> = reachable
        .iter()
        .copied()
        .enumerate()
        .map(|(dense_idx, source_id)| (source_id, LinearFuncId::new(dense_idx)))
        .collect();
    let input = LoweringInput {
        program,
        fn_names: &fn_names,
        sema,
    };
    {
        let mut state = LoweringState {
            diagnostics,
            linear: &mut linear,
            expr_map: &mut expr_map,
            stmt_map: &mut stmt_map,
        };

        for source_id in reachable {
            let Some(function) = program.function(source_id) else {
                continue;
            };
            let body = lower_stmt(&input, function.body, &mut state);
            let dense_id = dense_id_by_source
                .get(&source_id)
                .copied()
                .unwrap_or(LinearFuncId::INVALID);
            state.linear.functions.push(LinearFunction {
                id: dense_id,
                name: function.name,
                params: function.params.clone(),
                body,
            });
        }

        state.linear.entrypoints = program
            .entrypoints()
            .iter()
            .filter_map(|source| dense_id_by_source.get(source).copied())
            .collect();
    }
    linear
}

fn lower_stmt(
    input: &LoweringInput<'_>,
    stmt_id: StmtId,
    state: &mut LoweringState<'_>,
) -> LinearStmtId {
    if let Some(id) = state.stmt_map.get(stmt_id.index()).copied().flatten() {
        return id;
    }

    let Some(stmt) = input.program.stmt(stmt_id) else {
        let id = state.linear.push_stmt(LinearStmt::Error);
        state.stmt_map[stmt_id.index()] = Some(id);
        return id;
    };

    let kind = match &stmt.kind {
        StmtKind::Return(expr) => LinearStmt::Return(lower_expr(input, *expr, state)),
        StmtKind::Let {
            binding,
            value,
            next,
        } => LinearStmt::Let {
            binding: *binding,
            value: lower_expr(input, *value, state),
            next: lower_stmt(input, *next, state),
        },
        StmtKind::Val {
            binding,
            value,
            next,
        } => LinearStmt::Val {
            binding: *binding,
            value: lower_stmt(input, *value, state),
            next: lower_stmt(input, *next, state),
        },
        StmtKind::Call {
            result,
            callee,
            args,
            effects,
            next,
            ..
        } => {
            let callee_name = input.fn_names.get(callee).copied().unwrap_or_else(|| {
                state.diagnostics.error(
                    "LINEARIZE_UNKNOWN_CALLEE",
                    "Could not resolve function id while lowering call to linear IR",
                    stmt.span,
                );
                SymbolId::INVALID
            });
            let lowered_args = args
                .iter()
                .copied()
                .map(|arg| lower_expr(input, arg, state))
                .collect();
            let lowered_next = lower_stmt(input, *next, state);
            linear_call_stmt(
                *result,
                callee_name,
                lowered_args,
                lowered_next,
                classify_call_convention(effects, input.sema),
            )
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => LinearStmt::If {
            cond: lower_expr(input, *cond, state),
            then_branch: lower_stmt(input, *then_branch, state),
            else_branch: lower_stmt(input, *else_branch, state),
        },
        StmtKind::Match {
            scrutinee,
            arms,
            default,
        } => LinearStmt::Match {
            scrutinee: lower_expr(input, *scrutinee, state),
            arms: arms
                .iter()
                .map(|arm| LinearMatchArm {
                    tag: arm.tag,
                    binders: arm.binders.clone(),
                    body: lower_stmt(input, arm.body, state),
                })
                .collect(),
            default: default.map(|default_stmt| lower_stmt(input, default_stmt, state)),
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
                .map(|arg| lower_expr(input, arg, state))
                .collect(),
            next: lower_stmt(input, *next, state),
        },
        StmtKind::Resume { next, .. } => {
            state.diagnostics.error(
                "LINEARIZE_RESUME_OUTSIDE_HANDLER",
                "Encountered `resume` outside a handler-lowering context",
                stmt.span,
            );
            return lower_stmt(input, *next, state);
        }
        StmtKind::Handle {
            handler,
            body,
            next,
        } => {
            if let Some(handler_def) = input.program.handlers().get(handler.index()) {
                let is_dead_handler =
                    !stmt_effect_row_contains(input.sema, *body, handler_def.effect);
                if is_dead_handler && is_identity_handler_return_clause(input.program, handler_def)
                {
                    state.diagnostics.note(
                        "LINEARIZE_DEAD_HANDLER_ELIMINATED",
                        "Elided handler with empty handled-effect intersection and identity return clause",
                        stmt.span,
                    );
                    let lowered_body = lower_stmt(input, *body, state);
                    if let Some(next_stmt) = next {
                        let lowered_next = lower_stmt(input, *next_stmt, state);
                        let lowered = state.linear.push_stmt(LinearStmt::Val {
                            binding: handler_def.return_param,
                            value: lowered_body,
                            next: lowered_next,
                        });
                        state.stmt_map[stmt_id.index()] = Some(lowered);
                        return lowered;
                    }
                    state.stmt_map[stmt_id.index()] = Some(lowered_body);
                    return lowered_body;
                }

                let lowered_body = lower_stmt_under_handler(input, *body, handler_def, state, None);
                if linear_stmt_contains_perform_effect(
                    state.linear,
                    lowered_body,
                    handler_def.effect,
                ) {
                    state.diagnostics.error(
                        "LINEARIZE_HANDLED_EFFECT_LEAK",
                        "Handled effect perform leaked across linearize boundary",
                        stmt.span,
                    );
                }
                if let Some(next_stmt) = next {
                    let lowered_next = lower_stmt(input, *next_stmt, state);
                    let lowered = state.linear.push_stmt(LinearStmt::Val {
                        binding: handler_def.return_param,
                        value: lowered_body,
                        next: lowered_next,
                    });
                    state.stmt_map[stmt_id.index()] = Some(lowered);
                    return lowered;
                }
                state.stmt_map[stmt_id.index()] = Some(lowered_body);
                return lowered_body;
            } else {
                state.diagnostics.error(
                    "LINEARIZE_UNKNOWN_HANDLER",
                    "Could not resolve handler id while lowering to linear IR",
                    stmt.span,
                );
                let effect = cielo_base::ids::EffectLabelId::INVALID;
                LinearStmt::Handle {
                    effect,
                    body: lower_stmt(input, *body, state),
                    next: next.map(|next_stmt| lower_stmt(input, next_stmt, state)),
                }
            }
        }
        StmtKind::Stage { stage, body, next } => LinearStmt::Stage {
            stage: *stage,
            body: lower_stmt(input, *body, state),
            next: next.map(|next_stmt| lower_stmt(input, next_stmt, state)),
        },
        StmtKind::Hole { .. } => LinearStmt::Hole,
        StmtKind::Error(_) => LinearStmt::Error,
    };

    let id = state.linear.push_stmt_at(kind, stmt.span);
    state.stmt_map[stmt_id.index()] = Some(id);
    id
}

fn lower_stmt_under_handler(
    input: &LoweringInput<'_>,
    stmt_id: StmtId,
    handler: &HandlerDef,
    state: &mut LoweringState<'_>,
    resume_ctx: Option<ResumeContext>,
) -> LinearStmtId {
    let Some(stmt) = input.program.stmt(stmt_id) else {
        return state.linear.push_stmt(LinearStmt::Error);
    };

    match &stmt.kind {
        StmtKind::Return(expr) => {
            let ret_value = lower_expr(input, *expr, state);
            let lowered_return = lower_stmt(input, handler.return_body, state);
            state.linear.push_stmt(LinearStmt::Let {
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
                let resume_analysis = analyze_clause_resume(input.program, clause);
                if clause.resume_param.is_some() {
                    state.diagnostics.note(
                        "LINEARIZE_RESUME_QUALIFIER",
                        format!(
                            "handler clause resume qualifier: {} (min={}, max={}, tail={})",
                            resume_analysis.qualifier.as_str(),
                            resume_analysis.min_uses.as_str(),
                            resume_analysis.max_uses.as_str(),
                            resume_analysis.tail_resumptive
                        ),
                        clause.span,
                    );
                }
                if matches!(resume_analysis.qualifier, ResumeQualifier::Multi) {
                    state.diagnostics.error(
                        "LINEARIZE_MULTI_SHOT_RESUME",
                        "Handler clause resumes the continuation more than once; v1 supports single-shot resumptions only",
                        clause.span,
                    );
                }
                let clause_convention = classify_clause_convention(clause, resume_analysis);
                if clause.resume_param.is_some() {
                    state.diagnostics.note(
                        "LINEARIZE_RESUME_LOWERING_GATE",
                        format!(
                            "handler clause lowering gate: {} path selected (reason: {})",
                            clause_convention.as_str(),
                            resume_convention_reason(resume_analysis, clause_convention),
                        ),
                        clause.span,
                    );
                }
                let clause_resume_ctx = clause.resume_param.map(|resume_var| ResumeContext {
                    resume_var,
                    perform_result: *result,
                    continuation: *next,
                    clause_convention,
                });
                lower_matching_clause(input, clause, args, handler, clause_resume_ctx, state)
            } else {
                state.diagnostics.error(
                    "LINEARIZE_MISSING_HANDLER_CLAUSE",
                    "Missing handler clause for performed operation",
                    stmt.span,
                );
                lower_stmt_under_handler(input, *next, handler, state, resume_ctx)
            }
        }
        StmtKind::Resume {
            result,
            resume,
            arg,
            next,
        } => {
            let Some(active_ctx) = resume_ctx.filter(|ctx| *resume == ctx.resume_var) else {
                state.diagnostics.error(
                    "LINEARIZE_RESUME_OUTSIDE_CLAUSE",
                    "`resume` used outside the active handler clause context",
                    stmt.span,
                );
                return lower_stmt_under_handler(input, *next, handler, state, resume_ctx);
            };

            let continuation =
                lower_stmt_under_handler(input, active_ctx.continuation, handler, state, None);
            let continuation = if let Some(perform_var) = active_ctx.perform_result {
                let arg_expr = lower_expr(input, *arg, state);
                state.linear.push_stmt(LinearStmt::Let {
                    binding: perform_var,
                    value: arg_expr,
                    next: continuation,
                })
            } else {
                continuation
            };

            if is_identity_return_of_var(input.program, *next, *result)
                && matches!(active_ctx.clause_convention, ClauseConvention::Direct)
            {
                return continuation;
            }

            if matches!(active_ctx.clause_convention, ClauseConvention::Direct) {
                // Direct clauses must resume in tail position. Preserve a conservative fallback
                // if earlier rewrites invalidate the syntactic guarantee.
                state.diagnostics.error(
                    "LINEARIZE_DIRECT_RESUME_NON_TAIL",
                    "Direct handler clause resume is not in tail position; lowering as control path",
                    stmt.span,
                );
            }

            let lowered_next = lower_stmt_under_handler(input, *next, handler, state, None);
            state.linear.push_stmt(LinearStmt::Val {
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
            let lowered_next = lower_stmt_under_handler(input, *next, handler, state, resume_ctx);
            let lowered_value = lower_expr(input, *value, state);
            state.linear.push_stmt(LinearStmt::Let {
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
            let lowered_value = lower_stmt_under_handler(input, *value, handler, state, resume_ctx);
            let lowered_next = lower_stmt_under_handler(input, *next, handler, state, resume_ctx);
            state.linear.push_stmt(LinearStmt::Val {
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
            let callee_name = input.fn_names.get(callee).copied().unwrap_or_else(|| {
                state.diagnostics.error(
                    "LINEARIZE_UNKNOWN_CALLEE",
                    "Could not resolve function id while lowering call to linear IR",
                    stmt.span,
                );
                SymbolId::INVALID
            });
            let lowered_next = lower_stmt_under_handler(input, *next, handler, state, resume_ctx);
            let lowered_args = args
                .iter()
                .copied()
                .map(|arg| lower_expr(input, arg, state))
                .collect();
            state.linear.push_stmt(linear_call_stmt(
                *result,
                callee_name,
                lowered_args,
                lowered_next,
                classify_call_convention(effects, input.sema),
            ))
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            let then_lowered =
                lower_stmt_under_handler(input, *then_branch, handler, state, resume_ctx);
            let else_lowered =
                lower_stmt_under_handler(input, *else_branch, handler, state, resume_ctx);
            let lowered_cond = lower_expr(input, *cond, state);
            state.linear.push_stmt(LinearStmt::If {
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
                    body: lower_stmt_under_handler(input, arm.body, handler, state, resume_ctx),
                })
                .collect();
            let lowered_default = default.map(|default_stmt| {
                lower_stmt_under_handler(input, default_stmt, handler, state, resume_ctx)
            });
            let lowered_scrutinee = lower_expr(input, *scrutinee, state);
            state.linear.push_stmt(LinearStmt::Match {
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
            let lowered_next = lower_stmt_under_handler(input, *next, handler, state, resume_ctx);
            let lowered_args = args
                .iter()
                .copied()
                .map(|arg| lower_expr(input, arg, state))
                .collect();
            state.linear.push_stmt(LinearStmt::Perform {
                result: *result,
                effect: *effect,
                operation: *operation,
                args: lowered_args,
                next: lowered_next,
            })
        }
        StmtKind::Handle { .. } => lower_stmt(input, stmt_id, state),
        StmtKind::Stage { stage, body, next } => {
            let lowered_body = lower_stmt_under_handler(input, *body, handler, state, resume_ctx);
            let lowered_next = next.map(|next_stmt| {
                lower_stmt_under_handler(input, next_stmt, handler, state, resume_ctx)
            });
            state.linear.push_stmt(LinearStmt::Stage {
                stage: *stage,
                body: lowered_body,
                next: lowered_next,
            })
        }
        StmtKind::Hole { .. } => state.linear.push_stmt(LinearStmt::Hole),
        StmtKind::Error(_) => state.linear.push_stmt(LinearStmt::Error),
    }
}

fn lower_matching_clause(
    input: &LoweringInput<'_>,
    clause: &HandlerClause,
    args: &[ExprId],
    handler: &HandlerDef,
    resume_ctx: Option<ResumeContext>,
    state: &mut LoweringState<'_>,
) -> LinearStmtId {
    let clause_body = lower_stmt_under_handler(input, clause.body, handler, state, resume_ctx);
    let mut current = clause_body;

    for (param, arg) in clause
        .params
        .iter()
        .copied()
        .zip(args.iter().copied())
        .rev()
    {
        let arg_expr = lower_expr(input, arg, state);
        current = state.linear.push_stmt(LinearStmt::Let {
            binding: param,
            value: arg_expr,
            next: current,
        });
    }

    current
}

fn lower_expr(
    input: &LoweringInput<'_>,
    expr_id: ExprId,
    state: &mut LoweringState<'_>,
) -> LinearExprId {
    if let Some(id) = state.expr_map.get(expr_id.index()).copied().flatten() {
        return id;
    }

    let Some(expr) = input.program.expr(expr_id) else {
        let id = state.linear.push_expr(LinearExpr::Error);
        state.expr_map[expr_id.index()] = Some(id);
        return id;
    };

    let kind = match &expr.kind {
        ExprKind::Var(var) => LinearExpr::Var(*var),
        ExprKind::Literal(lit) => LinearExpr::Literal(lit.clone()),
        ExprKind::Unary { op, expr } => LinearExpr::Unary {
            op: *op,
            expr: lower_expr(input, *expr, state),
        },
        ExprKind::Binary { op, lhs, rhs } => LinearExpr::Binary {
            op: *op,
            lhs: lower_expr(input, *lhs, state),
            rhs: lower_expr(input, *rhs, state),
        },
        ExprKind::PureCall { callee, args } => LinearExpr::PureCall {
            callee: input
                .fn_names
                .get(callee)
                .copied()
                .unwrap_or(SymbolId::INVALID),
            args: args
                .iter()
                .copied()
                .map(|arg| lower_expr(input, arg, state))
                .collect(),
        },
        ExprKind::MakeStruct { ty, fields } => LinearExpr::MakeStruct {
            ty: *ty,
            fields: fields
                .iter()
                .copied()
                .map(|field| lower_expr(input, field, state))
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
                .map(|field| lower_expr(input, field, state))
                .collect(),
        },
        ExprKind::Error(_) => LinearExpr::Error,
    };

    let id = state.linear.push_expr(kind);
    state.expr_map[expr_id.index()] = Some(id);
    id
}

fn classify_call_convention(effects: &SortedEffectRow, sema: &SemanticTables) -> CallConvention {
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
