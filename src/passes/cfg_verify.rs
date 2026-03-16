use std::collections::HashSet;

use crate::analysis::cfg_liveness::{CfgLiveness, CfgUseSite};
use crate::common::diagnostics::DiagnosticBag;
use crate::common::ids::CfgValueId;
use crate::common::span::Span;
use crate::ir::cfg::{
    CfgArcOp, CfgArcOpKind, CfgExpr, CfgProgram, CfgProjectionMode, CfgTerminator,
};
use crate::pipeline::phases::SemanticTables;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CfgArcVerifyStats {
    pub checked_blocks: u32,
    pub checked_ops: u32,
    pub checked_moves: u32,
    pub errors: u32,
}

pub fn verify(
    cfg: &CfgProgram,
    _sema: &SemanticTables,
    diagnostics: &mut DiagnosticBag,
) -> CfgArcVerifyStats {
    let mut stats = CfgArcVerifyStats::default();
    if let Err(errors) = cfg.validate() {
        for error in errors {
            report(diagnostics, &mut stats, "CFG_VERIFY_INVALID_GRAPH", error);
        }
    }
    let liveness = CfgLiveness::analyze(cfg);
    for block in cfg.blocks() {
        stats.checked_blocks = stats.checked_blocks.saturating_add(1);
        verify_ops(cfg, &block.entry_arc, diagnostics, &mut stats);
        for instruction in &block.instructions {
            if let Some(instruction) = cfg.instruction(*instruction) {
                verify_ops(cfg, &instruction.arc.pre, diagnostics, &mut stats);
                verify_ops(cfg, &instruction.arc.post, diagnostics, &mut stats);
            }
        }
        verify_ops(cfg, &block.terminator_arc.pre, diagnostics, &mut stats);
        verify_ops(cfg, &block.terminator_arc.post, diagnostics, &mut stats);

        if let CfgTerminator::Match {
            scrutinee, arms, ..
        } = &block.terminator
        {
            let parent = direct_value(cfg, *scrutinee);
            let parent_live = parent.is_some_and(|parent| {
                liveness
                    .live_after(CfgUseSite::Terminator(block.id))
                    .is_some_and(|live| live.contains(&parent))
            });
            for arm in arms {
                for (binder, mode) in arm.binders.iter().zip(&arm.projections) {
                    match mode {
                        CfgProjectionMode::Move => {
                            stats.checked_moves = stats.checked_moves.saturating_add(1);
                            if parent_live {
                                report(
                                    diagnostics,
                                    &mut stats,
                                    "CFG_ARC_VERIFY_LIVE_PARENT_MOVE",
                                    format!(
                                        "field v{} moves out of a match scrutinee that remains live after b{}",
                                        binder.as_u32(),
                                        block.id.as_u32()
                                    ),
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    stats
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

fn direct_value(cfg: &CfgProgram, expression: crate::common::ids::CfgExprId) -> Option<CfgValueId> {
    match &cfg.expr(expression)?.kind {
        CfgExpr::Value(value) => Some(*value),
        _ => None,
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
