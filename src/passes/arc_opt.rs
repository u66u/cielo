use std::collections::HashSet;

use crate::common::ids::{StmtId, VarId};

use super::arc_insert::{ArcInsertionPlan, ArcOpKind, ArcPlannedOp};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ArcOptStats {
    pub eliminated_move_pairs: u32,
    pub removed_retain_ops: u32,
    pub removed_release_ops: u32,
}

#[derive(Clone, Debug, Default)]
pub struct ArcOptResult {
    pub plan: ArcInsertionPlan,
    pub stats: ArcOptStats,
}

pub fn optimize(plan: ArcInsertionPlan) -> ArcOptResult {
    let mut retain_sites = HashSet::new();
    let mut release_sites = HashSet::new();
    for op in &plan.ops {
        match op.kind {
            ArcOpKind::Retain { var } => {
                retain_sites.insert(ArcOpSite { stmt: op.stmt, var });
            }
            ArcOpKind::Release { var } => {
                release_sites.insert(ArcOpSite { stmt: op.stmt, var });
            }
        }
    }

    let cancelled = retain_sites
        .intersection(&release_sites)
        .copied()
        .collect::<HashSet<_>>();
    let mut optimized_ops = Vec::with_capacity(plan.ops.len());
    let mut stats = ArcOptStats {
        eliminated_move_pairs: cancelled.len() as u32,
        ..ArcOptStats::default()
    };
    for op in plan.ops {
        let site = ArcOpSite::from_planned(op);
        if cancelled.contains(&site) {
            match op.kind {
                ArcOpKind::Retain { .. } => {
                    stats.removed_retain_ops = stats.removed_retain_ops.saturating_add(1)
                }
                ArcOpKind::Release { .. } => {
                    stats.removed_release_ops = stats.removed_release_ops.saturating_add(1)
                }
            }
            continue;
        }
        optimized_ops.push(op);
    }

    let mut result = ArcOptResult {
        plan: ArcInsertionPlan {
            ops: optimized_ops,
            stats: Default::default(),
        },
        stats,
    };
    recompute_plan_stats(&mut result.plan);
    result
}

fn recompute_plan_stats(plan: &mut ArcInsertionPlan) {
    plan.stats = Default::default();
    for op in &plan.ops {
        match op.kind {
            ArcOpKind::Retain { .. } => {
                plan.stats.retain_ops = plan.stats.retain_ops.saturating_add(1);
            }
            ArcOpKind::Release { .. } => {
                plan.stats.release_ops = plan.stats.release_ops.saturating_add(1);
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct ArcOpSite {
    stmt: StmtId,
    var: VarId,
}

impl ArcOpSite {
    fn from_planned(op: ArcPlannedOp) -> Self {
        let var = match op.kind {
            ArcOpKind::Retain { var } | ArcOpKind::Release { var } => var,
        };
        Self { stmt: op.stmt, var }
    }
}
