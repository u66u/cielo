// Pass 7/8: linearize (Residual Core -> linear runtime IR)
//
// Inputs:
// - Residualized Core program
//
// Outputs:
// - LinearProgram with statement trees detached from arena IDs
//
// Invariants:
// - Function/variable identities are preserved
// - All `next` edges are materialized as nested linear nodes
//
// Diagnostics:
// - `LINEARIZE_UNKNOWN_CALLEE` when a call target cannot be resolved
// - `LINEARIZE_UNKNOWN_HANDLER` when a handler id cannot be resolved
//
// Complexity:
// - O(expr_count + stmt_count)

use std::collections::HashMap;

use crate::common::diagnostics::DiagnosticBag;
use crate::common::ids::{ExprId, FuncId, StmtId, SymbolId};
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

    let functions = program
        .functions()
        .iter()
        .enumerate()
        .map(|(idx, function)| LinearFunction {
            id: FuncId::new(idx),
            name: function.name,
            params: function.params.clone(),
            body: lower_stmt(program, function.body, &fn_names, diagnostics),
        })
        .collect();

    LinearProgram {
        functions,
        entrypoints: program.entrypoints().to_vec(),
    }
}

fn lower_stmt(
    program: &CoreProgram,
    stmt_id: StmtId,
    fn_names: &HashMap<FuncId, SymbolId>,
    diagnostics: &mut DiagnosticBag,
) -> LinearStmt {
    let Some(stmt) = program.stmt(stmt_id) else {
        return LinearStmt::Error;
    };

    match &stmt.kind {
        StmtKind::Return(expr) => LinearStmt::Return(lower_expr(program, *expr, fn_names)),
        StmtKind::Let {
            binding,
            value,
            next,
        } => LinearStmt::Let {
            binding: *binding,
            value: lower_expr(program, *value, fn_names),
            next: Box::new(lower_stmt(program, *next, fn_names, diagnostics)),
        },
        StmtKind::Val {
            binding,
            value,
            next,
        } => LinearStmt::Val {
            binding: *binding,
            value: Box::new(lower_stmt(program, *value, fn_names, diagnostics)),
            next: Box::new(lower_stmt(program, *next, fn_names, diagnostics)),
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
                    .map(|arg| lower_expr(program, arg, fn_names))
                    .collect(),
                next: Box::new(lower_stmt(program, *next, fn_names, diagnostics)),
            }
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => LinearStmt::If {
            cond: lower_expr(program, *cond, fn_names),
            then_branch: Box::new(lower_stmt(program, *then_branch, fn_names, diagnostics)),
            else_branch: Box::new(lower_stmt(program, *else_branch, fn_names, diagnostics)),
        },
        StmtKind::Match {
            scrutinee,
            arms,
            default,
        } => LinearStmt::Match {
            scrutinee: lower_expr(program, *scrutinee, fn_names),
            arms: arms
                .iter()
                .map(|arm| LinearMatchArm {
                    tag: arm.tag,
                    binders: arm.binders.clone(),
                    body: Box::new(lower_stmt(program, arm.body, fn_names, diagnostics)),
                })
                .collect(),
            default: default.map(|default_stmt| {
                Box::new(lower_stmt(program, default_stmt, fn_names, diagnostics))
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
                .map(|arg| lower_expr(program, arg, fn_names))
                .collect(),
            next: Box::new(lower_stmt(program, *next, fn_names, diagnostics)),
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
                body: Box::new(lower_stmt(program, *body, fn_names, diagnostics)),
                next: next.map(|next_stmt| {
                    Box::new(lower_stmt(program, next_stmt, fn_names, diagnostics))
                }),
            }
        }
        StmtKind::Stage { stage, body, next } => LinearStmt::Stage {
            stage: *stage,
            body: Box::new(lower_stmt(program, *body, fn_names, diagnostics)),
            next: next
                .map(|next_stmt| Box::new(lower_stmt(program, next_stmt, fn_names, diagnostics))),
        },
        StmtKind::Hole { .. } => LinearStmt::Hole,
        StmtKind::Error(_) => LinearStmt::Error,
    }
}

fn lower_expr(
    program: &CoreProgram,
    expr_id: ExprId,
    fn_names: &HashMap<FuncId, SymbolId>,
) -> LinearExpr {
    let Some(expr) = program.expr(expr_id) else {
        return LinearExpr::Error;
    };

    match &expr.kind {
        ExprKind::Var(var) => LinearExpr::Var(*var),
        ExprKind::Literal(lit) => LinearExpr::Literal(lit.clone()),
        ExprKind::Unary { op, expr } => LinearExpr::Unary {
            op: *op,
            expr: Box::new(lower_expr(program, *expr, fn_names)),
        },
        ExprKind::Binary { op, lhs, rhs } => LinearExpr::Binary {
            op: *op,
            lhs: Box::new(lower_expr(program, *lhs, fn_names)),
            rhs: Box::new(lower_expr(program, *rhs, fn_names)),
        },
        ExprKind::PureCall { callee, args } => LinearExpr::PureCall {
            callee: fn_names.get(callee).copied().unwrap_or(SymbolId::INVALID),
            args: args
                .iter()
                .copied()
                .map(|arg| lower_expr(program, arg, fn_names))
                .collect(),
        },
        ExprKind::MakeStruct { ty, fields } => LinearExpr::MakeStruct {
            ty: *ty,
            fields: fields
                .iter()
                .copied()
                .map(|field| lower_expr(program, field, fn_names))
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
                .map(|field| lower_expr(program, field, fn_names))
                .collect(),
        },
        ExprKind::Error(_) => LinearExpr::Error,
    }
}
