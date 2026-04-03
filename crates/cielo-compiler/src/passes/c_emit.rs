//! Final backend: ownership-annotated CFG to C.

use crate::common::gc::GcConfig;
use crate::common::symbols::Interner;
use crate::ir::cfg::CfgProgram;
use crate::ir::linear::LinearProgram;
use crate::passes::{cfg_codegen, cfg_lower, constant_table};
use crate::pipeline::phases::Residualized;
use cielo_memory::{MemoryInput, MemoryReport};

#[derive(Clone, Debug)]
pub struct EmittedC {
    pub residual: Residualized,
    pub linear: LinearProgram,
    pub cfg: CfgProgram,
    pub memory: MemoryReport,
    pub c_source: String,
}

pub fn run(
    residual: Residualized,
    linear: LinearProgram,
    cfg: CfgProgram,
    interner: &Interner,
) -> EmittedC {
    run_with_gc_config(residual, linear, cfg, interner, &GcConfig::default())
}

pub fn run_with_gc_config(
    mut residual: Residualized,
    linear: LinearProgram,
    cfg: CfgProgram,
    interner: &Interner,
    gc: &GcConfig,
) -> EmittedC {
    let managed = cielo_memory::lower(
        MemoryInput {
            cfg: &cfg,
            core: residual.program(),
            sema: residual.sema(),
            diagnostics: residual.diagnostics(),
        },
        *gc,
    );
    let cfg = managed.cfg;
    *residual.diagnostics_mut() = managed.diagnostics;
    let c_source = cfg_codegen::emit(
        &cfg,
        interner,
        &residual.residual().constant_table,
        gc.arc_emit_trace_enabled(),
    );
    EmittedC {
        residual,
        linear,
        cfg,
        memory: managed.report,
        c_source,
    }
}

/// Compatibility entrypoint for IR construction tests. Production compilation
/// always enters through `run` and therefore includes CFG ownership insertion.
pub fn emit_c_program(linear: &LinearProgram, interner: &Interner) -> String {
    let cfg = cfg_lower::run(linear);
    let constants = constant_table::build_for_linear(linear);
    cfg_codegen::emit(&cfg, interner, &constants, false)
}
