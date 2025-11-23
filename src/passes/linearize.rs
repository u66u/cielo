// Pass 8/9: linearize (Residual Core -> linear runtime IR)
//
// Inputs:
// - Residualized Core program
//
// Outputs:
// - Arena-backed LinearProgram with explicit expression/statement ids
//
// Invariants:
// - Function/variable identities are preserved
// - Core node sharing is preserved via memoized ID mapping
//
// Diagnostics:
// - `LINEARIZE_UNKNOWN_CALLEE` when a call target cannot be resolved
// - `LINEARIZE_UNKNOWN_HANDLER` when a handler id cannot be resolved
//
// Complexity:
// - O(expr_count + stmt_count)

use std::collections::HashMap;

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

    for (idx, function) in program.functions().iter().enumerate() {
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
        linear.functions.push(LinearFunction {
            id: FuncId::new(idx),
            name: function.name,
            params: function.params.clone(),
            body,
        });
    }

    linear.entrypoints = program.entrypoints().to_vec();
    linear
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
            LinearStmt::Call {
                result: *result,
                callee: callee_name,
                convention: classify_call_convention(effects, sema),
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
            }
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
                lower_matching_clause(
                    program,
                    clause,
                    args,
                    handler,
                    *result,
                    *next,
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
                )
            }
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
            );
            let lowered_args = args
                .iter()
                .copied()
                .map(|arg| lower_expr(program, arg, fn_names, linear, expr_map))
                .collect();
            linear.push_stmt(LinearStmt::Call {
                result: *result,
                callee: callee_name,
                convention: classify_call_convention(effects, sema),
                args: lowered_args,
                next: lowered_next,
            })
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

#[allow(clippy::too_many_arguments)]
fn lower_matching_clause(
    program: &CoreProgram,
    clause: &HandlerClause,
    args: &[ExprId],
    handler: &HandlerDef,
    perform_result: Option<VarId>,
    perform_next: StmtId,
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
    );
    let mut current = if clause.resume_param.is_some() {
        let continuation = lower_stmt_under_handler(
            program,
            perform_next,
            handler,
            fn_names,
            sema,
            diagnostics,
            linear,
            expr_map,
            stmt_map,
        );
        let binding = perform_result.unwrap_or_else(|| {
            clause
                .resume_param
                .expect("resumptive clauses must carry a resume parameter var")
        });
        linear.push_stmt(LinearStmt::Val {
            binding,
            value: clause_body,
            next: continuation,
        })
    } else {
        clause_body
    };

    for (param, arg) in clause.params.iter().copied().zip(args.iter().copied()).rev() {
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
