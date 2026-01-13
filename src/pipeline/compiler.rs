use std::path::PathBuf;

use crate::common::diagnostics::DiagnosticBag;
use crate::common::ids::SourceId;
use crate::common::symbols::Interner;
use crate::frontend::parser::parse_source;
use crate::ir::core::CoreProgram;
use crate::passes::bta;
use crate::passes::c_emit;
use crate::passes::ct_propagate;
use crate::passes::handler_specialize;
use crate::passes::linearize;
use crate::passes::lowering::{LowerConfig, TargetBuiltinSymbols, lower_program};
use crate::passes::monomorphize;
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
}

impl Default for CompilerConfig {
    fn default() -> Self {
        Self {
            target: TargetSpec::default(),
            ct_query_cache_path: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct Compiler {
    config: CompilerConfig,
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

    pub fn compile_source_v0(
        &self,
        source: &str,
        source_id: SourceId,
        interner: &mut Interner,
    ) -> Residualized {
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
        let specialized = handler_specialize::run(residual);
        let linearized = linearize::run(specialized);
        let emitted = c_emit::run(linearized, interner);
        CompiledC {
            residual: emitted.linearized.residual,
            linear: emitted.linearized.linear,
            c_source: emitted.c_source,
        }
    }

    pub fn run_v0_core_pipeline(&self, built: CoreBuilt) -> Residualized {
        let typed = self.typecheck(built);
        let mono = self.monomorphize(typed);
        let ct = self.ct_propagate(mono);
        let bta = self.classify_staging(ct);
        self.residualize(bta)
    }

    fn typecheck(&self, built: CoreBuilt) -> Typed {
        let (program, mut diagnostics) = built.into_parts();
        let sema = typecheck_core(&program, &mut diagnostics);
        Typed::new(program, diagnostics, sema)
    }

    fn monomorphize(&self, typed: Typed) -> Monomorphized {
        monomorphize::run(typed)
    }

    fn ct_propagate(&self, mono: Monomorphized) -> CtPropagated {
        ct_propagate::run_with_query_cache(
            mono,
            self.config.target,
            self.config.ct_query_cache_path.as_deref(),
        )
    }

    fn classify_staging(&self, ct: CtPropagated) -> BtaClassified {
        bta::run(ct)
    }

    fn residualize(&self, bta: BtaClassified) -> Residualized {
        residualize::run(bta)
    }
}
