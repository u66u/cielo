//! Memory management over the self-contained runtime artifact.

use cielo_base::DiagnosticBag;
use cielo_ir::cfg::CfgProgram;
use cielo_ir::runtime::RuntimeProgram;

pub mod config;
pub mod refcount;
pub mod unmanaged;

pub use config::{ArcConfig, ArcFeatures, MemoryPreset, MemoryProfile, MemoryStrategy};
pub use refcount::ArcStats;
pub use refcount::borrow_hazard::BorrowHazardReport;
pub use refcount::verify::CfgArcVerifyStats;

#[derive(Clone, Copy)]
pub struct MemoryInput<'a> {
    pub runtime: &'a RuntimeProgram,
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
    pub emit_arc_trace_comments: bool,
}

pub fn lower(input: MemoryInput<'_>, profile: MemoryProfile) -> MemoryProgram {
    match profile.strategy {
        MemoryStrategy::Unmanaged => unmanaged::lower(input),
        MemoryStrategy::ReferenceCounting(config) => refcount::lower(input, config),
    }
}

// A future implemented strategy gets a sibling module and a branch in
// `lower`. Its private analyses do not become a mandatory common pipeline.
