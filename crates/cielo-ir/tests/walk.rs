use cielo_base::{CfgBlockId, CfgExprId, CfgFuncId, CfgValueId, ExprId, FuncId, Span, SymbolId};
use cielo_ir::cfg::{
    CfgExpr, CfgInstruction, CfgMatchArm, CfgProgram, CfgProjectionMode, CfgTerminator,
};
use cielo_ir::core::{
    BinaryOp, CoreProgram, ExprKind, ExprNode, Literal, StmtKind, StmtNode, UnaryOp,
};
use cielo_ir::ownership::OperandRole;
use cielo_ir::walk::{Walk, any_expr, fingerprint_exprs, walk_exprs, walk_operands};

fn push(program: &mut CoreProgram, kind: ExprKind) -> ExprId {
    program.push_expr(ExprNode {
        span: Span::synthetic(),
        kind,
    })
}

fn int(program: &mut CoreProgram, value: i64) -> ExprId {
    push(program, ExprKind::Literal(Literal::Int(value)))
}

fn value(program: &mut CfgProgram, index: u32) -> CfgExprId {
    program.push_expr(CfgExpr::Value(CfgValueId::from_u32(index)), None)
}

#[test]
fn every_core_expression_kind_lists_its_operands() {
    let mut program = CoreProgram::new();
    let leaf = int(&mut program, 1);
    let other = int(&mut program, 2);

    let cases = vec![
        (ExprKind::Var(cielo_base::VarId::from_u32(0)), 0),
        (ExprKind::Literal(Literal::Unit), 0),
        (
            ExprKind::Unary {
                op: UnaryOp::Neg,
                expr: leaf,
            },
            1,
        ),
        (
            ExprKind::Field {
                base: leaf,
                field: SymbolId::from_u32(3),
            },
            1,
        ),
        (
            ExprKind::Binary {
                op: BinaryOp::Add,
                lhs: leaf,
                rhs: other,
            },
            2,
        ),
        (
            ExprKind::PureCall {
                callee: FuncId::from_u32(0),
                args: vec![leaf, other],
            },
            2,
        ),
        (
            ExprKind::MakeStruct {
                ty: SymbolId::from_u32(1),
                fields: vec![leaf],
            },
            1,
        ),
        (
            ExprKind::MakeEnum {
                ty: SymbolId::from_u32(1),
                variant: SymbolId::from_u32(2),
                fields: vec![leaf, other],
            },
            2,
        ),
    ];

    for (kind, expected) in cases {
        assert_eq!(
            kind.child_exprs().len(),
            expected,
            "{} should list {expected} operands",
            kind.tag()
        );
        assert_eq!(kind.operands().len(), kind.child_exprs().len());
    }
}

#[test]
fn walk_reaches_operands_nested_below_a_constructor() {
    let mut program = CoreProgram::new();
    let lhs = int(&mut program, 1);
    let rhs = int(&mut program, 2);
    let sum = push(
        &mut program,
        ExprKind::Binary {
            op: BinaryOp::Add,
            lhs,
            rhs,
        },
    );
    let outer = push(
        &mut program,
        ExprKind::MakeStruct {
            ty: SymbolId::from_u32(1),
            fields: vec![sum],
        },
    );

    let mut order = Vec::new();
    walk_exprs(&program, outer, &mut |expr_id, _| {
        order.push(expr_id);
        Walk::Descend
    });

    assert_eq!(order, vec![outer, sum, lhs, rhs]);
}

#[test]
fn skip_prunes_only_the_skipped_subtree() {
    let mut program = CoreProgram::new();
    let hidden = int(&mut program, 1);
    let skipped = push(
        &mut program,
        ExprKind::Unary {
            op: UnaryOp::Neg,
            expr: hidden,
        },
    );
    let sibling = int(&mut program, 2);
    let root = push(
        &mut program,
        ExprKind::Binary {
            op: BinaryOp::Add,
            lhs: skipped,
            rhs: sibling,
        },
    );

    let mut order = Vec::new();
    let completed = walk_exprs(&program, root, &mut |expr_id, _| {
        order.push(expr_id);
        if expr_id == skipped {
            Walk::Skip
        } else {
            Walk::Descend
        }
    });

    assert!(completed);
    assert_eq!(order, vec![root, skipped, sibling]);
}

#[test]
fn any_expr_stops_at_the_first_match() {
    let mut program = CoreProgram::new();
    let target = push(&mut program, ExprKind::Var(cielo_base::VarId::from_u32(7)));
    let root = push(
        &mut program,
        ExprKind::Unary {
            op: UnaryOp::Not,
            expr: target,
        },
    );

    assert!(any_expr(&program, root, &mut |_, expr| matches!(
        &expr.kind,
        ExprKind::Var(_)
    )));
    assert!(!any_expr(&program, root, &mut |_, expr| matches!(
        &expr.kind,
        ExprKind::PureCall { .. }
    )));
}

