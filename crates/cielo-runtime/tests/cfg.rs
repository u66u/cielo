use cielo_base::{LinearFuncId, SymbolId, VarId};
use cielo_ir::cfg::{CfgExpr, CfgFunction, CfgInstruction, CfgProgram, CfgTerminator};
use cielo_ir::core::Literal;
use cielo_ir::linear::{LinearExpr, LinearFunction, LinearProgram, LinearStmt};
use cielo_memory::refcount::analysis::cfg_liveness::{CfgLiveness, CfgUseSite};
use cielo_runtime::cfg_lower;

fn source_value(cfg: &cielo_ir::cfg::CfgProgram, source: VarId) -> cielo_base::CfgValueId {
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

/// `cielo_ctor_field_copy` does not release its base, so a constructor left
/// inline there is allocated and never named again. ARC keys releases on
/// values, so the constructor has to become one.
#[test]
fn producer_under_a_projection_is_bound_to_its_own_value() {
    let mut linear = LinearProgram::default();
    let one = linear.push_expr(LinearExpr::Literal(Literal::Int(1)));
    let two = linear.push_expr(LinearExpr::Literal(Literal::Int(2)));
    let pair = linear.push_expr(LinearExpr::MakeStruct {
        ty: SymbolId::from_u32(7),
        fields: vec![one, two],
    });
    let projection = linear.push_expr(LinearExpr::Field {
        base: pair,
        index: 0,
    });
    let body = linear.push_stmt(LinearStmt::Return(projection));
    linear.functions.push(LinearFunction {
        id: LinearFuncId::from_u32(0),
        name: SymbolId::from_u32(0),
        params: Vec::new(),
        body,
    });

    let cfg = cfg_lower::lower_program(&linear);
    cfg.validate().expect("lowered CFG should be valid");
    let block = cfg
        .blocks()
        .iter()
        .find(|block| block.source == Some(body))
        .expect("return should have a block");
    let CfgTerminator::Return(returned) = block.terminator else {
        panic!("return should lower to a return terminator");
    };
    let CfgExpr::Field { base, .. } = cfg.expr(returned).expect("known expression").kind else {
        panic!("the projection stays in place; only its base is bound");
    };
    let CfgExpr::Value(bound) = cfg.expr(base).expect("known expression").kind else {
        panic!("the constructor under a projection must be bound to a value");
    };
    assert!(
        block.instructions.iter().any(|instruction| matches!(
            cfg.instruction(*instruction).map(|node| &node.kind),
            Some(CfgInstruction::Let { result, value })
                if *result == bound
                    && matches!(
                        cfg.expr(*value).map(|node| &node.kind),
                        Some(CfgExpr::MakeStruct { .. })
                    )
        )),
        "the binding must land in the block that reads it"
    );
}

/// A sink position already has a consumer that releases the reference, so
/// binding there would only add a temporary.
#[test]
fn producer_in_a_sink_position_stays_inline() {
    let mut linear = LinearProgram::default();
    let one = linear.push_expr(LinearExpr::Literal(Literal::Int(1)));
    let inner = linear.push_expr(LinearExpr::MakeStruct {
        ty: SymbolId::from_u32(7),
        fields: vec![one],
    });
    let outer = linear.push_expr(LinearExpr::MakeStruct {
        ty: SymbolId::from_u32(8),
        fields: vec![inner],
    });
    let body = linear.push_stmt(LinearStmt::Return(outer));
    linear.functions.push(LinearFunction {
        id: LinearFuncId::from_u32(0),
        name: SymbolId::from_u32(0),
        params: Vec::new(),
        body,
    });

    let cfg = cfg_lower::lower_program(&linear);
    cfg.validate().expect("lowered CFG should be valid");
    let block = cfg
        .blocks()
        .iter()
        .find(|block| block.source == Some(body))
        .expect("return should have a block");
    assert!(
        block.instructions.is_empty(),
        "a constructor field is a sink argument and needs no binding"
    );
}

/// The hazard in binding subexpressions: reuse the binding from a block that
/// only some paths run and the emitted C reads an uninitialised local.
#[test]
fn validate_rejects_a_value_left_undefined_on_one_path() {
    let mut cfg = CfgProgram::default();
    let bound = cfg.push_value(None);
    let cond = cfg.push_expr(CfgExpr::Literal(Literal::Bool(true)), None);
    let literal = cfg.push_expr(CfgExpr::Literal(Literal::Int(1)), None);
    let read = cfg.push_expr(CfgExpr::Value(bound), None);

    let entry = cfg.push_block(Vec::new(), None);
    let defining = cfg.push_block(Vec::new(), None);
    let join = cfg.push_block(Vec::new(), None);
    cfg.push_instruction(
        defining,
        CfgInstruction::Let {
            result: bound,
            value: literal,
        },
        None,
    );
    cfg.set_terminator(
        defining,
        CfgTerminator::Goto {
            target: join,
            args: Vec::new(),
        },
    );
    cfg.set_terminator(join, CfgTerminator::Return(read));
    cfg.functions.push(CfgFunction {
        id: cielo_base::CfgFuncId::from_u32(0),
        name: SymbolId::from_u32(0),
        params: Vec::new(),
        entry,
    });

    cfg.set_terminator(
        entry,
        CfgTerminator::Goto {
            target: defining,
            args: Vec::new(),
        },
    );
    cfg.validate()
        .expect("every path through the join defines the value");

    cfg.set_terminator(
        entry,
        CfgTerminator::Branch {
            cond,
            then_target: defining,
            else_target: join,
        },
    );
    let errors = cfg
        .validate()
        .expect_err("the else edge reaches the read without the binding");
    assert!(
        errors.iter().any(|error| error.contains("undefined")),
        "expected an undefined-read diagnostic, got {errors:?}"
    );
}
