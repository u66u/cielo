use std::collections::{HashMap, HashSet, VecDeque};

use crate::analysis::arc_cfg::ArcCfg;
use crate::common::gc::ArcOptLevel;
use crate::common::ids::{StmtId, VarId};
use crate::ir::core::CoreProgram;

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
    optimize_internal(None, plan, ArcOptLevel::SAME_STMT_PAIR_ELIM)
}

pub fn optimize_with_cfg(program: &CoreProgram, plan: ArcInsertionPlan) -> ArcOptResult {
    optimize_internal(
        Some(program),
        plan,
        ArcOptLevel::SAME_STMT_PAIR_ELIM | ArcOptLevel::CFG_REDUNDANT_RELEASE_ELIM,
    )
}

pub fn optimize_with_level(
    program: &CoreProgram,
    plan: ArcInsertionPlan,
    level: ArcOptLevel,
) -> ArcOptResult {
    optimize_internal(Some(program), plan, level)
}

fn optimize_internal(
    program: Option<&CoreProgram>,
    mut plan: ArcInsertionPlan,
    level: ArcOptLevel,
) -> ArcOptResult {
    let mut stats = ArcOptStats::default();
    if level.contains(ArcOptLevel::SAME_STMT_PAIR_ELIM) {
        eliminate_same_stmt_pairs(&mut plan, &mut stats);
    }
    if level.contains(ArcOptLevel::CFG_REDUNDANT_RELEASE_ELIM)
        && let Some(program) = program
    {
        eliminate_cfg_redundant_releases(program, &mut plan, &mut stats);
    }
    let mut result = ArcOptResult { plan, stats };
    recompute_plan_stats(&mut result.plan);
    result
}

fn eliminate_same_stmt_pairs(plan: &mut ArcInsertionPlan, stats: &mut ArcOptStats) {
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
    stats.eliminated_move_pairs = stats
        .eliminated_move_pairs
        .saturating_add(cancelled.len() as u32);
    let mut optimized_ops = Vec::with_capacity(plan.ops.len());
    for op in plan.ops.iter().copied() {
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
    plan.ops = optimized_ops;
}

fn eliminate_cfg_redundant_releases(
    program: &CoreProgram,
    plan: &mut ArcInsertionPlan,
    stats: &mut ArcOptStats,
) {
    if plan.ops.is_empty() {
        return;
    }

    let cfg = ArcCfg::build(program);
    if cfg.reachable().is_empty() {
        return;
    }

    let mut retain_by_stmt = HashMap::<StmtId, Vec<VarId>>::new();
    let mut release_by_stmt = HashMap::<StmtId, Vec<VarId>>::new();
    for op in &plan.ops {
        match op.kind {
            ArcOpKind::Retain { var } => push_unique_site_var(&mut retain_by_stmt, op.stmt, var),
            ArcOpKind::Release { var } => push_unique_site_var(&mut release_by_stmt, op.stmt, var),
        }
    }

    let mut in_released = vec![None::<HashSet<VarId>>; cfg.stmt_capacity()];
    let mut worklist = VecDeque::new();
    for root in cfg.roots().iter().copied() {
        if root.index() >= in_released.len() {
            continue;
        }
        if merge_must_set(&mut in_released[root.index()], HashSet::new()) {
            worklist.push_back(root);
        }
    }

    while let Some(stmt_id) = worklist.pop_front() {
        if stmt_id.index() >= in_released.len() {
            continue;
        }
        let Some(summary) = cfg.summary(stmt_id) else {
            continue;
        };

        let mut out = in_released[stmt_id.index()].clone().unwrap_or_default();
        for def in &summary.defs {
            out.remove(def);
        }
        if let Some(retains) = retain_by_stmt.get(&stmt_id) {
            for var in retains {
                out.remove(var);
            }
        }
        if let Some(releases) = release_by_stmt.get(&stmt_id) {
            for var in releases {
                out.insert(*var);
            }
        }

        for succ in &summary.successors {
            if succ.index() >= in_released.len() {
                continue;
            }
            if merge_must_set(&mut in_released[succ.index()], out.clone()) {
                worklist.push_back(*succ);
            }
        }
    }

    let mut kept = Vec::with_capacity(plan.ops.len());
    for op in plan.ops.iter().copied() {
        if let ArcOpKind::Release { var } = op.kind
            && op.stmt.index() < in_released.len()
            && in_released[op.stmt.index()]
                .as_ref()
                .is_some_and(|released| released.contains(&var))
        {
            stats.removed_release_ops = stats.removed_release_ops.saturating_add(1);
            continue;
        }
        kept.push(op);
    }
    plan.ops = kept;
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

fn merge_must_set(target: &mut Option<HashSet<VarId>>, incoming: HashSet<VarId>) -> bool {
    match target {
        None => {
            *target = Some(incoming);
            true
        }
        Some(current) => {
            let merged = current
                .intersection(&incoming)
                .copied()
                .collect::<HashSet<_>>();
            if *current == merged {
                return false;
            }
            *current = merged;
            true
        }
    }
}

fn push_unique_site_var(out: &mut HashMap<StmtId, Vec<VarId>>, stmt: StmtId, var: VarId) {
    let vars = out.entry(stmt).or_default();
    if !vars.contains(&var) {
        vars.push(var);
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
