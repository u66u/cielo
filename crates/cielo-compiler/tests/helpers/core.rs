#![allow(dead_code)]

use cielo_base::diagnostics::DiagnosticBag;
use cielo_base::SourceId;
use cielo_base::Interner;
use cielo_ir::core::CoreProgram;
use cielo_ir::{cfg::CfgProgram, linear::LinearProgram};
use cielo_memory::{GcConfig, MemoryInput};
use cielo_staging::pipeline::phases::SemanticTables;
use cielo_staging::pipeline::phases::Residualized;
use cielo_sema::typecheck::typecheck_core;
use cielo::{CompiledC, Compiler, CompilerConfig};

pub fn lower_to_core(source: &str) -> CoreProgram {
    let compiler = Compiler::new(CompilerConfig::default());
    let mut interner = Interner::new();
    let core = compiler.parse_and_lower_to_core(source, SourceId::from_u32(0), &mut interner);
    core.program().clone()
}

pub fn lower_and_typecheck(source: &str) -> (CoreProgram, SemanticTables) {
    let compiler = Compiler::new(CompilerConfig::default());
    let mut interner = Interner::new();
    let core = compiler.parse_and_lower_to_core(source, SourceId::from_u32(0), &mut interner);
    let program = core.program().clone();
    let mut diagnostics = DiagnosticBag::default();
    let sema = typecheck_core(&program, &mut diagnostics);
    assert!(
        !diagnostics.has_errors(),
        "fixture must typecheck cleanly, got diagnostics: {:?}",
        diagnostics.entries()
    );
    (program, sema)
}

pub fn compile_source_to_c(source: &str) -> CompiledC {
    compile_source_to_c_with_config(source, CompilerConfig::default())
}

pub fn compile_source_to_c_with_config(source: &str, config: CompilerConfig) -> CompiledC {
    let compiler = Compiler::new(config);
    let mut interner = Interner::new();
    let compiled = compiler.compile_source_to_c(source, SourceId::from_u32(0), &mut interner);
    assert!(
        !compiled.residual.diagnostics().has_errors(),
        "fixture must compile cleanly, got diagnostics: {:?}",
        compiled.residual.diagnostics().entries()
    );
    compiled
}

pub fn emit_pipeline(
    mut residual: Residualized,
    linear: LinearProgram,
    cfg: CfgProgram,
    interner: &Interner,
    gc: GcConfig,
) -> CompiledC {
    let managed = cielo_memory::lower(
        MemoryInput {
            cfg: &cfg,
            core: residual.program(),
            sema: residual.sema(),
            diagnostics: residual.diagnostics(),
        },
        gc,
    );
    *residual.diagnostics_mut() = managed.diagnostics;
    let c_source = cielo_backend_c::emit(
        &managed.cfg,
        interner,
        &residual.residual().constant_table,
        gc.arc_emit_trace_enabled(),
    );
    CompiledC {
        residual,
        linear,
        cfg: managed.cfg,
        memory: managed.report,
        c_source,
    }
}

pub fn emit_c_program(linear: &LinearProgram, interner: &Interner) -> String {
    let cfg = cielo_runtime::cfg_lower::run(linear);
    let constants = cielo_staging::passes::constant_table::build_for_linear(linear);
    cielo_backend_c::emit(&cfg, interner, &constants, false)
}
