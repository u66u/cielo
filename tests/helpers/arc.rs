#![allow(dead_code)]

use cielo::common::gc::ArcInsertRule;
use cielo::common::ids::{StmtId, VarId};
use cielo::ir::core::{CoreProgram, ExprKind, Literal, StmtKind};
use cielo::passes::arc_insert::{
    self, ArcDecisionSite, ArcDecisionTraceEntry, ArcInsertionPlan, ArcOpKind,
};

use crate::helpers::core::lower_and_typecheck;

pub fn plan_with_rules(source: &str, rules: ArcInsertRule) -> (CoreProgram, ArcInsertionPlan) {
    let (program, sema) = lower_and_typecheck(source);
    let plan = arc_insert::plan_with_rules(&program, &sema, rules);
    (program, plan)
}

pub fn call_stmt_with_arg_var(program: &CoreProgram, target: VarId) -> StmtId {
    call_stmts_with_arg_var(program, target)
        .into_iter()
        .next()
        .expect("expected call statement using target var")
}

pub fn call_stmts_with_arg_var(program: &CoreProgram, target: VarId) -> Vec<StmtId> {
    program
        .stmts()
        .iter()
        .enumerate()
        .filter_map(|(idx, _)| {
            let stmt_id = StmtId::new(idx);
            stmt_contains_call_arg_var(program, stmt_id, target).then_some(stmt_id)
        })
        .collect()
}

pub fn has_retain_op(plan: &ArcInsertionPlan, stmt: StmtId, var: VarId) -> bool {
    plan.ops
        .iter()
        .any(|op| op.stmt == stmt && matches!(op.kind, ArcOpKind::Retain { var: v } if v == var))
}

pub fn has_release_op(plan: &ArcInsertionPlan, stmt: StmtId, var: VarId) -> bool {
    plan.ops
        .iter()
        .any(|op| op.stmt == stmt && matches!(op.kind, ArcOpKind::Release { var: v } if v == var))
}

pub fn call_arg_decision(
    plan: &ArcInsertionPlan,
    stmt: StmtId,
    var: VarId,
) -> Option<ArcDecisionTraceEntry> {
    plan.decision_trace.iter().copied().find(|entry| {
        entry.stmt == stmt && matches!(entry.site, ArcDecisionSite::CallArg { var: v } if v == var)
    })
}

pub fn alias_copy_stmt_from_source(program: &CoreProgram, source: VarId) -> (StmtId, VarId) {
    program
        .stmts()
        .iter()
        .enumerate()
        .find_map(|(idx, stmt)| {
            if let StmtKind::Let { binding, value, .. } = &stmt.kind
                && let Some(ExprKind::Var(var)) = program.expr(*value).map(|expr| &expr.kind)
                && *var == source
            {
                return Some((StmtId::new(idx), *binding));
            }
            None
        })
        .expect("expected alias copy statement from source var")
}

pub fn alias_copy_decision(
    plan: &ArcInsertionPlan,
    stmt: StmtId,
    source: VarId,
    binding: VarId,
) -> Option<ArcDecisionTraceEntry> {
    plan.decision_trace.iter().copied().find(|entry| {
        entry.stmt == stmt
            && matches!(
                entry.site,
                ArcDecisionSite::AliasCopy {
                    source: trace_source,
                    binding: trace_binding,
                } if trace_source == source && trace_binding == binding
            )
    })
}

pub fn managed_ctor_binding(program: &CoreProgram) -> VarId {
    managed_ctor_binding_with_int_literal(program, 1)
}

pub fn managed_ctor_binding_with_int_literal(program: &CoreProgram, target: i64) -> VarId {
    program
        .stmts()
        .iter()
        .find_map(|stmt| {
            if let StmtKind::Let { binding, value, .. } = &stmt.kind
                && let Some(expr) = program.expr(*value)
                && let ExprKind::MakeEnum { fields, .. } | ExprKind::MakeStruct { fields, .. } =
                    &expr.kind
                && fields.iter().copied().any(|field| {
                    matches!(
                        program.expr(field).map(|expr| &expr.kind),
                        Some(ExprKind::Literal(Literal::Int(n))) if *n == target
                    )
                })
            {
                return Some(*binding);
            }
            None
        })
        .expect("managed constructor binding with literal field")
}

fn stmt_contains_call_arg_var(program: &CoreProgram, stmt_id: StmtId, target: VarId) -> bool {
    let Some(stmt) = program.stmt(stmt_id) else {
        return false;
    };
    if let StmtKind::Call { args, .. } = &stmt.kind {
        for arg in args {
            if let Some(ExprKind::Var(var)) = program.expr(*arg).map(|expr| &expr.kind)
                && *var == target
            {
                return true;
            }
        }
    }
    for expr_id in stmt.child_exprs() {
        if expr_contains_call_arg_var(program, expr_id, target) {
            return true;
        }
    }
    false
}

fn expr_contains_call_arg_var(
    program: &CoreProgram,
    expr_id: cielo::common::ids::ExprId,
    target: VarId,
) -> bool {
    let Some(expr) = program.expr(expr_id) else {
        return false;
    };
    match &expr.kind {
        ExprKind::PureCall { args, .. } => {
            for arg in args {
                if let Some(ExprKind::Var(var)) = program.expr(*arg).map(|expr| &expr.kind)
                    && *var == target
                {
                    return true;
                }
                if expr_contains_call_arg_var(program, *arg, target) {
                    return true;
                }
            }
            false
        }
        ExprKind::Unary { expr, .. } => expr_contains_call_arg_var(program, *expr, target),
        ExprKind::Binary { lhs, rhs, .. } => {
            expr_contains_call_arg_var(program, *lhs, target)
                || expr_contains_call_arg_var(program, *rhs, target)
        }
        ExprKind::MakeStruct { fields, .. } | ExprKind::MakeEnum { fields, .. } => fields
            .iter()
            .copied()
            .any(|field| expr_contains_call_arg_var(program, field, target)),
        ExprKind::Var(_) | ExprKind::Literal(_) | ExprKind::Error(_) => false,
    }
}
