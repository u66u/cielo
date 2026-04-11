use cielo_base::SymbolId;
use cielo_ir::cfg::{CfgExpr, CfgInstruction, CfgProgram, CfgTerminator};
use cielo_ir::core::Literal;
use cielo_ir::ownership::OwnershipClass;
use cielo_ir::runtime::RuntimeValueFacts;
use cielo_memory::refcount::analysis::managed;

#[test]
fn managed_source_values_propagate_through_lets() {
    let mut cfg = CfgProgram::default();
    let source = cfg.push_value(None);
    let result = cfg.push_value(None);
    let source_expr = cfg.push_expr(CfgExpr::Value(source), None);
    let block = cfg.push_block(Vec::new(), None);
    cfg.push_instruction(
        block,
        CfgInstruction::Let {
            result,
            value: source_expr,
        },
        None,
    );
    let result_expr = cfg.push_expr(CfgExpr::Value(result), None);
    cfg.set_terminator(block, CfgTerminator::Return(result_expr));

    let facts = RuntimeValueFacts::new(vec![OwnershipClass::Managed, OwnershipClass::BorrowedView]);
    let classified = managed::classify(&cfg, &facts);

    assert!(managed::is_managed(&classified, source));
    assert!(managed::is_managed(&classified, result));
}

#[test]
fn constructor_results_are_managed() {
    let mut cfg = CfgProgram::default();
    let result = cfg.push_value(None);
    let field = cfg.push_expr(CfgExpr::Literal(Literal::Int(1)), None);
    let constructor = cfg.push_expr(
        CfgExpr::MakeEnum {
            ty: SymbolId::from_u32(1),
            variant: SymbolId::from_u32(2),
            fields: vec![field],
        },
        None,
    );
    let block = cfg.push_block(Vec::new(), None);
    cfg.push_instruction(
        block,
        CfgInstruction::Let {
            result,
            value: constructor,
        },
        None,
    );
    let result_expr = cfg.push_expr(CfgExpr::Value(result), None);
    cfg.set_terminator(block, CfgTerminator::Return(result_expr));

    let facts = RuntimeValueFacts::new(vec![OwnershipClass::BorrowedView]);
    let classified = managed::classify(&cfg, &facts);

    assert!(managed::is_managed(&classified, result));
}

#[test]
fn managed_arguments_propagate_to_block_parameters() {
    let mut cfg = CfgProgram::default();
    let source = cfg.push_value(None);
    let parameter = cfg.push_value(None);
    let source_expr = cfg.push_expr(CfgExpr::Value(source), None);
    let parameter_expr = cfg.push_expr(CfgExpr::Value(parameter), None);
    let target = cfg.push_block(vec![parameter], None);
    cfg.set_terminator(target, CfgTerminator::Return(parameter_expr));
    let entry = cfg.push_block(Vec::new(), None);
    cfg.set_terminator(
        entry,
        CfgTerminator::Goto {
            target,
            args: vec![source_expr],
        },
    );

    let facts = RuntimeValueFacts::new(vec![OwnershipClass::Managed, OwnershipClass::BorrowedView]);
    let classified = managed::classify(&cfg, &facts);

    assert!(managed::is_managed(&classified, parameter));
}
