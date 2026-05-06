//! Reference-counting analysis and lowering.

use cielo_base::DiagnosticBag;
use cielo_ir::cfg::CfgProgram;

use crate::{ArcConfig, BorrowHazardReport, MemoryInput};

pub mod analysis;
pub mod borrow_hazard;
pub mod passes;
pub mod verify;

use verify::CfgArcVerifyStats;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ArcStats {
    pub planned_retain_ops: u32,
    pub planned_release_ops: u32,
    pub eliminated_move_pairs: u32,
    pub removed_retain_ops: u32,
    pub removed_release_ops: u32,
    pub final_retain_ops: u32,
    pub final_release_ops: u32,
    /// Destructive takes whose `rc == 1 && !immortal` gate the uniqueness
    /// query discharged, so the emitted C never performs the test.
    pub static_unique_takes: u32,
}

#[derive(Clone, Debug, Default)]
pub struct ReferenceCountingReport {
    pub arc: ArcStats,
    pub verifier: Option<CfgArcVerifyStats>,
    pub borrow_hazards: BorrowHazardReport,
}

#[derive(Clone, Debug)]
pub struct ReferenceCountingProgram {
    pub(super) cfg: CfgProgram,
    pub(super) diagnostics: DiagnosticBag,
    pub(super) report: ReferenceCountingReport,
    pub(super) emit_trace_comments: bool,
}

pub fn lower(input: MemoryInput<'_>, config: ArcConfig) -> ReferenceCountingProgram {
    let mut cfg = input.runtime.cfg.clone();
    let mut diagnostics = input.runtime.diagnostics.clone();
    let managed_values = analysis::managed::classify(&cfg, &input.runtime.values);
    let borrow_hazards = borrow_hazard::analyze(&cfg, &managed_values);
    verify_hazard_report(&borrow_hazards);
    if config.borrow_hazard_diagnostics_enabled() {
        borrow_hazard::emit_diagnostics(&input.runtime.sources, &borrow_hazards, &mut diagnostics);
    }
    let arc = passes::cfg_arc::run(&mut cfg, &managed_values, &input.runtime.constants, &config);
    verify_arc_stats(arc);
    let verifier = config
        .verify_enabled()
        .then(|| verify::verify(&cfg, &mut diagnostics));

    ReferenceCountingProgram {
        cfg,
        diagnostics,
        report: ReferenceCountingReport {
            arc,
            verifier,
            borrow_hazards,
        },
        emit_trace_comments: config.emit_trace_enabled(),
    }
}

fn verify_arc_stats(stats: ArcStats) {
    assert!(
        stats.final_retain_ops <= stats.planned_retain_ops,
        "compiler bug: final retain count exceeds planned retain count"
    );
    assert!(
        stats.final_release_ops <= stats.planned_release_ops,
        "compiler bug: final release count exceeds planned release count"
    );
    assert_eq!(
        stats
            .planned_retain_ops
            .saturating_sub(stats.removed_retain_ops),
        stats.final_retain_ops,
        "compiler bug: retain accounting mismatch"
    );
    assert_eq!(
        stats
            .planned_release_ops
            .saturating_sub(stats.removed_release_ops),
        stats.final_release_ops,
        "compiler bug: release accounting mismatch"
    );
}

fn verify_hazard_report(report: &BorrowHazardReport) {
    assert_eq!(
        report.alias_fanout_count as usize,
        report.alias_fanout_sites.len()
    );
    assert_eq!(
        report.projection_count as usize,
        report.projection_sites.len()
    );
    assert_eq!(
        report.call_escape_count as usize,
        report.call_escape_sites.len()
    );
    assert_eq!(
        report.hotspots.len(),
        report.alias_fanout_sites.len()
            + report.projection_sites.len()
            + report.call_escape_sites.len()
    );
}
