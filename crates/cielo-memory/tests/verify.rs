use cielo_base::DiagnosticBag;
use cielo_ir::cfg::{CfgArcOp, CfgArcOpKind, CfgExpr, CfgInstruction, CfgProgram, CfgTerminator};
use cielo_memory::refcount::verify::verify;

/// The previous check restated the planner's own liveness predicate and so
/// could never fail. These assert the replacement reports a real defect.
#[test]
fn reports_a_value_released_twice_in_one_block() {
    let mut cfg = CfgProgram::default();
    let value = cfg.push_value(None);
    let observed = cfg.push_value(None);
    let value_expr = cfg.push_expr(CfgExpr::Value(value), None);
    let block = cfg.push_block(Vec::new(), None);

    let instruction = cfg.push_instruction(
        block,
        CfgInstruction::Eval {
            result: observed,
            value: value_expr,
        },
        None,
    );
    cfg.instruction_mut(instruction)
        .expect("instruction")
        .arc
        .post
        .push(CfgArcOp {
            kind: CfgArcOpKind::Release,
            value,
        });
    cfg.set_terminator(block, CfgTerminator::Return(value_expr));
    cfg.block_mut(block)
        .expect("block")
        .terminator_arc
        .pre
        .push(CfgArcOp {
            kind: CfgArcOpKind::Release,
            value,
        });

    let mut diagnostics = DiagnosticBag::default();
    let stats = verify(&cfg, &mut diagnostics);

    assert!(stats.errors > 0, "a double release must be reported");
    assert!(
        diagnostics
            .entries()
            .iter()
            .any(|d| d.code == "CFG_ARC_VERIFY_DOUBLE_RELEASE"),
        "expected CFG_ARC_VERIFY_DOUBLE_RELEASE, got {:?}",
        diagnostics
            .entries()
            .iter()
            .map(|d| d.code.to_owned())
            .collect::<Vec<_>>()
    );
}

#[test]
fn accepts_a_single_release_per_value() {
    let mut cfg = CfgProgram::default();
    let value = cfg.push_value(None);
    let value_expr = cfg.push_expr(CfgExpr::Value(value), None);
    let block = cfg.push_block(Vec::new(), None);
    cfg.set_terminator(block, CfgTerminator::Return(value_expr));
    cfg.block_mut(block)
        .expect("block")
        .terminator_arc
        .pre
        .push(CfgArcOp {
            kind: CfgArcOpKind::Release,
            value,
        });

    let mut diagnostics = DiagnosticBag::default();
    let stats = verify(&cfg, &mut diagnostics);

    assert_eq!(stats.errors, 0, "{:?}", diagnostics.entries());
}
