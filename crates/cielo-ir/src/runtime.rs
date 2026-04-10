//! Stable input contract for runtime analyses and memory strategies.

use cielo_base::{CfgBlockId, CfgInstId, CfgValueId, DiagnosticBag, Span};

use crate::cfg::CfgProgram;
use crate::constants::ConstantTable;
use crate::ownership::OwnershipClass;

#[derive(Clone, Debug, Default)]
pub struct RuntimeValueFacts {
    ownership: Vec<OwnershipClass>,
}

impl RuntimeValueFacts {
    pub fn new(ownership: Vec<OwnershipClass>) -> Self {
        Self { ownership }
    }

    pub fn ownership(&self, value: CfgValueId) -> OwnershipClass {
        self.ownership
            .get(value.index())
            .copied()
            .unwrap_or(OwnershipClass::BorrowedView)
    }

    pub fn is_managed(&self, value: CfgValueId) -> bool {
        self.ownership(value).is_managed()
    }

    pub fn len(&self) -> usize {
        self.ownership.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ownership.is_empty()
    }
}

#[derive(Clone, Debug, Default)]
pub struct RuntimeSourceMap {
    block_spans: Vec<Span>,
    instruction_spans: Vec<Span>,
}

impl RuntimeSourceMap {
    pub fn new(block_spans: Vec<Span>, instruction_spans: Vec<Span>) -> Self {
        Self {
            block_spans,
            instruction_spans,
        }
    }

    pub fn block_span(&self, block: CfgBlockId) -> Span {
        self.block_spans
            .get(block.index())
            .copied()
            .unwrap_or_else(Span::synthetic)
    }

    pub fn instruction_span(&self, instruction: CfgInstId) -> Span {
        self.instruction_spans
            .get(instruction.index())
            .copied()
            .unwrap_or_else(Span::synthetic)
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeProgram {
    pub cfg: CfgProgram,
    pub values: RuntimeValueFacts,
    pub sources: RuntimeSourceMap,
    pub constants: ConstantTable,
    pub diagnostics: DiagnosticBag,
}

impl RuntimeProgram {
    pub fn new(
        cfg: CfgProgram,
        values: RuntimeValueFacts,
        sources: RuntimeSourceMap,
        constants: ConstantTable,
        diagnostics: DiagnosticBag,
    ) -> Self {
        assert_eq!(
            cfg.values().len(),
            values.len(),
            "runtime value facts must cover every CFG value"
        );
        Self {
            cfg,
            values,
            sources,
            constants,
            diagnostics,
        }
    }
}
