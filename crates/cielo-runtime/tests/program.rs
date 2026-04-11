use cielo_base::{DiagnosticBag, Span, SymbolId, VarId};
use cielo_ir::constants::ConstantTable;
use cielo_ir::linear::{LinearExpr, LinearFunction, LinearProgram, LinearStmt};
use cielo_ir::ownership::OwnershipClass;
use cielo_runtime::{assemble_program, cfg_lower};
use cielo_sema::SemanticTables;

#[test]
fn assembly_translates_semantic_ownership_to_cfg_values() {
    let managed_var = VarId::from_u32(3);
    let trivial_var = VarId::from_u32(4);
    let span = Span::new(cielo_base::SourceId::from_u32(1), 10, 20);
    let mut linear = LinearProgram::default();
    let managed = linear.push_expr(LinearExpr::Var(managed_var));
    let return_stmt = linear.push_stmt_at(LinearStmt::Return(managed), span);
    linear.functions.push(LinearFunction {
        id: cielo_base::LinearFuncId::from_u32(0),
        name: SymbolId::from_u32(1),
        params: vec![managed_var, trivial_var],
        body: return_stmt,
    });
    linear
        .entrypoints
        .push(cielo_base::LinearFuncId::from_u32(0));

    let mut sema = SemanticTables::default();
    sema.ownership_of_var
        .insert(managed_var, OwnershipClass::Managed);
    sema.ownership_of_var
        .insert(trivial_var, OwnershipClass::Trivial);
    let cfg = cfg_lower::run(&linear);
    let runtime = assemble_program(
        cfg,
        &linear,
        &sema,
        ConstantTable::default(),
        DiagnosticBag::default(),
    );

    let managed_value = runtime
        .cfg
        .values()
        .iter()
        .find(|value| value.source_var == Some(managed_var))
        .expect("managed parameter value");
    let trivial_value = runtime
        .cfg
        .values()
        .iter()
        .find(|value| value.source_var == Some(trivial_var))
        .expect("trivial parameter value");

    assert!(runtime.values.is_managed(managed_value.id));
    assert_eq!(
        runtime.values.ownership(trivial_value.id),
        OwnershipClass::Trivial
    );
    assert!(runtime.cfg.blocks().iter().any(|block| {
        block.source == Some(return_stmt) && runtime.sources.block_span(block.id) == span
    }));
}

#[test]
fn assembly_defaults_synthetic_cfg_values_to_managed() {
    let mut linear = LinearProgram::default();
    let value = linear.push_expr(LinearExpr::Literal(cielo_ir::core::Literal::Int(1)));
    let body = linear.push_stmt(LinearStmt::Return(value));
    linear.functions.push(LinearFunction {
        id: cielo_base::LinearFuncId::from_u32(0),
        name: SymbolId::from_u32(1),
        params: Vec::new(),
        body,
    });
    linear
        .entrypoints
        .push(cielo_base::LinearFuncId::from_u32(0));

    let mut cfg = cfg_lower::run(&linear);
    let synthetic = cfg.push_value(None);
    let runtime = assemble_program(
        cfg,
        &linear,
        &SemanticTables::default(),
        ConstantTable::default(),
        DiagnosticBag::default(),
    );

    assert_eq!(runtime.values.ownership(synthetic), OwnershipClass::Managed);
}

#[test]
fn assembly_carries_runtime_constants_and_diagnostics() {
    let mut linear = LinearProgram::default();
    let value = linear.push_expr(LinearExpr::Literal(cielo_ir::core::Literal::Unit));
    let body = linear.push_stmt(LinearStmt::Return(value));
    linear.functions.push(LinearFunction {
        id: cielo_base::LinearFuncId::from_u32(0),
        name: SymbolId::from_u32(2),
        params: Vec::new(),
        body,
    });
    linear
        .entrypoints
        .push(cielo_base::LinearFuncId::from_u32(0));

    let constants = ConstantTable {
        entry_cap_bytes: 128,
        unit_cap_bytes: 512,
        total_size_bytes: 64,
        ..ConstantTable::default()
    };
    let mut diagnostics = DiagnosticBag::default();
    diagnostics.warning(
        "RUNTIME_TEST_WARNING",
        "runtime artifact diagnostic",
        Span::synthetic(),
    );

    let runtime = assemble_program(
        cfg_lower::run(&linear),
        &linear,
        &SemanticTables::default(),
        constants,
        diagnostics,
    );

    assert_eq!(runtime.constants.entry_cap_bytes, 128);
    assert_eq!(runtime.constants.unit_cap_bytes, 512);
    assert_eq!(runtime.constants.total_size_bytes, 64);
    assert_eq!(runtime.diagnostics.entries().len(), 1);
    assert_eq!(
        runtime.diagnostics.entries()[0].code,
        "RUNTIME_TEST_WARNING"
    );
}