#[test]
fn a_self_referential_expression_does_not_hang_the_walk() {
    let mut program = CoreProgram::new();
    let looping = push(
        &mut program,
        ExprKind::Unary {
            op: UnaryOp::Neg,
            expr: ExprId::from_u32(0),
        },
    );
    assert_eq!(looping, ExprId::from_u32(0));

    let mut visits = 0usize;
    walk_exprs(&program, looping, &mut |_, _| {
        visits += 1;
        Walk::Descend
    });
    assert_eq!(visits, 1);

    let mut role_visits = 0usize;
    walk_operands(&program, looping, OperandRole::Owned, &mut |_, _, _| {
        role_visits += 1;
    });
    assert_eq!(role_visits, 1);
}

/// The bug behind CIELO-7: a walker that only looked at `CfgExpr::Value` leaves
/// of the top-level node left nested owned temporaries without ARC ops.
#[test]
fn nested_constructor_fields_are_owned_operands() {
    let mut program = CfgProgram::default();
    let inner_field = value(&mut program, 1);
    let inner = program.push_expr(
        CfgExpr::MakeStruct {
            ty: SymbolId::from_u32(1),
            fields: vec![inner_field],
        },
        None,
    );
    let outer = program.push_expr(
        CfgExpr::MakeEnum {
            ty: SymbolId::from_u32(2),
            variant: SymbolId::from_u32(3),
            fields: vec![inner],
        },
        None,
    );

    let mut owned = Vec::new();
    walk_operands(&program, outer, OperandRole::Owned, &mut |_, node, role| {
        if let CfgExpr::Value(id) = &node.kind
            && role == OperandRole::Owned
        {
            owned.push(*id);
        }
    });

    assert_eq!(owned, vec![CfgValueId::from_u32(1)]);
}

#[test]
fn a_read_parent_does_not_downgrade_an_owned_operand() {
    let mut program = CfgProgram::default();
    let field = value(&mut program, 4);
    let ctor = program.push_expr(
        CfgExpr::MakeStruct {
            ty: SymbolId::from_u32(1),
            fields: vec![field],
        },
        None,
    );
    let borrowed = value(&mut program, 5);
    let root = program.push_expr(
        CfgExpr::Binary {
            op: BinaryOp::Eq,
            lhs: ctor,
            rhs: borrowed,
        },
        None,
    );

    let mut roles = Vec::new();
    walk_operands(&program, root, OperandRole::Read, &mut |_, node, role| {
        if let CfgExpr::Value(id) = &node.kind {
            roles.push((*id, role));
        }
    });

    assert_eq!(
        roles,
        vec![
            (CfgValueId::from_u32(4), OperandRole::Owned),
            (CfgValueId::from_u32(5), OperandRole::Read),
        ]
    );
}

#[test]
fn shared_operands_are_counted_once_per_parent_only_by_the_role_walk() {
    let mut program = CfgProgram::default();
    let shared = value(&mut program, 9);
    let root = program.push_expr(
        CfgExpr::MakeStruct {
            ty: SymbolId::from_u32(1),
            fields: vec![shared, shared],
        },
        None,
    );

    let mut distinct = 0usize;
    walk_exprs(&program, root, &mut |_, _| {
        distinct += 1;
        Walk::Descend
    });
    assert_eq!(distinct, 2);

    let mut occurrences = 0usize;
    walk_operands(&program, root, OperandRole::Owned, &mut |_, _, _| {
        occurrences += 1;
    });
    assert_eq!(occurrences, 3);
}

#[test]
fn terminator_operand_roles_split_sinks_from_selectors() {
    let mut program = CfgProgram::default();
    let selector = value(&mut program, 0);
    let argument = value(&mut program, 1);
    let block = CfgBlockId::from_u32(0);

    let cases = vec![
        (CfgTerminator::Return(argument), vec![OperandRole::Owned]),
        (
            CfgTerminator::Goto {
                target: block,
                args: vec![argument],
            },
            vec![OperandRole::Owned],
        ),
        (
            CfgTerminator::Branch {
                cond: selector,
                then_target: block,
                else_target: block,
            },
            vec![OperandRole::Read],
        ),
        (
            CfgTerminator::Match {
                scrutinee: selector,
                arms: Vec::new(),
                default: block,
            },
            vec![OperandRole::Read],
        ),
        (
            CfgTerminator::Switch {
                selector,
                targets: vec![block],
                default: block,
            },
            vec![OperandRole::Read],
        ),
        (
            CfgTerminator::Call {
                convention: cielo_ir::cfg::CfgCallConvention::Direct,
                callee: SymbolId::from_u32(0),
                callee_fn: CfgFuncId::from_u32(0),
                args: vec![argument, selector],
                result: CfgValueId::from_u32(2),
                target: block,
            },
            vec![OperandRole::Owned, OperandRole::Owned],
        ),
        (CfgTerminator::Unreachable, Vec::new()),
    ];

    for (terminator, expected) in cases {
        let roles = terminator
            .operands()
            .into_iter()
            .map(|(_, role)| role)
            .collect::<Vec<_>>();
        assert_eq!(roles, expected);
        assert_eq!(terminator.child_exprs().len(), expected.len());
    }
}

