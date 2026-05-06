use cielo_base::{DiagnosticBag, SymbolId};
use cielo_ir::cfg::{
    CfgArcOp, CfgArcOpKind, CfgExpr, CfgInstruction, CfgMatchArm, CfgProgram, CfgProjectionMode,
    CfgTerminator,
};
use cielo_memory::refcount::verify::verify;

fn codes(diagnostics: &DiagnosticBag) -> Vec<String> {
    diagnostics
        .entries()
        .iter()
        .map(|entry| entry.code.to_owned())
        .collect()
}

/// One `Match` block taking a field as statically unique from its scrutinee.
fn unique_take_program() -> (CfgProgram, cielo_base::CfgValueId, cielo_base::CfgBlockId) {
    let mut cfg = CfgProgram::default();
    let parent = cfg.push_value(None);
    let binder = cfg.push_value(None);
    let parent_expr = cfg.push_expr(CfgExpr::Value(parent), None);
    let binder_expr = cfg.push_expr(CfgExpr::Value(binder), None);

    let arm_target = cfg.push_block(vec![binder], None);
    cfg.set_terminator(arm_target, CfgTerminator::Return(binder_expr));
    let fallback = cfg.push_block(Vec::new(), None);
    cfg.set_terminator(fallback, CfgTerminator::Unreachable);
    let block = cfg.push_block(vec![parent], None);
    cfg.set_terminator(
        block,
        CfgTerminator::Match {
            scrutinee: parent_expr,
            arms: vec![CfgMatchArm {
                tag: SymbolId::new(1),
                binders: vec![binder],
                projections: vec![CfgProjectionMode::MoveUnique],
                target: arm_target,
            }],
            default: fallback,
        },
    );
    (cfg, parent, block)
}

#[test]
fn accepts_an_unchecked_take_from_a_parent_that_is_never_retained() {
    let (cfg, _, _) = unique_take_program();
    let mut diagnostics = DiagnosticBag::default();
    let stats = verify(&cfg, &mut diagnostics);

    assert_eq!(stats.errors, 0, "{:?}", diagnostics.entries());
    assert_eq!(stats.checked_unique_moves, 1);
}

/// A retain anywhere means somebody else holds a reference, which contradicts
/// the claim the unchecked take rests on. The planner cannot produce this
/// today; the verifier is what keeps it that way.
#[test]
fn reports_an_unchecked_take_from_a_retained_parent() {
    let (mut cfg, parent, block) = unique_take_program();
    cfg.block_mut(block)
        .expect("block")
        .terminator_arc
        .pre
        .push(CfgArcOp {
            kind: CfgArcOpKind::Retain,
            value: parent,
        });

    let mut diagnostics = DiagnosticBag::default();
    let stats = verify(&cfg, &mut diagnostics);

    assert!(stats.errors > 0);
    assert!(
        codes(&diagnostics).contains(&"CFG_ARC_VERIFY_UNIQUE_MOVE_RETAINED".to_owned()),
        "{:?}",
        codes(&diagnostics)
    );
}

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
