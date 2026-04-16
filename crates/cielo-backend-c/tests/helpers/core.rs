#![allow(dead_code)]

use cielo_base::Interner;
use cielo_base::SourceId;
use cielo_base::diagnostics::DiagnosticBag;
use cielo_ir::core::CoreProgram;
use cielo_ir::{cfg::CfgProgram, linear::LinearProgram};
use cielo_memory::{MemoryInput, MemoryProfile};
use cielo_sema::typecheck::typecheck_core;
use cielo_staging::pipeline::phases::SemanticTables;
use cielo_staging::pipeline::phases::StagedCore;
use cielo_test_support::{CompiledC, PassConfig, PassHarness};

pub fn lower_to_core(source: &str) -> CoreProgram {
    let compiler = PassHarness::new(PassConfig::default());
    let mut interner = Interner::new();
    let core = compiler.parse_and_lower_to_core(source, SourceId::from_u32(0), &mut interner);
    core.program().clone()
}

pub fn lower_and_typecheck(source: &str) -> (CoreProgram, SemanticTables) {
    let compiler = PassHarness::new(PassConfig::default());
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
    compile_source_to_c_with_config(source, PassConfig::default())
}

pub fn compile_source_to_c_with_config(source: &str, config: PassConfig) -> CompiledC {
    let compiler = PassHarness::new(config);
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
    mut residual: StagedCore,
    linear: LinearProgram,
    cfg: CfgProgram,
    interner: &Interner,
    memory: MemoryProfile,
) -> CompiledC {
    let runtime = cielo_runtime::assemble_program(
        cfg,
        &linear,
        residual.sema(),
        residual.facts().constant_table.clone(),
        residual.diagnostics().clone(),
    );
    let managed = cielo_memory::lower(MemoryInput { runtime: &runtime }, memory);
    *residual.diagnostics_mut() = managed.diagnostics;
    let c_source = cielo_backend_c::emit(
        &managed.cfg,
        interner,
        &runtime.constants,
        managed.emit_arc_trace_comments,
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
