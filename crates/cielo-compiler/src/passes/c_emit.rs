//! Final backend: ownership-annotated CFG to C.

use crate::common::gc::GcConfig;
use crate::common::symbols::Interner;
use crate::ir::cfg::CfgProgram;
use crate::ir::linear::LinearProgram;
use crate::passes::cfg_lower::CfgLowered;
use crate::passes::{cfg_arc, cfg_codegen, cfg_lower, cfg_verify, constant_table};

#[derive(Clone, Debug)]
pub struct EmittedC {
    pub linearized: crate::passes::linearize::Linearized,
    pub cfg: CfgProgram,
    pub c_source: String,
}

pub fn run(cfg_lowered: CfgLowered, interner: &Interner) -> EmittedC {
    run_with_gc_config(cfg_lowered, interner, &GcConfig::default())
}

pub fn run_with_gc_config(cfg_lowered: CfgLowered, interner: &Interner, gc: &GcConfig) -> EmittedC {
    let CfgLowered {
        mut linearized,
        mut cfg,
    } = cfg_lowered;
    let sema = linearized.residual.sema().clone();
    let arc_stats = cfg_arc::run(&mut cfg, &sema, gc);
    linearized.residual.residual_mut().arc_stats = arc_stats;
    if gc.arc_verify_enabled() {
        let (_, diagnostics) = linearized.residual.program_and_diagnostics_mut();
        let _ = cfg_verify::verify(&cfg, &sema, diagnostics);
    }
    let c_source = cfg_codegen::emit(
        &cfg,
        interner,
        &linearized.residual.residual().constant_table,
        gc.arc_emit_trace_enabled(),
    );
    EmittedC {
        linearized,
        cfg,
        c_source,
    }
}

/// Compatibility entrypoint for IR construction tests. Production compilation
/// always enters through `run` and therefore includes CFG ownership insertion.
pub fn emit_c_program(linear: &LinearProgram, interner: &Interner) -> String {
    let cfg = cfg_lower::lower_program(linear);
    let constants = constant_table::build_for_linear(linear);
    cfg_codegen::emit(&cfg, interner, &constants, false)
}
