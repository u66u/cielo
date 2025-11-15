use crate::common::diagnostics::DiagnosticBag;
use crate::common::ids::SourceId;
use crate::common::symbols::Interner;
use crate::frontend::parser::parse_source;
use crate::ir::core::CoreProgram;
use crate::passes::bta;
use crate::passes::c_emit;
use crate::passes::ct_propagate;
use crate::passes::linearize;
use crate::passes::lowering::{LowerConfig, lower_program};
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

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CompilerConfig {
    pub target: TargetSpec,
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
        self.config
    }

    pub fn bootstrap_core(&self, program: CoreProgram) -> CoreBuilt {
        let _ = self.config;
        CoreBuilt::new(program, DiagnosticBag::default())
    }

    pub fn parse(&self, source: &str, source_id: SourceId, interner: &mut Interner) -> Parsed {
        let _ = self.config;
        let parsed = parse_source(source, source_id, interner);
        Parsed::new(parsed.program, parsed.diagnostics)
    }

    pub fn lower_parsed_to_core(&self, parsed: Parsed) -> CoreBuilt {
        let _ = self.config;
        let lowered = lower_program(parsed.ast(), LowerConfig::default());
        parsed.into_core_built(lowered.program, lowered.diagnostics)
    }

    pub fn lower_parsed_to_core_with_config(
        &self,
        parsed: Parsed,
        config: LowerConfig,
    ) -> CoreBuilt {
        let _ = self.config;
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
        self.lower_parsed_to_core_with_config(parsed, LowerConfig::with_entrypoint(main_symbol))
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
        let linearized = linearize::run(residual);
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
        let _ = self.config.target;
        monomorphize::run(typed)
    }

    fn ct_propagate(&self, mono: Monomorphized) -> CtPropagated {
        let _ = self.config.target;
        ct_propagate::run(mono)
    }

    fn classify_staging(&self, ct: CtPropagated) -> BtaClassified {
        let _ = self.config.target;
        bta::run(ct)
    }

    fn residualize(&self, bta: BtaClassified) -> Residualized {
        let _ = self.config.target;
        residualize::run(bta)
    }
}
