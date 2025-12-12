use std::collections::HashSet;

use crate::common::ids::{ExprId, FuncId, StmtId};
use crate::ir::core::{CoreProgram, ExprKind, StmtKind};

pub fn collect_reachable_functions(program: &CoreProgram) -> Vec<FuncId> {
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
