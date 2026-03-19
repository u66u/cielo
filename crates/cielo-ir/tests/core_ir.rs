use cielo_base::{Span, VarId};
use cielo_ir::core::{CoreProgram, ExprKind, ExprNode, Literal, StmtKind, StmtNode};

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
