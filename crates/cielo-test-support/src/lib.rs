//! Direct pass harness used by compiler component tests.
//!
//! Production compilation goes through `cielo-db`. This crate deliberately
//! drives ordinary pass functions so a component test can stop at, or alter,
//! any boundary without adding test entry points to the compiler facade.

use std::fs;

use cielo_base::{DiagnosticBag, Interner, SourceId};
use cielo_frontend::{ParseOutput, parse_source};
use cielo_ir::builtins::BuiltinSymbols;
use cielo_ir::{cfg::CfgProgram, core::CoreProgram, linear::LinearProgram};
use cielo_lowering::{LowerConfig, LowerOutput, TargetBuiltinSymbols, lower_program};
use cielo_memory::{MemoryInput, MemoryPreset, MemoryProfile, MemoryReport};
use cielo_runtime::{assemble_program, cfg_lower, linearize};
use cielo_sema::{TypedCore, check_core};
use cielo_staging::{
    file_deps,
    passes::{bta, comptime, ct_eval, handler_specialize, monomorphize, normalize, residualize},
    pipeline::phases::{BtaClassified, CtFileDep, CtPropagated, Monomorphized, StagedCore},
};

use cielo_ir::target::TargetSpec;

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct PassConfig {
    pub target: TargetSpec,
    pub memory: MemoryProfile,
}

impl PassConfig {
    pub fn with_memory_preset(mut self, preset: MemoryPreset) -> Self {
        self.memory = MemoryProfile::from_preset(preset);
        self
    }
}

#[derive(Clone, Debug)]
pub struct CompiledC {
    pub residual: StagedCore,
    pub linear: LinearProgram,
    pub cfg: CfgProgram,
    pub memory: MemoryReport,
    pub c_source: String,
}

#[derive(Clone, Debug, Default)]
pub struct PassHarness {
    config: PassConfig,
}

