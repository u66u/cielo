//! Final backend: ownership-annotated CFG to C.

use crate::common::gc::GcConfig;
use crate::common::symbols::Interner;
use crate::ir::cfg::CfgProgram;
use crate::ir::linear::LinearProgram;
use crate::passes::{cfg_arc, cfg_codegen, cfg_lower, cfg_verify, constant_table};
use crate::pipeline::phases::Residualized;

#[derive(Clone, Debug)]
pub struct EmittedC {
    pub residual: Residualized,
    pub linear: LinearProgram,
    pub cfg: CfgProgram,
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
    mut cfg: CfgProgram,
    interner: &Interner,
    gc: &GcConfig,
) -> EmittedC {
    let sema = residual.sema().clone();
    let arc_stats = cfg_arc::run(&mut cfg, &sema, gc);
    residual.residual_mut().arc_stats = arc_stats;
    if gc.arc_verify_enabled() {
        let (_, diagnostics) = residual.program_and_diagnostics_mut();
        let _ = cfg_verify::verify(&cfg, diagnostics);
    }
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
