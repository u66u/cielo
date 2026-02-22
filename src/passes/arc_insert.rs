use crate::analysis::arc_cfg::ArcCfg;
use crate::analysis::arc_last_use::ArcLastUseTables;
use crate::common::ids::{StmtId, VarId};
use crate::ir::core::{CoreProgram, ExprKind, StmtKind};
use crate::pipeline::phases::SemanticTables;
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
                push_op(
                    &mut plan,
                    ArcPlannedOp {
                        stmt: stmt_id,
                        kind: ArcOpKind::Retain { var: source },
                    },
                );
            }
        }

        for var in last_use.last_uses(stmt_id) {
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