impl PassHarness {
    pub fn new(config: PassConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> PassConfig {
        self.config.clone()
    }

    pub fn bootstrap_core(&self, program: CoreProgram) -> LowerOutput {
        LowerOutput::new(program, DiagnosticBag::default())
    }

    pub fn parse(&self, source: &str, source_id: SourceId, interner: &mut Interner) -> ParseOutput {
        parse_source(source, source_id, interner)
    }

    pub fn lower_parsed_to_core(&self, parsed: ParseOutput) -> LowerOutput {
        self.lower_parsed_to_core_with_config(parsed, LowerConfig::default())
    }

    pub fn lower_parsed_to_core_with_config(
        &self,
        parsed: ParseOutput,
        config: LowerConfig,
    ) -> LowerOutput {
        let (ast, mut diagnostics) = parsed.into_parts();
        let lowered = lower_program(&ast, config);
        diagnostics.extend(lowered.diagnostics);
        LowerOutput::new(lowered.program, diagnostics)
    }

    pub fn parse_and_lower_to_core(
        &self,
        source: &str,
        source_id: SourceId,
        interner: &mut Interner,
    ) -> LowerOutput {
        let parsed = self.parse(source, source_id, interner);
        let main = interner.intern("main");
        let builtins = TargetBuiltinSymbols::intern(interner);
        let runtime_builtins = BuiltinSymbols::intern(interner);
        self.lower_parsed_to_core_with_config(
            parsed,
            LowerConfig::with_entrypoint(main)
                .with_target_builtins(self.config.target, builtins)
                .with_builtins(runtime_builtins),
        )
    }

    pub fn compile_source(
        &self,
        source: &str,
        source_id: SourceId,
        interner: &mut Interner,
    ) -> StagedCore {
        let core = self.parse_and_lower_to_core(source, source_id, interner);
        self.stage_core(core)
    }

    pub fn compile_source_to_c(
        &self,
        source: &str,
        source_id: SourceId,
        interner: &mut Interner,
    ) -> CompiledC {
        let residual = self.compile_source(source, source_id, interner);
        let normalized = normalize::run(residual);
        emit_runtime(normalized, interner, self.config.memory)
    }

    pub fn compile_source_baseline(
        &self,
        source: &str,
        source_id: SourceId,
        interner: &mut Interner,
    ) -> StagedCore {
        let core = self.parse_and_lower_to_core(source, source_id, interner);
        self.stage_core_baseline(core)
    }

    pub fn compile_source_baseline_to_c(
        &self,
        source: &str,
        source_id: SourceId,
        interner: &mut Interner,
    ) -> CompiledC {
        let residual = self.compile_source_baseline(source, source_id, interner);
        let residual = handler_specialize::run(residual);
        let normalized = normalize::run(residual);
        emit_runtime(normalized, interner, self.config.memory)
    }

    pub fn evaluate_classify(&self, built: LowerOutput) -> BtaClassified {
        let mono = monomorphize::run(typecheck(built));
        let file_deps = snapshot_file_deps(&mono);
        comptime::evaluate_classify(mono, self.config.target, file_deps)
    }

    pub fn evaluate_constants(&self, built: LowerOutput) -> CtPropagated {
        let mono = monomorphize::run(typecheck(built));
        let file_deps = snapshot_file_deps(&mono);
        ct_eval::run(mono, self.config.target, file_deps)
    }

    pub fn residualize_specialize(&self, classified: BtaClassified) -> StagedCore {
        comptime::residualize_specialize(classified)
    }

    pub fn normalize(&self, residual: StagedCore) -> StagedCore {
        normalize::run(residual)
    }

    pub fn stage_core(&self, built: LowerOutput) -> StagedCore {
        let classified = self.evaluate_classify(built);
        self.residualize_specialize(classified)
    }

    pub fn stage_typed(&self, typed: TypedCore) -> StagedCore {
        let mono = monomorphize::run(typed);
        let file_deps = snapshot_file_deps(&mono);
        let classified = comptime::evaluate_classify(mono, self.config.target, file_deps);
        comptime::residualize_specialize(classified)
    }

    pub fn stage_core_baseline(&self, built: LowerOutput) -> StagedCore {
        let mono = monomorphize::run(typecheck(built));
        let file_deps = snapshot_file_deps(&mono);
        let ct = ct_eval::run(mono, self.config.target, file_deps);
        residualize::run(bta::run(ct))
    }
}

fn snapshot_file_deps(mono: &Monomorphized) -> Vec<CtFileDep> {
    let paths = file_deps::discover(mono.program(), mono.sema());
    file_deps::snapshot(paths, |path| fs::read(path).ok())
}

fn typecheck(built: LowerOutput) -> TypedCore {
    let (program, diagnostics) = built.into_parts();
    check_core(program, diagnostics)
}

pub fn emit_runtime(
    mut residual: StagedCore,
    interner: &Interner,
    memory: MemoryProfile,
) -> CompiledC {
    let sema = residual.sema().clone();
    let linear = {
        let (program, diagnostics) = residual.program_and_diagnostics_mut();
        linearize::run(program, &sema, diagnostics)
    };
    let cfg = cfg_lower::run(&linear);
    emit_lowered(residual, linear, cfg, interner, memory)
}

pub fn emit_lowered(
    mut residual: StagedCore,
    linear: LinearProgram,
    cfg: CfgProgram,
    interner: &Interner,
    memory: MemoryProfile,
) -> CompiledC {
    let runtime = assemble_program(
        cfg,
        &linear,
        residual.sema(),
        residual.facts().constant_table.clone(),
        residual.diagnostics().clone(),
    );
    let managed = cielo_memory::lower(MemoryInput { runtime: &runtime }, memory);
    let managed = managed.into_parts();
    *residual.diagnostics_mut() = managed.diagnostics;
    let c_source = cielo_backend_c::emit(
        &managed.cfg,
        interner,
        &runtime.constants,
        managed.emit_trace_comments,
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
    let cfg = cfg_lower::run(linear);
    let constants = cielo_staging::passes::constant_table::build_for_linear(linear);
    cielo_backend_c::emit(&cfg, interner, &constants, false)
}
