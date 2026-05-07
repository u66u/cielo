//! Passthrough memory lowering for programs managed by external means.

use cielo_base::DiagnosticBag;
use cielo_ir::cfg::CfgProgram;

use crate::MemoryInput;

#[derive(Clone, Debug)]
pub struct UnmanagedProgram {
    pub(super) cfg: CfgProgram,
    pub(super) diagnostics: DiagnosticBag,
}

pub fn lower(input: MemoryInput<'_>) -> UnmanagedProgram {
    UnmanagedProgram {
        cfg: input.runtime.cfg.clone(),
        diagnostics: input.runtime.diagnostics.clone(),
    }
}
