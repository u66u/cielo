use std::time::{Duration, Instant};

use crate::common::diagnostics::DiagnosticBag;
use crate::common::gc::{GcConfig, GcPreset};
use crate::common::ids::SourceId;
use crate::common::symbols::Interner;
use crate::ir::core::CoreProgram;
use crate::passes::bta;
use crate::passes::c_emit;
use crate::passes::cfg_lower;
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
use cielo_db::{
    CieloDatabase, CompileProfile, SourceFile, TargetProfile, core_file, parsed_file, typed_file,
};
pub use cielo_ir::target::{Endianness, TargetSpec};

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct CompilerConfig {
    pub target: TargetSpec,
    pub gc: GcConfig,
}

impl CompilerConfig {
    pub fn with_gc_preset(mut self, preset: GcPreset) -> Self {
        self.gc = GcConfig::from_preset(preset);
        self
    }
}

#[derive(Default)]
pub struct Compiler {
    config: CompilerConfig,
    db: CieloDatabase,
}

impl std::fmt::Debug for Compiler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Compiler")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
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
    pub cfg: crate::ir::cfg::CfgProgram,
    pub memory: cielo_memory::MemoryReport,
    pub c_source: String,
}

impl Compiler {
    pub fn new(config: CompilerConfig) -> Self {
        Self {
            config,
            db: CieloDatabase::default(),
        }
    }

    pub fn config(&self) -> CompilerConfig {
        self.config.clone()
    }

    pub fn database(&self) -> &CieloDatabase {
        &self.db
    }

    fn database_profile(&self) -> CompileProfile {
        CompileProfile {
            target: TargetProfile::from(self.config.target),
            gc: self.config.gc,
        }
    }

    pub fn database_source(&self, source: &str, source_id: SourceId) -> SourceFile {
        SourceFile::new(
            &self.db,
            source_id.as_u32(),
            "<memory>".to_owned(),
            source.to_owned(),
        )
    }

    pub fn database_parse_file(&self, file: SourceFile, interner: &mut Interner) -> Parsed {
        let parsed = parsed_file(&self.db, file);
        *interner = parsed.interner.clone();
        Parsed::new(parsed.ast.clone(), parsed.diagnostics.clone())
    }

    pub fn database_lower_file(&self, file: SourceFile, interner: &mut Interner) -> CoreBuilt {
        let lowered = core_file(&self.db, file, TargetProfile::from(self.config.target));
        *interner = lowered.interner.clone();
        lowered.core.clone()
    }

    pub fn database_type_file(&self, file: SourceFile, interner: &mut Interner) -> Typed {
        let typed = typed_file(&self.db, file, TargetProfile::from(self.config.target));
        *interner = typed.interner.clone();
        typed.typed.clone()
    }

    /// Convert the cached parse artifact to the phase type used by the public
    /// compatibility API. Compiler passes still receive ordinary Rust values.
    pub fn parse_with_database(
        &self,
        source: &str,
        source_id: SourceId,
        interner: &mut Interner,
    ) -> Parsed {
        let file = self.database_source(source, source_id);
        self.database_parse_file(file, interner)
    }

    pub fn database_compile(
        &self,
        source: &str,
        source_id: SourceId,
    ) -> std::sync::Arc<cielo_db::EmittedFile> {
        let file = self.database_source(source, source_id);
        self.database_compile_file(file)
    }

    pub fn database_compile_file(
        &self,
        file: SourceFile,
    ) -> std::sync::Arc<cielo_db::EmittedFile> {
        cielo_db::compile(&self.db, file, self.database_profile())
    }

    pub fn database_memory_file(
        &self,
        file: SourceFile,
    ) -> std::sync::Arc<cielo_db::MemoryFile> {
        cielo_db::compile_memory(&self.db, file, self.database_profile())
    }

