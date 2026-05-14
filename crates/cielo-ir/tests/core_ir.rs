use cielo_base::{FuncId, Span, VarId};
use cielo_ir::core::{CoreProgram, ExprKind, ExprNode, Literal, StmtKind, StmtNode};
use cielo_ir::effect::SortedEffectRow;
use cielo_ir::function_graph::remap_program_function_ids;

#[test]
fn core_program_assigns_dense_ids() {
    let mut program = CoreProgram::new();
    let expr_0 = program.push_expr(ExprNode {
        span: Span::synthetic(),
        kind: ExprKind::Literal(Literal::Int(10)),
    });
    let expr_1 = program.push_expr(ExprNode {
        span: Span::synthetic(),
        kind: ExprKind::Var(VarId::from_u32(1)),
    });
    let stmt = program.push_stmt(StmtNode {
        span: Span::synthetic(),
        kind: StmtKind::Return(expr_0),
    });

    assert_eq!(expr_0.index(), 0);
    assert_eq!(expr_1.index(), 1);
    assert_eq!(stmt.index(), 0);
}

/// CIELO-52's shape: a call site whose callee is dropped must not keep an id
/// that another function has since moved into.
#[test]
fn a_dropped_callee_leaves_no_call_site_aliasing_a_live_function() {
    let mut program = CoreProgram::new();
    let unit = program.push_expr(ExprNode {
        span: Span::synthetic(),
        kind: ExprKind::Literal(Literal::Unit),
    });
    let ret = program.push_stmt(StmtNode {
        span: Span::synthetic(),
        kind: StmtKind::Return(unit),
    });
    let call = program.push_stmt(StmtNode {
        span: Span::synthetic(),
        kind: StmtKind::Call {
            result: VarId::from_u32(0),
            callee: FuncId::new(2),
            args: Vec::new(),
            effects: SortedEffectRow::empty(),
            next: ret,
        },
    });
    let pure_call = program.push_expr(ExprNode {
        span: Span::synthetic(),
        kind: ExprKind::PureCall {
            callee: FuncId::new(2),
            args: Vec::new(),
        },
    });

    // f2 is dropped; f3 takes its dense slot.
    let remap = vec![
        Some(FuncId::new(0)),
        Some(FuncId::new(1)),
        None,
        Some(FuncId::new(2)),
    ];
    remap_program_function_ids(&mut program, &remap);

    let live_functions = 3;
    let StmtKind::Call { callee, .. } = program.stmt(call).expect("call stmt").kind else {
        panic!("expected a call statement");
    };
    assert!(
        callee.index() >= live_functions,
        "dropped callee must not resolve to a live function, got f{}",
        callee.as_u32()
    );
    let ExprKind::PureCall { callee, .. } = &program.expr(pure_call).expect("pure call").kind
    else {
        panic!("expected a pure call");
    };
    assert!(
        callee.index() >= live_functions,
        "dropped pure callee must not resolve to a live function, got f{}",
        callee.as_u32()
    );
}
