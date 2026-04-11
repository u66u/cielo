//! Assembly of the self-contained runtime artifact.

use cielo_base::{DiagnosticBag, Span};
use cielo_ir::cfg::CfgProgram;
use cielo_ir::constants::ConstantTable;
use cielo_ir::linear::LinearProgram;
use cielo_ir::ownership::OwnershipClass;
use cielo_ir::runtime::{RuntimeProgram, RuntimeSourceMap, RuntimeValueFacts};
use cielo_sema::SemanticTables;

pub fn assemble_program(
    cfg: CfgProgram,
    linear: &LinearProgram,
    sema: &SemanticTables,
    constants: ConstantTable,
    diagnostics: DiagnosticBag,
) -> RuntimeProgram {
    let ownership = cfg
        .values()
        .iter()
        .map(|value| {
            value
                .source_var
                .and_then(|var| sema.ownership_of_var.get(&var).copied())
                .unwrap_or(OwnershipClass::Managed)
        })
        .collect();

    let block_spans = cfg
        .blocks()
        .iter()
        .map(|block| source_span(linear, block.source))
        .collect();
    let instruction_spans = cfg
        .instructions()
        .iter()
        .map(|instruction| source_span(linear, instruction.source))
        .collect();

    RuntimeProgram::new(
        cfg,
        RuntimeValueFacts::new(ownership),
        RuntimeSourceMap::new(block_spans, instruction_spans),
        constants,
        diagnostics,
    )
}

fn source_span(linear: &LinearProgram, source: Option<cielo_base::LinearStmtId>) -> Span {
    source
        .and_then(|statement| linear.stmt(statement))
        .map(|statement| statement.span)
        .unwrap_or_else(Span::synthetic)
}
