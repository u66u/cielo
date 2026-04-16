//! Direct pass harness used by compiler component tests.
//!
//! Production compilation goes through `cielo-db`. This crate deliberately
//! drives ordinary pass functions so a component test can stop at, or alter,
//! any boundary without adding test entry points to the compiler facade.

use cielo_base::{DiagnosticBag, Interner, SourceId};
use cielo_frontend::{ParseOutput, parse_source};
use cielo_ir::{cfg::CfgProgram, core::CoreProgram, linear::LinearProgram};
use cielo_lowering::{LowerConfig, LowerOutput, TargetBuiltinSymbols, lower_program};
use cielo_memory::{MemoryInput, MemoryPreset, MemoryProfile, MemoryReport};
use cielo_runtime::{assemble_program, cfg_lower, linearize};
use cielo_sema::{TypedCore, check_core};
use cielo_staging::{
    passes::{bta, comptime, ct_eval, handler_specialize, monomorphize, normalize, residualize},
    pipeline::phases::{BtaClassified, CtPropagated, StagedCore},
};

pub use cielo_ir::target::{Endianness, TargetSpec};

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct CompilerConfig {
    pub target: TargetSpec,
    pub memory: MemoryProfile,
}

impl CompilerConfig {
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
pub struct Compiler {
    config: CompilerConfig,
}

impl Compiler {
    pub fn new(config: CompilerConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> CompilerConfig {
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
        self.lower_parsed_to_core_with_config(
            parsed,
            LowerConfig::with_entrypoint(main).with_target_builtins(self.config.target, builtins),
        )
    }

    pub fn compile_source(
        &self,
        source: &str,
        source_id: SourceId,
        interner: &mut Interner,
    ) -> StagedCore {
        let core = self.parse_and_lower_to_core(source, source_id, interner);
        self.run_v1_core_pipeline(core)
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

    pub fn compile_source_v0(
        &self,
        source: &str,
        source_id: SourceId,
        interner: &mut Interner,
    ) -> StagedCore {
        let core = self.parse_and_lower_to_core(source, source_id, interner);
        self.run_v0_core_pipeline(core)
    }

    pub fn compile_source_v0_to_c(
        &self,
        source: &str,
        source_id: SourceId,
        interner: &mut Interner,
    ) -> CompiledC {
        let residual = self.compile_source_v0(source, source_id, interner);
        let residual = handler_specialize::run(residual);
        let normalized = normalize::run(residual);
        emit_runtime(normalized, interner, self.config.memory)
    }

    pub fn run_v1_evaluate_classify(&self, built: LowerOutput) -> BtaClassified {
        let mono = monomorphize::run(typecheck(built));
        comptime::evaluate_classify(mono, self.config.target)
    }

    pub fn run_v1_ct_eval(&self, built: LowerOutput) -> CtPropagated {
        let mono = monomorphize::run(typecheck(built));
        ct_eval::run(mono, self.config.target)
    }

    pub fn run_v1_residualize_specialize(&self, classified: BtaClassified) -> StagedCore {
        comptime::residualize_specialize(classified)
    }

    pub fn run_v1_normalize(&self, residual: StagedCore) -> StagedCore {
        normalize::run(residual)
    }

    pub fn run_v1_core_pipeline(&self, built: LowerOutput) -> StagedCore {
        let classified = self.run_v1_evaluate_classify(built);
        self.run_v1_residualize_specialize(classified)
    }

    pub fn run_v1_typed_pipeline(&self, typed: TypedCore) -> StagedCore {
        let mono = monomorphize::run(typed);
        let classified = comptime::evaluate_classify(mono, self.config.target);
        comptime::residualize_specialize(classified)
    }

    pub fn run_v0_core_pipeline(&self, built: LowerOutput) -> StagedCore {
        let mono = monomorphize::run(typecheck(built));
        let ct = ct_eval::run(mono, self.config.target);
        residualize::run(bta::run(ct))
    }
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
    let cfg = cfg_lower::run(linear);
    let constants = cielo_staging::passes::constant_table::build_for_linear(linear);
    cielo_backend_c::emit(&cfg, interner, &constants, false)
}