    pub fn database_staged_file(
        &self,
        file: SourceFile,
    ) -> std::sync::Arc<cielo_db::StagedFile> {
        cielo_db::staged_file(&self.db, file, TargetProfile::from(self.config.target))
    }

    pub fn database_runtime_file(
        &self,
        file: SourceFile,
    ) -> std::sync::Arc<cielo_db::RuntimeFile> {
        cielo_db::runtime_file(&self.db, file, TargetProfile::from(self.config.target))
    }

    pub fn database_emitted_file(
        &self,
        file: SourceFile,
    ) -> std::sync::Arc<cielo_db::EmittedFile> {
        cielo_db::compile(&self.db, file, self.database_profile())
    }

    pub fn bootstrap_core(&self, program: CoreProgram) -> CoreBuilt {
        CoreBuilt::new(program, DiagnosticBag::default())
    }

    pub fn parse(&self, source: &str, source_id: SourceId, interner: &mut Interner) -> Parsed {
        self.parse_with_database(source, source_id, interner)
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
        let file = self.database_source(source, source_id);
        self.database_lower_file(file, interner)
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
        let file = self.database_source(source, source_id);
        let staged = cielo_db::staged_file(
            &self.db,
            file,
            TargetProfile::from(self.config.target),
        );
        *interner = staged.interner.clone();
        staged.residual.clone()
    }

    pub fn compile_source_v1_to_c(
        &self,
        source: &str,
        source_id: SourceId,
        interner: &mut Interner,
    ) -> CompiledC {
        let file = self.database_source(source, source_id);
        let emitted = cielo_db::emitted_file(
            &self.db,
            file,
            TargetProfile::from(self.config.target),
            self.config.gc,
        );
        *interner = emitted.memory.runtime.interner.clone();
        let mut residual = emitted.memory.runtime.residual.clone();
        *residual.diagnostics_mut() = emitted.memory.memory.diagnostics.clone();
        CompiledC {
            residual,
            linear: emitted.memory.runtime.linear.clone(),
            cfg: emitted.memory.memory.cfg.clone(),
            memory: emitted.memory.memory.report.clone(),
            c_source: emitted.c_source.clone(),
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
        let specialized = handler_specialize::run(residual);
        let normalized = normalize::run(specialized);
        let (normalized, linear, cfg) = lower_runtime(normalized);
        let emitted =
            c_emit::run_with_gc_config(normalized, linear, cfg, interner, &self.config.gc);
        CompiledC {
            residual: emitted.residual,
            linear: emitted.linear,
            cfg: emitted.cfg,
            memory: emitted.memory,
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
        ct_eval::run(mono, self.config.target)
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

    pub fn run_v1_typed_pipeline(&self, typed: Typed) -> Residualized {
        let mono = self.monomorphize(typed);
        let classified = self.evaluate_classify(mono);
        self.residualize_specialize(classified)
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
        ct_eval::run(mono, self.config.target)
    }

    fn classify_staging(&self, ct: CtPropagated) -> BtaClassified {
        bta::run(ct)
    }

    fn residualize(&self, bta: BtaClassified) -> Residualized {
        residualize::run(bta)
    }

    fn evaluate_classify(&self, mono: Monomorphized) -> BtaClassified {
        comptime::evaluate_classify(mono, self.config.target)
    }

    fn residualize_specialize(&self, bta: BtaClassified) -> Residualized {
        comptime::residualize_specialize(bta)
    }

    fn normalize(&self, residual: Residualized) -> Residualized {
        normalize::run(residual)
    }
}

fn lower_runtime(
    mut residual: Residualized,
) -> (
    Residualized,
    crate::ir::linear::LinearProgram,
    crate::ir::cfg::CfgProgram,
) {
    let sema = residual.sema().clone();
    let linear = {
        let (program, diagnostics) = residual.program_and_diagnostics_mut();
        linearize::run(program, &sema, diagnostics)
    };
    let cfg = cfg_lower::run(&linear);
    (residual, linear, cfg)
}
