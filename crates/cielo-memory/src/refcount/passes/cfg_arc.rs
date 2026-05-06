//! CFG-native ownership insertion.
//!
//! The generated ABI is sink-oriented: function arguments, constructor fields,
//! block arguments, and returns consume one owned reference. A last read moves
//! that reference; a non-last read inserts a retain. Borrow-only last reads are
//! followed by a release. Match projections use `Move`/`Copy` modes, mirroring
//! Nim's `=sink` plus `wasMoved` transformation.

use std::collections::{BTreeSet, HashMap, HashSet};

use cielo_base::ids::{CfgBlockId, CfgExprId, CfgInstId, CfgValueId};
use cielo_ir::cfg::{
    CfgArcOp, CfgArcOpKind, CfgExpr, CfgInstruction, CfgProgram, CfgProjectionMode, CfgTerminator,
};
use cielo_ir::constants::ConstantTable;
use cielo_ir::ownership::OperandRole;
use cielo_ir::walk::walk_operands;

use crate::ArcConfig;
use crate::refcount::ArcStats;
use crate::refcount::analysis::cfg_liveness::{CfgLiveness, CfgUseSite};
use crate::refcount::analysis::uniqueness::{Uniqueness, UniquenessQuery};

#[derive(Clone, Copy, Default)]
struct UseCount {
    borrows: u32,
    consumes: u32,
}

pub fn run(
    cfg: &mut CfgProgram,
    managed: &[bool],
    constants: &ConstantTable,
    config: &ArcConfig,
) -> ArcStats {
    clear_annotations(cfg);
    if !config.insertion_enabled() {
        return ArcStats::default();
    }

    let liveness = CfgLiveness::analyze(cfg);
    // Both analyses read the CFG as `clear_annotations` left it, before any
    // edge block or planned retain exists.
    let uniqueness = UniquenessQuery::analyze(cfg, constants);
    let mut borrowed_binders = HashSet::new();
    let mut stats = ArcStats::default();
    let optimize_moves = config.optimization_enabled();

    plan_match_projections(
        cfg,
        &liveness,
        &uniqueness,
        managed,
        optimize_moves,
        &mut borrowed_binders,
        &mut stats,
    );

    let blocks = cfg.blocks().to_vec();
    for block in blocks {
        let entry_live = liveness
            .live_after_entry(block.id)
            .cloned()
            .unwrap_or_default();
        let mut entry_ops = Vec::new();
        for param in &block.params {
            if is_managed(managed, *param)
                && !entry_live.contains(param)
                && !borrowed_binders.contains(param)
            {
                entry_ops.push(release(*param));
                bump_release(&mut stats);
            }
        }
        cfg.block_mut(block.id).expect("known CFG block").entry_arc = entry_ops;

        for instruction_id in &block.instructions {
            let instruction = cfg
                .instruction(*instruction_id)
                .expect("known CFG instruction")
                .kind
                .clone();
            let site = CfgUseSite::Instruction(*instruction_id);
            let live_after = liveness.live_after(site).cloned().unwrap_or_default();
            let mut counts = HashMap::new();
            for (operand, role) in instruction.operands() {
                collect_expr_uses(cfg, operand, role, &mut counts);
            }
            let (pre, mut post) =
                plan_uses(&counts, &live_after, managed, optimize_moves, &mut stats);
            if let CfgInstruction::Let { result, .. } = instruction {
                if is_managed(managed, result) && !live_after.contains(&result) {
                    post.push(release(result));
                    bump_release(&mut stats);
                }
            } else if let CfgInstruction::Eval { result, value } = instruction
                && expr_produces_owned(cfg, value)
                && !live_after.contains(&result)
            {
                post.push(release(result));
                bump_release(&mut stats);
            }
            let node = cfg
                .instruction_mut(*instruction_id)
                .expect("known CFG instruction");
            node.arc.pre = pre;
            node.arc.post = post;
        }

        let site = CfgUseSite::Terminator(block.id);
        let live_after = liveness.live_after(site).cloned().unwrap_or_default();
        let mut counts = HashMap::new();
        collect_terminator_uses(cfg, &block.terminator, &mut counts);
        let (pre, post) = plan_uses(&counts, &live_after, managed, optimize_moves, &mut stats);
        let target = cfg.block_mut(block.id).expect("known CFG block");
        target.terminator_arc.pre = pre;
        target.terminator_arc.post = post;
    }

    plan_edge_drops(cfg, &liveness, managed, &borrowed_binders, &mut stats);

    stats
}

fn clear_annotations(cfg: &mut CfgProgram) {
    for instruction in 0..cfg.instructions().len() {
        cfg.instruction_mut(CfgInstId::new(instruction))
            .expect("known CFG instruction")
            .arc = Default::default();
    }
    for block in 0..cfg.blocks().len() {
        let block = cfg
            .block_mut(CfgBlockId::new(block))
            .expect("known CFG block");
        block.entry_arc.clear();
        block.terminator_arc = Default::default();
        if let CfgTerminator::Match { arms, .. } = &mut block.terminator {
            for arm in arms {
                arm.projections.fill(CfgProjectionMode::Borrow);
            }
        }
    }
}

