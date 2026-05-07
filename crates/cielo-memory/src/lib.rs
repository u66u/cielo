//! Memory management over the self-contained runtime artifact.

use cielo_ir::cfg::CfgProgram;
use cielo_ir::runtime::RuntimeProgram;

pub mod config;
pub mod refcount;
pub mod region;
pub mod unmanaged;

pub use config::{ArcConfig, ArcFeatures, MemoryPreset, MemoryProfile, MemoryStrategy};
pub use refcount::analysis::uniqueness::{Uniqueness, UniquenessQuery};
pub use refcount::borrow_hazard::BorrowHazardReport;
pub use refcount::verify::CfgArcVerifyStats;
pub use refcount::{ArcStats, ReferenceCountingProgram, ReferenceCountingReport};
pub use region::RegionPlacementStats;
pub use unmanaged::UnmanagedProgram;

#[derive(Clone, Copy)]
pub struct MemoryInput<'a> {
    pub runtime: &'a RuntimeProgram,
}

#[derive(Clone, Debug)]
pub enum MemoryProgram {
    Unmanaged(UnmanagedProgram),
    ReferenceCounting(ReferenceCountingProgram),
}

#[derive(Clone, Debug)]
pub enum MemoryReport {
    Unmanaged,
    ReferenceCounting(ReferenceCountingReport),
}

impl MemoryReport {
    pub fn reference_counting(&self) -> Option<&ReferenceCountingReport> {
        match self {
            Self::Unmanaged => None,
            Self::ReferenceCounting(report) => Some(report),
        }
    }
}

#[derive(Clone, Debug)]
pub struct MemoryProgramParts {
    pub cfg: CfgProgram,
    pub diagnostics: cielo_base::DiagnosticBag,
    pub report: MemoryReport,
    pub emit_trace_comments: bool,
}

impl MemoryProgram {
    pub fn cfg(&self) -> &CfgProgram {
        match self {
            Self::Unmanaged(program) => &program.cfg,
            Self::ReferenceCounting(program) => &program.cfg,
        }
    }

    pub fn diagnostics(&self) -> &cielo_base::DiagnosticBag {
        match self {
            Self::Unmanaged(program) => &program.diagnostics,
            Self::ReferenceCounting(program) => &program.diagnostics,
        }
    }

    pub fn emit_trace_comments(&self) -> bool {
        match self {
            Self::Unmanaged(_) => false,
            Self::ReferenceCounting(program) => program.emit_trace_comments,
        }
    }

    /// Reported outside [`MemoryReport`] because both strategies compute it:
    /// region placement is storage layout, not a refcounting result.
    pub fn regions(&self) -> RegionPlacementStats {
        match self {
            Self::Unmanaged(program) => program.regions,
            Self::ReferenceCounting(program) => program.report.regions,
        }
    }

    pub fn report(&self) -> MemoryReport {
        match self {
            Self::Unmanaged(_) => MemoryReport::Unmanaged,
            Self::ReferenceCounting(program) => {
                MemoryReport::ReferenceCounting(program.report.clone())
            }
        }
    }

    pub fn into_parts(self) -> MemoryProgramParts {
        match self {
            Self::Unmanaged(program) => MemoryProgramParts {
                cfg: program.cfg,
                diagnostics: program.diagnostics,
                report: MemoryReport::Unmanaged,
                emit_trace_comments: false,
            },
            Self::ReferenceCounting(program) => MemoryProgramParts {
                cfg: program.cfg,
                diagnostics: program.diagnostics,
                report: MemoryReport::ReferenceCounting(program.report),
                emit_trace_comments: program.emit_trace_comments,
            },
        }
    }
}

pub fn lower(input: MemoryInput<'_>, profile: MemoryProfile) -> MemoryProgram {
    match profile.strategy {
        MemoryStrategy::Unmanaged => MemoryProgram::Unmanaged(unmanaged::lower(input)),
        MemoryStrategy::ReferenceCounting(config) => {
            MemoryProgram::ReferenceCounting(refcount::lower(input, config))
        }
    }
}

// A future implemented strategy gets a sibling module and a branch in
// `lower`. Its private analyses do not become a mandatory common pipeline.