#[test]
fn switch_successors_are_rewritable_in_place() {
    let selector = CfgExprId::from_u32(0);
    let mut terminator = CfgTerminator::Switch {
        selector,
        targets: vec![CfgBlockId::from_u32(1), CfgBlockId::from_u32(2)],
        default: CfgBlockId::from_u32(3),
    };

    for (index, target) in terminator.successors_mut().into_iter().enumerate() {
        *target = CfgBlockId::from_u32(10 + index as u32);
    }

    assert_eq!(
        terminator.successors(),
        vec![
            CfgBlockId::from_u32(10),
            CfgBlockId::from_u32(11),
            CfgBlockId::from_u32(12),
        ]
    );
}

#[test]
fn terminators_and_instructions_report_the_values_they_define() {
    let block = CfgBlockId::from_u32(0);
    let result = CfgValueId::from_u32(5);
    let binder = CfgValueId::from_u32(6);

    let matched = CfgTerminator::Match {
        scrutinee: CfgExprId::from_u32(0),
        arms: vec![CfgMatchArm {
            tag: SymbolId::from_u32(1),
            binders: vec![binder],
            projections: vec![CfgProjectionMode::Borrow],
            target: block,
        }],
        default: block,
    };
    assert_eq!(matched.defined_values().to_vec(), vec![binder]);
    assert!(CfgTerminator::Unreachable.defined_values().is_empty());

    let eval = CfgInstruction::Eval {
        result,
        value: CfgExprId::from_u32(0),
    };
    assert_eq!(eval.result(), Some(result));
    assert_eq!(
        eval.operands().to_vec(),
        vec![(CfgExprId::from_u32(0), OperandRole::Read)]
    );
    assert_eq!(CfgInstruction::Hole.result(), None);
}

#[test]
fn fingerprints_agree_on_structure_and_split_on_payload() {
    let mut program = CoreProgram::new();
    let left_one = int(&mut program, 1);
    let left_two = int(&mut program, 2);
    let left = push(
        &mut program,
        ExprKind::Binary {
            op: BinaryOp::Add,
            lhs: left_one,
            rhs: left_two,
        },
    );
    let right_one = int(&mut program, 1);
    let right_two = int(&mut program, 2);
    let right = push(
        &mut program,
        ExprKind::Binary {
            op: BinaryOp::Add,
            lhs: right_one,
            rhs: right_two,
        },
    );
    let different_op = push(
        &mut program,
        ExprKind::Binary {
            op: BinaryOp::Sub,
            lhs: left_one,
            rhs: left_two,
        },
    );
    let different_operand = push(
        &mut program,
        ExprKind::Binary {
            op: BinaryOp::Add,
            lhs: left_one,
            rhs: left_one,
        },
    );

    let fingerprints = fingerprint_exprs(&program);
    assert_eq!(fingerprints[left.index()], fingerprints[right.index()]);
    assert_ne!(
        fingerprints[left.index()],
        fingerprints[different_op.index()]
    );
    assert_ne!(
        fingerprints[left.index()],
        fingerprints[different_operand.index()]
    );
}

#[test]
fn fingerprints_separate_nodes_that_only_differ_by_span() {
    let mut program = CoreProgram::new();
    let early = program.push_expr(ExprNode {
        span: Span::new(cielo_base::SourceId::from_u32(0), 0, 1),
        kind: ExprKind::Literal(Literal::Int(1)),
    });
    let late = program.push_expr(ExprNode {
        span: Span::new(cielo_base::SourceId::from_u32(0), 4, 5),
        kind: ExprKind::Literal(Literal::Int(1)),
    });

    let fingerprints = fingerprint_exprs(&program);
    assert_ne!(fingerprints[early.index()], fingerprints[late.index()]);
}

#[test]
fn statement_operands_still_reach_expression_walks() {
    let mut program = CoreProgram::new();
    let operand = int(&mut program, 3);
    let negated = push(
        &mut program,
        ExprKind::Unary {
            op: UnaryOp::Neg,
            expr: operand,
        },
    );
    let stmt = program.push_stmt(StmtNode {
        span: Span::synthetic(),
        kind: StmtKind::Return(negated),
    });

    let roots = program
        .stmt(stmt)
        .expect("statement was just pushed")
        .child_exprs();
    let mut reached = Vec::new();
    for root in roots {
        walk_exprs(&program, root, &mut |expr_id, _| {
            reached.push(expr_id);
            Walk::Descend
        });
    }

    assert_eq!(reached, vec![negated, operand]);
}
