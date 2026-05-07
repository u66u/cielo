//! Passthrough memory lowering for programs managed by external means.

use cielo_base::DiagnosticBag;
use cielo_ir::cfg::CfgProgram;

use crate::MemoryInput;
use crate::region::{self, RegionPlacementStats};

#[derive(Clone, Debug)]
pub struct UnmanagedProgram {
    pub(super) cfg: CfgProgram,
    pub(super) diagnostics: DiagnosticBag,
    pub(super) regions: RegionPlacementStats,
}

pub fn lower(input: MemoryInput<'_>) -> UnmanagedProgram {
    let mut cfg = input.runtime.cfg.clone();
    // Region placement is storage layout, not refcounting: an unmanaged build
    // still has to know which slots need an arena to free.
    let regions = region::place(&mut cfg);
    UnmanagedProgram {
        cfg,
        diagnostics: input.runtime.diagnostics.clone(),
        regions,
    }
}
