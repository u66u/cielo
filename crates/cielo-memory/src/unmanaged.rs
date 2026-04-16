//! Passthrough memory lowering for programs managed by external means.

use crate::{MemoryInput, MemoryProgram, MemoryReport};

pub fn lower(input: MemoryInput<'_>) -> MemoryProgram {
    MemoryProgram {
        cfg: input.runtime.cfg.clone(),
        diagnostics: input.runtime.diagnostics.clone(),
        report: MemoryReport::default(),
        emit_arc_trace_comments: false,
    }
}
