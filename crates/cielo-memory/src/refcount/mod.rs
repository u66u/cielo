//! Reference-counting analysis and lowering.

use cielo_base::DiagnosticBag;
use cielo_ir::cfg::CfgProgram;
use cielo_ir::core::CoreProgram;
use cielo_ir::ownership::OwnershipClass;
use cielo_ir::runtime::RuntimeValueFacts;
use cielo_sema::SemanticTables;

use crate::GcConfig;

pub mod analysis;
pub mod borrow_hazard;
pub mod passes;
pub mod verify;

pub use borrow_hazard::BorrowHazardReport;
pub use verify::CfgArcVerifyStats;

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

#[derive(Clone, Copy)]
pub struct MemoryInput<'a> {
    pub cfg: &'a CfgProgram,
    pub core: &'a CoreProgram,
    pub sema: &'a SemanticTables,
    pub diagnostics: &'a DiagnosticBag,
}

#[derive(Clone, Debug, Default)]
pub struct MemoryReport {
    pub arc: ArcStats,
    pub verifier: Option<CfgArcVerifyStats>,
    pub borrow_hazards: BorrowHazardReport,
}

#[derive(Clone, Debug)]
pub struct MemoryProgram {
    pub cfg: CfgProgram,
    pub diagnostics: DiagnosticBag,
    pub report: MemoryReport,
}

pub fn lower(input: MemoryInput<'_>, config: GcConfig) -> MemoryProgram {
    let mut cfg = input.cfg.clone();
    let mut diagnostics = input.diagnostics.clone();
    let borrow_hazards = borrow_hazard::analyze(input.core, input.sema);
    verify_hazard_report(&borrow_hazards);
    if config.borrow_hazard_diagnostics_enabled() {
        borrow_hazard::emit_diagnostics(input.core, &borrow_hazards, &mut diagnostics);
    }

    let value_facts = RuntimeValueFacts::new(
        cfg.values()
            .iter()
            .map(|value| {
                value
                    .source_var
                    .and_then(|var| input.sema.ownership_of_var.get(&var).copied())
                    .unwrap_or(OwnershipClass::Managed)
            })
            .collect(),
    );
    let managed_values = analysis::managed::classify(&cfg, &value_facts);
    let arc = passes::cfg_arc::run(&mut cfg, &managed_values, &config);
    verify_arc_stats(arc);
    let verifier = config
        .arc_verify_enabled()
        .then(|| verify::verify(&cfg, &mut diagnostics));

    MemoryProgram {
        cfg,
        diagnostics,
        report: MemoryReport {
            arc,
            verifier,
            borrow_hazards,
        },
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
