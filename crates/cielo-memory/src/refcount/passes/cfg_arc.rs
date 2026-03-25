//! CFG-native ownership insertion.
//!
//! The generated ABI is sink-oriented: function arguments, constructor fields,
//! block arguments, and returns consume one owned reference. A last read moves
//! that reference; a non-last read inserts a retain. Borrow-only last reads are
//! followed by a release. Match projections use `Move`/`Copy` modes, mirroring
//! Nim's `=sink` plus `wasMoved` transformation.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::analysis::cfg_liveness::{CfgLiveness, CfgUseSite};
use crate::common::gc::GcConfig;
use crate::common::ids::{CfgBlockId, CfgExprId, CfgValueId};
use crate::ir::cfg::{
    CfgArcOp, CfgArcOpKind, CfgExpr, CfgInstruction, CfgProgram, CfgProjectionMode, CfgTerminator,
};
use crate::pipeline::phases::{ArcStats, SemanticTables};
use crate::sema::ownership::OwnershipClass;

#[derive(Clone, Copy, Default)]
struct UseCount {
    borrows: u32,
    consumes: u32,
}

#[derive(Clone, Copy)]
enum UseMode {
    Borrow,
    Consume,
}

pub fn run(cfg: &mut CfgProgram, sema: &SemanticTables, gc: &GcConfig) -> ArcStats {
    clear_annotations(cfg);
    if !gc.arc_insertion_enabled() {
        return ArcStats::default();
    }

    let liveness = CfgLiveness::analyze(cfg);
    let mut managed = cfg
        .values()
        .iter()
        .map(|value| {
            value
                .source_var
                .and_then(|var| sema.ownership_of_var.get(&var).copied())
                .is_none_or(|class| class == OwnershipClass::RcManaged)
        })
        .collect::<Vec<_>>();
    infer_managed_values(cfg, &mut managed);
    let mut borrowed_binders = HashSet::new();
    let mut stats = ArcStats::default();
    let optimize_moves = gc.arc_optimization_enabled();

    plan_match_projections(
        cfg,
        &liveness,
        &managed,
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
            if is_managed(&managed, *param)
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
            match &instruction {
                CfgInstruction::Let { value, .. } => {
                    collect_expr_uses(cfg, *value, UseMode::Consume, &mut counts)
                }
                CfgInstruction::Eval { value, .. } => {
                    collect_expr_uses(cfg, *value, UseMode::Borrow, &mut counts)
                }
                _ => {}
            }
            let (pre, mut post) =
                plan_uses(&counts, &live_after, &managed, optimize_moves, &mut stats);
            if let CfgInstruction::Let { result, .. } = instruction {
                if is_managed(&managed, result) && !live_after.contains(&result) {
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
        let (pre, post) = plan_uses(&counts, &live_after, &managed, optimize_moves, &mut stats);
        let target = cfg.block_mut(block.id).expect("known CFG block");
        target.terminator_arc.pre = pre;
        target.terminator_arc.post = post;
    }

    stats
}

fn infer_managed_values(cfg: &CfgProgram, managed: &mut [bool]) {
    let mut changed = true;
    while changed {
        changed = false;
        for block in cfg.blocks() {
            for instruction in &block.instructions {
                let Some(instruction) = cfg.instruction(*instruction) else {
                    continue;
                };
                if let CfgInstruction::Let { result, value }
                | CfgInstruction::Eval { result, value } = instruction.kind
                    && expr_may_be_managed(cfg, value, managed)
                    && !managed[result.index()]
                {
                    managed[result.index()] = true;
                    changed = true;
                }
            }
            match &block.terminator {
                CfgTerminator::Goto { target, args } => {
                    if let Some(target) = cfg.block(*target) {
                        for (param, arg) in target.params.iter().zip(args) {
                            if expr_may_be_managed(cfg, *arg, managed) && !managed[param.index()] {
                                managed[param.index()] = true;
                                changed = true;
                            }
                        }
                    }
                }
                CfgTerminator::Match {
                    scrutinee, arms, ..
                } if expr_may_be_managed(cfg, *scrutinee, managed) => {
                    for binder in arms.iter().flat_map(|arm| arm.binders.iter()) {
                        if !managed[binder.index()] {
                            managed[binder.index()] = true;
                            changed = true;
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn expr_may_be_managed(cfg: &CfgProgram, expression: CfgExprId, managed: &[bool]) -> bool {
    match &cfg.expr(expression).map(|expression| &expression.kind) {
        Some(CfgExpr::Value(value)) => is_managed(managed, *value),
        Some(CfgExpr::PureCall { .. })
        | Some(CfgExpr::MakeStruct { .. })
        | Some(CfgExpr::MakeEnum { .. }) => true,
        _ => false,
    }
}

fn clear_annotations(cfg: &mut CfgProgram) {
    for instruction in 0..cfg.instructions().len() {
        cfg.instruction_mut(crate::common::ids::CfgInstId::new(instruction))
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
        let parent = direct_value(cfg, *scrutinee);
        let parent_dead = parent.is_none_or(|value| {
            !liveness
                .live_after(CfgUseSite::Terminator(block.id))
                .is_some_and(|live| live.contains(&value))
        });
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
                    CfgProjectionMode::Move
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
    match terminator {
        CfgTerminator::Return(value) => collect_expr_uses(cfg, *value, UseMode::Consume, counts),
        CfgTerminator::Goto { args, .. }
        | CfgTerminator::Call { args, .. }
        | CfgTerminator::Perform { args, .. } => {
            for arg in args {
                collect_expr_uses(cfg, *arg, UseMode::Consume, counts);
            }
        }
        CfgTerminator::Branch { cond, .. } => {
            collect_expr_uses(cfg, *cond, UseMode::Borrow, counts)
        }
        CfgTerminator::Match { scrutinee, .. } => {
            collect_expr_uses(cfg, *scrutinee, UseMode::Borrow, counts)
        }
        CfgTerminator::Unreachable => {}
    }
}

fn collect_expr_uses(
    cfg: &CfgProgram,
    expression: CfgExprId,
    mode: UseMode,
    counts: &mut HashMap<CfgValueId, UseCount>,
) {
    let Some(expression) = cfg.expr(expression) else {
        return;
    };
    match &expression.kind {
        CfgExpr::Value(value) => {
            let count = counts.entry(*value).or_default();
            match mode {
                UseMode::Borrow => count.borrows = count.borrows.saturating_add(1),
                UseMode::Consume => count.consumes = count.consumes.saturating_add(1),
            }
        }
        CfgExpr::Unary { expr, .. } => collect_expr_uses(cfg, *expr, UseMode::Borrow, counts),
        CfgExpr::Binary { lhs, rhs, .. } => {
            collect_expr_uses(cfg, *lhs, UseMode::Borrow, counts);
            collect_expr_uses(cfg, *rhs, UseMode::Borrow, counts);
        }
        CfgExpr::PureCall { args, .. }
        | CfgExpr::MakeStruct { fields: args, .. }
        | CfgExpr::MakeEnum { fields: args, .. } => {
            for arg in args {
                collect_expr_uses(cfg, *arg, UseMode::Consume, counts);
            }
        }
        CfgExpr::Literal(_) | CfgExpr::Error => {}
    }
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
            CfgExpr::PureCall { .. } | CfgExpr::MakeStruct { .. } | CfgExpr::MakeEnum { .. }
        )
    })
}

fn direct_value(cfg: &CfgProgram, expression: CfgExprId) -> Option<CfgValueId> {
    match &cfg.expr(expression)?.kind {
        CfgExpr::Value(value) => Some(*value),
        _ => None,
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
