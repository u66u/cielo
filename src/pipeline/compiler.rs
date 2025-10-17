use crate::common::diagnostics::DiagnosticBag;
use crate::common::ids::SourceId;
use crate::common::symbols::Interner;
use crate::frontend::parser::parse_source;
use crate::ir::core::CoreProgram;
use crate::passes::lowering::lower_program;
use crate::pipeline::phases::{
    BtaClassified, BtaTables, CoreBuilt, CtPropagated, CtPropagationTables,
    MonomorphizationSummary, Monomorphized, Parsed, ResidualTables, Residualized, Typed,
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
