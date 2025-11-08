// Pass 7/8: linearize (Residual Core -> linear runtime IR)
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
use crate::common::ids::{ExprId, FuncId, LinearExprId, LinearStmtId, StmtId, SymbolId};
use crate::ir::core::{CoreProgram, ExprKind, StmtKind};
use crate::ir::linear::{LinearExpr, LinearFunction, LinearMatchArm, LinearProgram, LinearStmt};
use crate::pipeline::phases::Residualized;

pub fn run(mut residual: Residualized) -> Linearized {
    let linear = lower_program(&residual.program, &mut residual.diagnostics);
    Linearized { residual, linear }
}

#[derive(Clone, Debug)]
pub struct Linearized {
    pub residual: Residualized,
    pub linear: LinearProgram,
}

fn lower_program(program: &CoreProgram, diagnostics: &mut DiagnosticBag) -> LinearProgram {
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
                diagnostics,
                linear,
                expr_map,
                stmt_map,
            ),
            next: lower_stmt(
                program,
                *next,
                fn_names,
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
                args: args
                    .iter()
                    .copied()
                    .map(|arg| lower_expr(program, arg, fn_names, linear, expr_map))
                    .collect(),
                next: lower_stmt(
                    program,
                    *next,
                    fn_names,
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
                diagnostics,
                linear,
                expr_map,
                stmt_map,
            ),
            else_branch: lower_stmt(
                program,
                *else_branch,
                fn_names,
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
            let effect = program.handlers().get(handler.index()).map(|h| h.effect);
            let effect = effect.unwrap_or_else(|| {
                diagnostics.error(
                    "LINEARIZE_UNKNOWN_HANDLER",
                    "Could not resolve handler id while lowering to linear IR",
                    stmt.span,
                );
                crate::common::ids::EffectLabelId::INVALID
            });
            LinearStmt::Handle {
                effect,
                body: lower_stmt(
                    program,
                    *body,
                    fn_names,
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
                        diagnostics,
                        linear,
                        expr_map,
                        stmt_map,
                    )
                }),
            }
        }
        StmtKind::Stage { stage, body, next } => LinearStmt::Stage {
            stage: *stage,
            body: lower_stmt(
                program,
                *body,
                fn_names,
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