fn plan_match_projections(
    cfg: &mut CfgProgram,
    liveness: &CfgLiveness,
    uniqueness: &UniquenessQuery,
    managed: &[bool],
    optimize_moves: bool,
    borrowed_binders: &mut HashSet<CfgValueId>,
    stats: &mut ArcStats,
) {
    let blocks = cfg.blocks().to_vec();
    for block in blocks {
        let CfgTerminator::Match {
            scrutinee, arms, ..
        } = &block.terminator
        else {
            continue;
        };
        let site = CfgUseSite::Terminator(block.id);
        let parent = direct_value(cfg, *scrutinee);
        let parent_dead = parent.is_none_or(|value| {
            !liveness
                .live_after(site)
                .is_some_and(|live| live.contains(&value))
        });
        // Taking one field neither retains nor releases the parent, so every
        // binder of the arm sees the same answer.
        let take_mode = match parent.map(|value| uniqueness.at(value, site)) {
            Some(Uniqueness::Unique) => CfgProjectionMode::MoveUnique,
            Some(Uniqueness::Shared | Uniqueness::Unknown) | None => CfgProjectionMode::Move,
        };
        let mut planned = arms.clone();
        for arm in &mut planned {
            let used = liveness
                .live_after_entry(arm.target)
                .cloned()
                .unwrap_or_default();
            for (idx, binder) in arm.binders.iter().copied().enumerate() {
                let mode = if !is_managed(managed, binder) || !used.contains(&binder) {
                    borrowed_binders.insert(binder);
                    CfgProjectionMode::Borrow
                } else if parent_dead && optimize_moves {
                    record_move(stats);
                    if take_mode == CfgProjectionMode::MoveUnique {
                        stats.static_unique_takes = stats.static_unique_takes.saturating_add(1);
                    }
                    take_mode
                } else {
                    stats.planned_retain_ops = stats.planned_retain_ops.saturating_add(1);
                    stats.final_retain_ops = stats.final_retain_ops.saturating_add(1);
                    CfgProjectionMode::Copy
                };
                arm.projections[idx] = mode;
            }
        }
        if let CfgTerminator::Match { arms, .. } =
            &mut cfg.block_mut(block.id).expect("known CFG block").terminator
        {
            *arms = planned;
        }
    }
}

fn collect_terminator_uses(
    cfg: &CfgProgram,
    terminator: &CfgTerminator,
    counts: &mut HashMap<CfgValueId, UseCount>,
) {
    for (operand, role) in terminator.operands() {
        collect_expr_uses(cfg, operand, role, counts);
    }
}

/// Counts occurrences, not distinct expression nodes: two parents that each own
/// the same nested value need two references, so a sub-expression reachable
/// twice must be charged twice. `borrow_hazard` deliberately counts the other
/// way.
fn collect_expr_uses(
    cfg: &CfgProgram,
    expression: CfgExprId,
    role: OperandRole,
    counts: &mut HashMap<CfgValueId, UseCount>,
) {
    walk_operands(cfg, expression, role, &mut |_, node, role| {
        let CfgExpr::Value(value) = &node.kind else {
            return;
        };
        let count = counts.entry(*value).or_default();
        match role {
            OperandRole::Read => count.borrows = count.borrows.saturating_add(1),
            OperandRole::Owned => count.consumes = count.consumes.saturating_add(1),
        }
    });
}

fn plan_uses(
    counts: &HashMap<CfgValueId, UseCount>,
    live_after: &BTreeSet<CfgValueId>,
    managed: &[bool],
    optimize_moves: bool,
    stats: &mut ArcStats,
) -> (Vec<CfgArcOp>, Vec<CfgArcOp>) {
    let mut values = counts.keys().copied().collect::<Vec<_>>();
    values.sort_by_key(|value| value.index());
    let mut pre = Vec::new();
    let mut post = Vec::new();
    for value in values {
        if !is_managed(managed, value) {
            continue;
        }
        let count = counts[&value];
        if count.consumes > 0 {
            let is_live = live_after.contains(&value);
            // Raw ARC keeps the source reference alive while every sink receives
            // a retained copy, then releases the source at its last use. The
            // optimized form sinks that source reference directly into one
            // consumer and only retains for additional consumers/live-out.
            let retains = if optimize_moves {
                count
                    .consumes
                    .saturating_add(u32::from(is_live))
                    .saturating_sub(1)
            } else {
                count.consumes
            };
            for _ in 0..retains {
                pre.push(retain(value));
                bump_retain(stats);
            }
            if !is_live && optimize_moves {
                record_move(stats);
            } else if !is_live {
                post.push(release(value));
                bump_release(stats);
            }
        } else if count.borrows > 0 && !live_after.contains(&value) {
            post.push(release(value));
            bump_release(stats);
        }
    }
    (pre, post)
}

