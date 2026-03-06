use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::common::diagnostics::DiagnosticBag;
use crate::common::gc::{GcConfig, GcPreset};
use crate::common::ids::SourceId;
use crate::common::symbols::Interner;
use crate::frontend::parser::parse_source;
use crate::ir::core::CoreProgram;
use crate::passes::bta;
use crate::passes::c_emit;
use crate::passes::comptime;
use crate::passes::ct_eval;
use crate::passes::handler_specialize;
use crate::passes::linearize;
use crate::passes::lowering::{LowerConfig, TargetBuiltinSymbols, lower_program};
use crate::passes::monomorphize;
use crate::passes::normalize;
use crate::passes::residualize;
use crate::pipeline::phases::{
    BtaClassified, CoreBuilt, CtPropagated, Monomorphized, Parsed, Residualized, Typed,
};
use crate::sema::typecheck::typecheck_core;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Endianness {
    Little,
    Big,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TargetSpec {
    pub word_size_bits: u8,
    pub endianness: Endianness,
    pub pointer_alignment: u8,
}

impl Default for TargetSpec {
    fn default() -> Self {
        Self {
            word_size_bits: 64,
            endianness: if cfg!(target_endian = "little") {
                Endianness::Little
            } else {
                Endianness::Big
            },
            pointer_alignment: 8,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CompilerConfig {
    pub target: TargetSpec,
    pub ct_query_cache_path: Option<PathBuf>,
    pub gc: GcConfig,
}

impl Default for CompilerConfig {
    fn default() -> Self {
        Self {
            target: TargetSpec::default(),
            ct_query_cache_path: None,
            gc: GcConfig::default(),
        }
    }
}

impl CompilerConfig {
    pub fn with_gc_preset(mut self, preset: GcPreset) -> Self {
        self.gc = GcConfig::from_preset(preset);
        self
    }
}

#[derive(Debug, Default)]
pub struct Compiler {
    config: CompilerConfig,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct V0PipelineTimings {
    pub parse: Duration,
    pub lower: Duration,
    pub typecheck: Duration,
    pub monomorphize: Duration,
    pub ct_eval: Duration,
    pub bta: Duration,
    pub residualize: Duration,
}

impl V0PipelineTimings {
    pub fn total(self) -> Duration {
        self.parse
            .saturating_add(self.lower)
            .saturating_add(self.typecheck)
            .saturating_add(self.monomorphize)
            .saturating_add(self.ct_eval)
            .saturating_add(self.bta)
            .saturating_add(self.residualize)
    }

    pub fn saturating_add_assign(&mut self, other: Self) {
        self.parse = self.parse.saturating_add(other.parse);
        self.lower = self.lower.saturating_add(other.lower);
        self.typecheck = self.typecheck.saturating_add(other.typecheck);
        self.monomorphize = self.monomorphize.saturating_add(other.monomorphize);
        self.ct_eval = self.ct_eval.saturating_add(other.ct_eval);
        self.bta = self.bta.saturating_add(other.bta);
        self.residualize = self.residualize.saturating_add(other.residualize);
    }

    pub fn per_iteration(self, iterations: u32) -> Self {
        if iterations == 0 {
            return Self::default();
        }
        Self {
            parse: self.parse / iterations,
            lower: self.lower / iterations,
            typecheck: self.typecheck / iterations,
            monomorphize: self.monomorphize / iterations,
            ct_eval: self.ct_eval / iterations,
            bta: self.bta / iterations,
            residualize: self.residualize / iterations,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CompiledC {
    pub residual: Residualized,
    pub linear: crate::ir::linear::LinearProgram,
    pub c_source: String,
}

impl Compiler {
    pub fn new(config: CompilerConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> CompilerConfig {
        self.config.clone()
    }

    pub fn bootstrap_core(&self, program: CoreProgram) -> CoreBuilt {
        CoreBuilt::new(program, DiagnosticBag::default())
    }

    pub fn parse(&self, source: &str, source_id: SourceId, interner: &mut Interner) -> Parsed {
        let parsed = parse_source(source, source_id, interner);
        Parsed::new(parsed.program, parsed.diagnostics)
    }

    pub fn lower_parsed_to_core(&self, parsed: Parsed) -> CoreBuilt {
        let lowered = lower_program(parsed.ast(), LowerConfig::default());
        parsed.into_core_built(lowered.program, lowered.diagnostics)
    }

    pub fn lower_parsed_to_core_with_config(
        &self,
        parsed: Parsed,
        config: LowerConfig,
    ) -> CoreBuilt {
        let lowered = lower_program(parsed.ast(), config);
        parsed.into_core_built(lowered.program, lowered.diagnostics)
    }

    pub fn parse_and_lower_to_core(
        &self,
        source: &str,
        source_id: SourceId,
        interner: &mut Interner,
    ) -> CoreBuilt {
        let parsed = self.parse(source, source_id, interner);
        let main_symbol = interner.intern("main");
        let target_builtins = TargetBuiltinSymbols::intern(interner);
        self.lower_parsed_to_core_with_config(
            parsed,
            LowerConfig::with_entrypoint(main_symbol)
                .with_target_builtins(self.config.target, target_builtins),
        )
    }

    pub fn compile_source(
        &self,
        source: &str,
        source_id: SourceId,
        interner: &mut Interner,
    ) -> Residualized {
        self.compile_source_v1(source, source_id, interner)
    }

    pub fn compile_source_to_c(
        &self,
        source: &str,
        source_id: SourceId,
        interner: &mut Interner,
    ) -> CompiledC {
        self.compile_source_v1_to_c(source, source_id, interner)
    }

    pub fn compile_source_v1(
        &self,
        source: &str,
        source_id: SourceId,
        interner: &mut Interner,
    ) -> Residualized {
        let core = self.parse_and_lower_to_core(source, source_id, interner);
        self.run_v1_core_pipeline(core)
    }

    pub fn compile_source_v1_to_c(
        &self,
        source: &str,
        source_id: SourceId,
        interner: &mut Interner,
    ) -> CompiledC {
        let residual = self.compile_source_v1(source, source_id, interner);
        let normalized = normalize::run(residual);
        let linearized = linearize::run(normalized);
        let emitted = c_emit::run_with_gc_config(linearized, interner, &self.config.gc);
        CompiledC {
            residual: emitted.linearized.residual,
            linear: emitted.linearized.linear,
            c_source: emitted.c_source,
        }
    }

    pub fn compile_source_v0(
        &self,
        source: &str,
        source_id: SourceId,
        interner: &mut Interner,
    ) -> Residualized {
        self.compile_source_v0_profiled(source, source_id, interner)
            .0
    }

    pub fn compile_source_v0_profiled(
        &self,
        source: &str,
        source_id: SourceId,
        interner: &mut Interner,
    ) -> (Residualized, V0PipelineTimings) {
        let mut timings = V0PipelineTimings::default();

        let parse_start = Instant::now();
        let parsed = self.parse(source, source_id, interner);
        timings.parse = parse_start.elapsed();

        let main_symbol = interner.intern("main");
        let target_builtins = TargetBuiltinSymbols::intern(interner);
        let lower_start = Instant::now();
        let core = self.lower_parsed_to_core_with_config(
            parsed,
            LowerConfig::with_entrypoint(main_symbol)
                .with_target_builtins(self.config.target, target_builtins),
        );
        timings.lower = lower_start.elapsed();

        let (residual, tail) = self.run_v0_core_pipeline_profiled(core);
        timings.saturating_add_assign(tail);
        (residual, timings)
    }

    pub fn compile_source_v0_to_c(
        &self,
        source: &str,
        source_id: SourceId,
        interner: &mut Interner,
    ) -> CompiledC {
        let residual = self.compile_source_v0(source, source_id, interner);
        let specialized = handler_specialize::run_with_gc_config(residual, &self.config.gc);
        let normalized = normalize::run(specialized);
        let linearized = linearize::run(normalized);
        let emitted = c_emit::run_with_gc_config(linearized, interner, &self.config.gc);
        CompiledC {
            residual: emitted.linearized.residual,
            linear: emitted.linearized.linear,
            c_source: emitted.c_source,
        }
    }

    pub fn run_v1_evaluate_classify(&self, built: CoreBuilt) -> BtaClassified {
        let typed = self.typecheck(built);
        let mono = self.monomorphize(typed);
        self.evaluate_classify(mono)
    }

    pub fn run_v1_ct_eval(&self, built: CoreBuilt) -> CtPropagated {
        let typed = self.typecheck(built);
        let mono = self.monomorphize(typed);
        ct_eval::run_with_query_cache(
            mono,
            self.config.target,
            self.config.ct_query_cache_path.as_deref(),
        )
    }

    pub fn run_v1_residualize_specialize(&self, classified: BtaClassified) -> Residualized {
        self.residualize_specialize(classified)
    }

    pub fn run_v1_normalize(&self, residual: Residualized) -> Residualized {
        self.normalize(residual)
    }

    pub fn run_v1_core_pipeline(&self, built: CoreBuilt) -> Residualized {
        let classified = self.run_v1_evaluate_classify(built);
        self.run_v1_residualize_specialize(classified)
    }

    pub fn run_v0_core_pipeline(&self, built: CoreBuilt) -> Residualized {
        self.run_v0_core_pipeline_profiled(built).0
    }

    pub fn run_v0_core_pipeline_profiled(
        &self,
        built: CoreBuilt,
    ) -> (Residualized, V0PipelineTimings) {
        let mut timings = V0PipelineTimings::default();

        let typecheck_start = Instant::now();
        let typed = self.typecheck(built);
        timings.typecheck = typecheck_start.elapsed();

        let mono_start = Instant::now();
        let mono = self.monomorphize(typed);
        timings.monomorphize = mono_start.elapsed();

        let ct_start = Instant::now();
        let ct = self.ct_eval(mono);
        timings.ct_eval = ct_start.elapsed();

        let bta_start = Instant::now();
        let bta = self.classify_staging(ct);
        timings.bta = bta_start.elapsed();

        let residual_start = Instant::now();
        let residual = self.residualize(bta);
        timings.residualize = residual_start.elapsed();

        (residual, timings)
    }

    fn typecheck(&self, built: CoreBuilt) -> Typed {
        let (program, mut diagnostics) = built.into_parts();
        let sema = typecheck_core(&program, &mut diagnostics);
        Typed::new(program, diagnostics, sema)
    }

    fn monomorphize(&self, typed: Typed) -> Monomorphized {
        monomorphize::run(typed)
    }

    fn ct_eval(&self, mono: Monomorphized) -> CtPropagated {
        ct_eval::run_with_query_cache(
            mono,
            self.config.target,
            self.config.ct_query_cache_path.as_deref(),
        )
    }

    fn classify_staging(&self, ct: CtPropagated) -> BtaClassified {
        bta::run(ct)
    }

    fn residualize(&self, bta: BtaClassified) -> Residualized {
        residualize::run_with_gc_config(bta, &self.config.gc)
    }

    fn evaluate_classify(&self, mono: Monomorphized) -> BtaClassified {
        comptime::evaluate_classify(
            mono,
            self.config.target,
            self.config.ct_query_cache_path.as_deref(),
        )
    }

    fn residualize_specialize(&self, bta: BtaClassified) -> Residualized {
        comptime::residualize_specialize_with_gc_config(bta, &self.config.gc)
    }

    fn normalize(&self, residual: Residualized) -> Residualized {
        normalize::run(residual)
    }
}
