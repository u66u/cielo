use crate::common::diagnostics::DiagnosticBag;
use crate::common::ids::SourceId;
use crate::common::symbols::Interner;
use crate::frontend::parser::parse_source;
use crate::ir::core::CoreProgram;
use crate::passes::lowering::lower_program;
use crate::passes::monomorphize;
use crate::pipeline::phases::{
    BtaClassified, BtaTables, CoreBuilt, CtPropagated, CtPropagationTables, Monomorphized, Parsed,
    ResidualTables, Residualized, Typed,
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

impl Compiler {
    pub fn new(config: CompilerConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> CompilerConfig {
        self.config
    }

    pub fn bootstrap_core(&self, program: CoreProgram) -> CoreBuilt {
        let _ = self.config;
        CoreBuilt {
            program,
            diagnostics: DiagnosticBag::default(),
        }
    }

    pub fn parse(&self, source: &str, source_id: SourceId, interner: &mut Interner) -> Parsed {
        let _ = self.config;
        let parsed = parse_source(source, source_id, interner);
        Parsed {
            ast: parsed.program,
            diagnostics: parsed.diagnostics,
        }
    }

    pub fn lower_parsed_to_core(&self, parsed: Parsed) -> CoreBuilt {
        let _ = self.config;
        let lowered = lower_program(&parsed.ast);
        parsed.into_core_built(lowered.program, lowered.diagnostics)
    }

    pub fn parse_and_lower_to_core(
        &self,
        source: &str,
        source_id: SourceId,
        interner: &mut Interner,
    ) -> CoreBuilt {
        let parsed = self.parse(source, source_id, interner);
        self.lower_parsed_to_core(parsed)
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

    pub fn run_v0_core_pipeline(&self, built: CoreBuilt) -> Residualized {
        let typed = self.typecheck(built);
        let mono = self.monomorphize(typed);
        let ct = self.ct_propagate(mono);
        let bta = self.classify_staging(ct);
        self.residualize(bta)
    }

    fn typecheck(&self, built: CoreBuilt) -> Typed {
        let mut diagnostics = built.diagnostics;
        let sema = typecheck_core(&built.program, &mut diagnostics);
        Typed {
            program: built.program,
            diagnostics,
            sema,
        }
    }

    fn monomorphize(&self, typed: Typed) -> Monomorphized {
        let _ = self.config.target;
        monomorphize::run(typed)
    }

    fn ct_propagate(&self, mono: Monomorphized) -> CtPropagated {
        let _ = self.config.target;
        mono.into_ct_propagated(CtPropagationTables::default())
    }

    fn classify_staging(&self, ct: CtPropagated) -> BtaClassified {
        let _ = self.config.target;
        ct.into_bta_classified(BtaTables::default())
    }

    fn residualize(&self, bta: BtaClassified) -> Residualized {
        let _ = self.config.target;
        bta.into_residualized(ResidualTables::default())
    }
}
