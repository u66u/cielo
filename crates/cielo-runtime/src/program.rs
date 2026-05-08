//! Assembly of the self-contained runtime artifact.

use std::collections::HashMap;

use cielo_base::{CfgValueId, DiagnosticBag, Span};
use cielo_ir::cfg::{CfgExpr, CfgInstruction, CfgProgram};
use cielo_ir::constants::ConstantTable;
use cielo_ir::core::Literal;
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
    let synthetic = synthetic_scalars(&cfg);
    let ownership = cfg
        .values()
        .iter()
        .map(|value| {
            value
                .source_var
                .and_then(|var| sema.ownership_of_var.get(&var).copied())
                .or_else(|| synthetic.get(&value.id).copied())
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

/// Values with no source variable default to `Managed`, which is the safe
/// direction but costs a no-op release on every scalar temporary lowering
/// introduces. Narrow only where the defining expression's type is statically
/// known, and leave everything else alone.
fn synthetic_scalars(cfg: &CfgProgram) -> HashMap<CfgValueId, OwnershipClass> {
    let mut scalars = HashMap::new();
    for instruction in cfg.instructions() {
        let (CfgInstruction::Let { result, value } | CfgInstruction::Eval { result, value }) =
            instruction.kind
        else {
            continue;
        };
        let Some(expression) = cfg.expr(value) else {
            continue;
        };
        let trivial = match &expression.kind {
            // `cv_*` arithmetic and comparison helpers all yield scalars.
            CfgExpr::Unary { .. } | CfgExpr::Binary { .. } => true,
            CfgExpr::Literal(literal) => !matches!(literal, Literal::String(_)),
            CfgExpr::BuiltinCall { builtin, .. } => {
                cielo_sema::ownership::classify_core_type_ref(&builtin.return_type())
                    == OwnershipClass::Trivial
            }
            _ => false,
        };
        if trivial {
            scalars.insert(result, OwnershipClass::Trivial);
        }
    }
    scalars
}

fn source_span(linear: &LinearProgram, source: Option<cielo_base::LinearStmtId>) -> Span {
    source
        .and_then(|statement| linear.stmt(statement))
        .map(|statement| statement.span)
        .unwrap_or_else(Span::synthetic)
}
