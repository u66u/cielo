use std::collections::{HashMap, HashSet};

use cielo_base::diagnostics::DiagnosticBag;
use cielo_base::ids::CfgValueId;
use cielo_base::span::Span;
use cielo_ir::cfg::{
    CfgArcOp, CfgArcOpKind, CfgExpr, CfgProgram, CfgProjectionMode, CfgTerminator,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CfgArcVerifyStats {
    pub checked_blocks: u32,
    pub checked_ops: u32,
    pub checked_moves: u32,
    pub checked_unique_moves: u32,
    pub errors: u32,
}

pub fn verify(cfg: &CfgProgram, diagnostics: &mut DiagnosticBag) -> CfgArcVerifyStats {
    let mut stats = CfgArcVerifyStats::default();
    if let Err(errors) = cfg.validate() {
        for error in errors {
            report(diagnostics, &mut stats, "CFG_VERIFY_INVALID_GRAPH", error);
        }
    }
    verify_unique_moves(cfg, diagnostics, &mut stats);
    for block in cfg.blocks() {
        stats.checked_blocks = stats.checked_blocks.saturating_add(1);
        let mut block_releases: HashSet<CfgValueId> = HashSet::new();

        verify_ops(cfg, &block.entry_arc, diagnostics, &mut stats);
        collect_releases(&block.entry_arc, &mut block_releases, |value| {
            report(
                diagnostics,
                &mut stats,
                "CFG_ARC_VERIFY_DOUBLE_RELEASE",
                format!(
                    "v{} is released twice on the straight-line path through b{}",
                    value.as_u32(),
                    block.id.as_u32()
                ),
            );
        });

        for instruction in &block.instructions {
            if let Some(instruction) = cfg.instruction(*instruction) {
                for ops in [&instruction.arc.pre, &instruction.arc.post] {
                    verify_ops(cfg, ops, diagnostics, &mut stats);
                    collect_releases(ops, &mut block_releases, |value| {
                        report(
                            diagnostics,
                            &mut stats,
                            "CFG_ARC_VERIFY_DOUBLE_RELEASE",
                            format!(
                                "v{} is released twice on the straight-line path through b{}",
                                value.as_u32(),
                                block.id.as_u32()
                            ),
                        );
                    });
                }
            }
        }

        for ops in [&block.terminator_arc.pre, &block.terminator_arc.post] {
            verify_ops(cfg, ops, diagnostics, &mut stats);
            collect_releases(ops, &mut block_releases, |value| {
                report(
                    diagnostics,
                    &mut stats,
                    "CFG_ARC_VERIFY_DOUBLE_RELEASE",
                    format!(
                        "v{} is released twice on the straight-line path through b{}",
                        value.as_u32(),
                        block.id.as_u32()
                    ),
                );
            });
        }

        // A destructive Move needs the parent to be dead *and* unique. Only the
        // first is statically known, and asserting it here would just restate
        // the planner's own predicate over the same liveness. Uniqueness is
        // enforced at runtime in `cielo_ctor_take_field`.
        if let CfgTerminator::Match { arms, .. } = &block.terminator {
            for arm in arms {
                for mode in &arm.projections {
                    match mode {
                        CfgProjectionMode::Move => {
                            stats.checked_moves = stats.checked_moves.saturating_add(1)
                        }
                        CfgProjectionMode::MoveUnique => {
                            stats.checked_unique_moves =
                                stats.checked_unique_moves.saturating_add(1)
                        }
                        CfgProjectionMode::Borrow | CfgProjectionMode::Copy => {}
                    }
                }
            }
        }
    }
    stats
}

/// `MoveUnique` drops the runtime gate, so this checks the finished plan rather
/// than re-running the planner's predicate: a parent claimed unique must never
/// be retained anywhere, and must be scrutinised at exactly one site. Both are
/// properties of the emitted ARC ops, so a planner that reasoned from the wrong
/// liveness still trips them.
fn verify_unique_moves(
    cfg: &CfgProgram,
    diagnostics: &mut DiagnosticBag,
    stats: &mut CfgArcVerifyStats,
) {
    let mut retained = HashSet::new();
    for block in cfg.blocks() {
        let sites = block
            .entry_arc
            .iter()
            .chain(&block.terminator_arc.pre)
            .chain(&block.terminator_arc.post)
            .chain(block.instructions.iter().flat_map(|instruction| {
                cfg.instruction(*instruction)
                    .into_iter()
                    .flat_map(|node| node.arc.pre.iter().chain(&node.arc.post))
            }));
        for op in sites {
            if op.kind == CfgArcOpKind::Retain {
                retained.insert(op.value);
            }
        }
    }

    let mut scrutinised: HashMap<CfgValueId, u32> = HashMap::new();
    for block in cfg.blocks() {
        if let CfgTerminator::Match { scrutinee, .. } = &block.terminator
            && let Some(CfgExpr::Value(value)) = cfg.expr(*scrutinee).map(|node| &node.kind)
        {
            *scrutinised.entry(*value).or_default() += 1;
        }
    }

    for block in cfg.blocks() {
        let CfgTerminator::Match {
            scrutinee, arms, ..
        } = &block.terminator
        else {
            continue;
        };
        if !arms
            .iter()
            .any(|arm| arm.projections.contains(&CfgProjectionMode::MoveUnique))
        {
            continue;
        }
        let Some(CfgExpr::Value(parent)) = cfg.expr(*scrutinee).map(|node| &node.kind) else {
            report(
                diagnostics,
                stats,
                "CFG_ARC_VERIFY_UNIQUE_MOVE_TEMPORARY",
                format!(
                    "b{} takes a field as statically unique from a scrutinee that is not a value",
                    block.id.as_u32()
                ),
            );
            continue;
        };
        if retained.contains(parent) {
            report(
                diagnostics,
                stats,
                "CFG_ARC_VERIFY_UNIQUE_MOVE_RETAINED",
                format!(
                    "v{} is retained somewhere yet b{} takes a field from it as statically unique",
                    parent.as_u32(),
                    block.id.as_u32()
                ),
            );
        }
        if scrutinised.get(parent).copied().unwrap_or(0) > 1 {
            report(
                diagnostics,
                stats,
                "CFG_ARC_VERIFY_UNIQUE_MOVE_SHARED",
                format!(
                    "v{} is matched at more than one site yet b{} takes a field from it as statically unique",
                    parent.as_u32(),
                    block.id.as_u32()
                ),
            );
        }
    }
}

/// Releases are accumulated across every site in a block, so a value released
/// at two different sites on one straight-line path is reported. `verify_ops`
/// only sees one site at a time and cannot catch this.
fn collect_releases(
    ops: &[CfgArcOp],
    seen: &mut HashSet<CfgValueId>,
    mut on_duplicate: impl FnMut(CfgValueId),
) {
    for op in ops {
        if op.kind == CfgArcOpKind::Release && !seen.insert(op.value) {
            on_duplicate(op.value);
        }
    }
}

fn verify_ops(
    cfg: &CfgProgram,
    ops: &[CfgArcOp],
    diagnostics: &mut DiagnosticBag,
    stats: &mut CfgArcVerifyStats,
) {
    let mut releases = HashSet::new();
    for op in ops {
        stats.checked_ops = stats.checked_ops.saturating_add(1);
        if cfg.value(op.value).is_none() {
            report(
                diagnostics,
                stats,
                "CFG_ARC_VERIFY_INVALID_VALUE",
                format!(
                    "ARC operation references invalid value v{}",
                    op.value.as_u32()
                ),
            );
        }
        if op.kind == CfgArcOpKind::Release && !releases.insert(op.value) {
            report(
                diagnostics,
                stats,
                "CFG_ARC_VERIFY_DUP_RELEASE",
                format!(
                    "duplicate release of v{} at one CFG site",
                    op.value.as_u32()
                ),
            );
        }
    }
}

fn report(
    diagnostics: &mut DiagnosticBag,
    stats: &mut CfgArcVerifyStats,
    code: &'static str,
    message: String,
) {
    diagnostics.error(code, message, Span::synthetic());
    stats.errors = stats.errors.saturating_add(1);
}