fn expr_produces_owned(cfg: &CfgProgram, expression: CfgExprId) -> bool {
    cfg.expr(expression).is_some_and(|expression| {
        matches!(
            &expression.kind,
            CfgExpr::PureCall { .. }
                | CfgExpr::MakeStruct { .. }
                | CfgExpr::MakeEnum { .. }
                | CfgExpr::Field { .. }
        )
    })
}

fn direct_value(cfg: &CfgProgram, expression: CfgExprId) -> Option<CfgValueId> {
    match &cfg.expr(expression)?.kind {
        CfgExpr::Value(value) => Some(*value),
        _ => None,
    }
}

/// Releases values kept alive only by a sibling branch.
///
/// `live_out(B)` is the union of its successors' `live_in`, so a value used by
/// one arm and not another is live out of `B` and never released on the arm
/// that does not use it. A single-successor edge can never have a drop set for
/// the same reason, so only branching terminators need this.
///
/// Drops go on a block interposed on the edge rather than on the terminator,
/// because the terminator is shared by every outgoing edge.
fn plan_edge_drops(
    cfg: &mut CfgProgram,
    liveness: &CfgLiveness,
    managed: &[bool],
    borrowed_binders: &HashSet<CfgValueId>,
    stats: &mut ArcStats,
) {
    for block in cfg.blocks().to_vec() {
        let edges = block.terminator.successors_with_arity();
        if edges.len() < 2 {
            continue;
        }
        let live_out = liveness.live_out(block.id).cloned().unwrap_or_default();

        let mut rewrites: Vec<(CfgBlockId, CfgBlockId)> = Vec::new();
        for (target, arity) in edges {
            let target_live_in = liveness.live_in(target).cloned().unwrap_or_default();
            let drops = live_out
                .iter()
                .copied()
                .filter(|value| {
                    !target_live_in.contains(value)
                        && is_managed(managed, *value)
                        && !borrowed_binders.contains(value)
                })
                .collect::<Vec<_>>();
            if drops.is_empty() {
                continue;
            }

            let params = (0..arity).map(|_| cfg.push_value(None)).collect::<Vec<_>>();
            let args = params
                .iter()
                .map(|param| cfg.push_expr(CfgExpr::Value(*param), None))
                .collect::<Vec<_>>();
            let edge_block = cfg.push_block(params, None);
            cfg.set_terminator(edge_block, CfgTerminator::Goto { target, args });
            let edge = cfg.block_mut(edge_block).expect("fresh CFG block");
            for value in drops {
                edge.entry_arc.push(release(value));
                bump_release(stats);
            }
            rewrites.push((target, edge_block));
        }

        if rewrites.is_empty() {
            continue;
        }
        let mut terminator = block.terminator.clone();
        redirect_edges(&mut terminator, &rewrites);
        cfg.set_terminator(block.id, terminator);
    }
}

/// Each original target is redirected at most once, so an arm and the default
/// sharing a target still get their own edge block.
fn redirect_edges(terminator: &mut CfgTerminator, rewrites: &[(CfgBlockId, CfgBlockId)]) {
    let mut remaining = rewrites.to_vec();
    for target in terminator.successors_mut() {
        if let Some(index) = remaining.iter().position(|(from, _)| from == target) {
            *target = remaining.remove(index).1;
        }
    }
}

fn is_managed(managed: &[bool], value: CfgValueId) -> bool {
    managed.get(value.index()).copied().unwrap_or(true)
}

fn retain(value: CfgValueId) -> CfgArcOp {
    CfgArcOp {
        kind: CfgArcOpKind::Retain,
        value,
    }
}

fn release(value: CfgValueId) -> CfgArcOp {
    CfgArcOp {
        kind: CfgArcOpKind::Release,
        value,
    }
}

fn bump_retain(stats: &mut ArcStats) {
    stats.planned_retain_ops = stats.planned_retain_ops.saturating_add(1);
    stats.final_retain_ops = stats.final_retain_ops.saturating_add(1);
}

fn bump_release(stats: &mut ArcStats) {
    stats.planned_release_ops = stats.planned_release_ops.saturating_add(1);
    stats.final_release_ops = stats.final_release_ops.saturating_add(1);
}

fn record_move(stats: &mut ArcStats) {
    stats.planned_retain_ops = stats.planned_retain_ops.saturating_add(1);
    stats.planned_release_ops = stats.planned_release_ops.saturating_add(1);
    stats.removed_retain_ops = stats.removed_retain_ops.saturating_add(1);
    stats.removed_release_ops = stats.removed_release_ops.saturating_add(1);
    stats.eliminated_move_pairs = stats.eliminated_move_pairs.saturating_add(1);
}
