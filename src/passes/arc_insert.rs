use std::collections::HashMap;

use crate::analysis::arc_cfg::ArcCfg;
use crate::analysis::arc_last_use::ArcLastUseTables;
use crate::common::ids::{ExprId, StmtId, VarId};
use crate::ir::core::{CoreProgram, ExprKind, StmtKind};
use crate::pipeline::phases::{ArcResidualOp, ArcResidualOpKind, ArcResidualPlan, SemanticTables};
use crate::sema::ownership::OwnershipClass;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArcOpKind {
    Retain { var: VarId },
    Release { var: VarId },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ArcPlannedOp {
    pub stmt: StmtId,
    pub kind: ArcOpKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ArcInsertStats {
    pub retain_ops: u32,
    pub release_ops: u32,
}

#[derive(Clone, Debug, Default)]
pub struct ArcInsertionPlan {
    pub ops: Vec<ArcPlannedOp>,
    pub stats: ArcInsertStats,
}

pub fn plan(program: &CoreProgram, sema: &SemanticTables) -> ArcInsertionPlan {
    let cfg = ArcCfg::build(program);
    let last_use = ArcLastUseTables::analyze(&cfg);
    let mut plan = ArcInsertionPlan::default();

    for stmt_id in cfg.reachable().iter().copied() {
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };

        let last_uses = last_use.last_uses(stmt_id);
        let mut moved_call_args = Vec::new();
        let mut moved_alias_sources = Vec::new();
        let mut call_arg_uses = HashMap::new();
        for expr_id in stmt.child_exprs() {
            collect_call_arg_var_counts(program, expr_id, &mut call_arg_uses);
        }
        if let StmtKind::Call { args, .. } = &stmt.kind {
            for arg in args {
                if let Some(ExprKind::Var(var)) = program.expr(*arg).map(|expr| &expr.kind) {
                    let count = call_arg_uses.entry(*var).or_insert(0);
                    *count = count.saturating_add(1);
                }
            }
        }
        for (var, use_count) in call_arg_uses {
            if ownership_of_var(sema, var) != OwnershipClass::RcManaged {
                continue;
            }
            if use_count == 1 && last_uses.contains(&var) {
                moved_call_args.push(var);
                continue;
            }
            push_op(
                &mut plan,
                ArcPlannedOp {
                    stmt: stmt_id,
                    kind: ArcOpKind::Retain { var },
                },
            );
        }

        if let StmtKind::Let {
            binding,
            value,
            next: _,
        } = &stmt.kind
            && let Some(expr) = program.expr(*value)
            && let ExprKind::Var(source) = expr.kind
        {
            let source_ownership = ownership_of_var(sema, source);
            let binding_ownership = ownership_of_var(sema, *binding);
            if source_ownership == OwnershipClass::RcManaged
                || binding_ownership == OwnershipClass::RcManaged
            {
                if source_ownership == OwnershipClass::RcManaged
                    && binding_ownership == OwnershipClass::RcManaged
                    && last_uses.contains(&source)
                {
                    moved_alias_sources.push(source);
                    continue;
                }
                push_op(
                    &mut plan,
                    ArcPlannedOp {
                        stmt: stmt_id,
                        kind: ArcOpKind::Retain { var: source },
                    },
                );
            }
        }

        for var in last_uses {
            if moved_call_args.contains(var) || moved_alias_sources.contains(var) {
                continue;
            }
            let ownership = ownership_of_var(sema, *var);
            if ownership == OwnershipClass::RcManaged {
                push_op(
                    &mut plan,
                    ArcPlannedOp {
                        stmt: stmt_id,
                        kind: ArcOpKind::Release { var: *var },
                    },
                );
            }
        }
    }

    plan
}

fn ownership_of_var(sema: &SemanticTables, var: VarId) -> OwnershipClass {
    sema.ownership_of_var
        .get(&var)
        .copied()
        .unwrap_or(OwnershipClass::BorrowedView)
}

fn collect_call_arg_var_counts(
    program: &CoreProgram,
    expr_id: ExprId,
    out: &mut HashMap<VarId, u8>,
) {
    let Some(expr) = program.expr(expr_id) else {
        return;
    };
    match &expr.kind {
        ExprKind::PureCall { args, .. } => {
            for arg in args {
                if let Some(ExprKind::Var(var)) = program.expr(*arg).map(|expr| &expr.kind) {
                    let count = out.entry(*var).or_insert(0);
                    *count = count.saturating_add(1);
                }
                collect_call_arg_var_counts(program, *arg, out);
            }
        }
        ExprKind::Unary { expr, .. } => collect_call_arg_var_counts(program, *expr, out),
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_call_arg_var_counts(program, *lhs, out);
            collect_call_arg_var_counts(program, *rhs, out);
        }
        ExprKind::MakeStruct { fields, .. } | ExprKind::MakeEnum { fields, .. } => {
            for field in fields {
                collect_call_arg_var_counts(program, *field, out);
            }
        }
        ExprKind::Var(_) | ExprKind::Literal(_) | ExprKind::Error(_) => {}
    }
}

fn push_op(plan: &mut ArcInsertionPlan, op: ArcPlannedOp) {
    if plan.ops.contains(&op) {
        return;
    }
    match op.kind {
        ArcOpKind::Retain { .. } => plan.stats.retain_ops = plan.stats.retain_ops.saturating_add(1),
        ArcOpKind::Release { .. } => {
            plan.stats.release_ops = plan.stats.release_ops.saturating_add(1)
        }
    }
    plan.ops.push(op);
    plan.ops
        .sort_by_key(|entry| (entry.stmt.index(), arc_kind_order(entry.kind)));
}

fn arc_kind_order(kind: ArcOpKind) -> u8 {
    match kind {
        ArcOpKind::Retain { .. } => 0,
        ArcOpKind::Release { .. } => 1,
    }
}

pub fn to_residual_plan(plan: &ArcInsertionPlan) -> ArcResidualPlan {
    ArcResidualPlan {
        ops: plan
            .ops
            .iter()
            .map(|op| ArcResidualOp {
                stmt: op.stmt,
                kind: match op.kind {
                    ArcOpKind::Retain { var } => ArcResidualOpKind::Retain { var },
                    ArcOpKind::Release { var } => ArcResidualOpKind::Release { var },
                },
            })
            .collect(),
    }
}
