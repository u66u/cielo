//! Reference-counting analysis and lowering.

use crate::{ArcConfig, BorrowHazardReport, MemoryInput, MemoryProgram, MemoryReport};

pub mod analysis;
pub mod borrow_hazard;
pub mod passes;
pub mod verify;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ArcStats {
    pub planned_retain_ops: u32,
    pub planned_release_ops: u32,
    pub eliminated_move_pairs: u32,
    pub removed_retain_ops: u32,
    pub removed_release_ops: u32,
    pub final_retain_ops: u32,
    pub final_release_ops: u32,
}

pub fn lower(input: MemoryInput<'_>, config: ArcConfig) -> MemoryProgram {
    let mut cfg = input.runtime.cfg.clone();
    let mut diagnostics = input.runtime.diagnostics.clone();
    let managed_values = analysis::managed::classify(&cfg, &input.runtime.values);
    let borrow_hazards = borrow_hazard::analyze(&cfg, &managed_values);
    verify_hazard_report(&borrow_hazards);
    if config.borrow_hazard_diagnostics_enabled() {
        borrow_hazard::emit_diagnostics(&input.runtime.sources, &borrow_hazards, &mut diagnostics);
    }
    let arc = passes::cfg_arc::run(&mut cfg, &managed_values, &config);
    verify_arc_stats(arc);
    let verifier = config
        .verify_enabled()
        .then(|| verify::verify(&cfg, &mut diagnostics));

    MemoryProgram {
        cfg,
        diagnostics,
        report: MemoryReport {
            arc,
            verifier,
            borrow_hazards,
        },
        emit_arc_trace_comments: config.emit_trace_enabled(),
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
