use cielo::analysis::cfg_liveness::{CfgLiveness, CfgUseSite};
use cielo::common::ids::{LinearFuncId, SymbolId, VarId};
use cielo::ir::cfg::CfgTerminator;
use cielo::ir::core::Literal;
use cielo::ir::linear::{LinearExpr, LinearFunction, LinearProgram, LinearStmt};
use cielo::passes::cfg_lower;

fn source_value(cfg: &cielo::ir::cfg::CfgProgram, source: VarId) -> cielo::common::ids::CfgValueId {
    cfg.values()
        .iter()
        .find(|value| value.source_var == Some(source))
        .expect("source variable should have a CFG value")
        .id
}

#[test]
fn val_value_flows_into_continuation_before_next() {
    let x = VarId::from_u32(0);
    let result = VarId::from_u32(1);
    let mut linear = LinearProgram::default();
    let value_x = linear.push_expr(LinearExpr::Var(x));
    let next_x = linear.push_expr(LinearExpr::Var(x));
    let value_return = linear.push_stmt(LinearStmt::Return(value_x));
    let next_return = linear.push_stmt(LinearStmt::Return(next_x));
    let root = linear.push_stmt(LinearStmt::Val {
        binding: result,
        value: value_return,
        next: next_return,
    });
    linear.functions.push(LinearFunction {
        id: LinearFuncId::from_u32(0),
        name: SymbolId::from_u32(0),
        params: vec![x],
        body: root,
    });
    linear.entrypoints.push(LinearFuncId::from_u32(0));

    let cfg = cfg_lower::lower_program(&linear);
    cfg.validate().expect("lowered CFG should be valid");
    let liveness = CfgLiveness::analyze(&cfg);
    let x_value = source_value(&cfg, x);
    let value_block = cfg
        .blocks()
        .iter()
        .find(|block| block.source == Some(value_return))
        .expect("value return should have a block");
    let next_block = cfg
        .blocks()
        .iter()
        .find(|block| block.source == Some(next_return))
        .expect("next return should have a block");

    assert!(matches!(
        &value_block.terminator,
        CfgTerminator::Goto { .. }
    ));
    assert!(
        liveness
            .live_out(value_block.id)
            .is_some_and(|live| live.contains(&x_value)),
        "x must remain live while the Val continuation still uses it"
    );
    assert!(!liveness.is_last_use(CfgUseSite::Terminator(value_block.id), x_value));
    assert!(liveness.is_last_use(CfgUseSite::Terminator(next_block.id), x_value));
}

#[test]
fn branch_results_join_before_continuation_liveness() {
    let x = VarId::from_u32(0);
    let result = VarId::from_u32(1);
    let mut linear = LinearProgram::default();
    let cond = linear.push_expr(LinearExpr::Literal(Literal::Bool(true)));
    let one = linear.push_expr(LinearExpr::Literal(Literal::Int(1)));
    let two = linear.push_expr(LinearExpr::Literal(Literal::Int(2)));
    let next_x = linear.push_expr(LinearExpr::Var(x));
    let one_return = linear.push_stmt(LinearStmt::Return(one));
    let two_return = linear.push_stmt(LinearStmt::Return(two));
    let branch = linear.push_stmt(LinearStmt::If {
        cond,
        then_branch: one_return,
        else_branch: two_return,
    });
    let next = linear.push_stmt(LinearStmt::Return(next_x));
    let root = linear.push_stmt(LinearStmt::Val {
        binding: result,
        value: branch,
        next,
    });
    linear.functions.push(LinearFunction {
        id: LinearFuncId::from_u32(0),
        name: SymbolId::from_u32(0),
        params: vec![x],
        body: root,
    });

    let cfg = cfg_lower::lower_program(&linear);
    cfg.validate().expect("lowered CFG should be valid");
    let liveness = CfgLiveness::analyze(&cfg);
    let x_value = source_value(&cfg, x);
    let branch_block = cfg
        .blocks()
        .iter()
        .find(|block| block.source == Some(branch))
        .expect("if should have a CFG block");
    let (then_target, else_target) = match &branch_block.terminator {
        CfgTerminator::Branch {
            then_target,
            else_target,
            ..
        } => (*then_target, *else_target),
        _ => panic!("if should lower to a branch terminator"),
    };

    for target in [then_target, else_target] {
        assert!(
            liveness
                .live_out(target)
                .is_some_and(|live| live.contains(&x_value)),
            "both branch results must keep x alive through the join"
        );
    }
}
