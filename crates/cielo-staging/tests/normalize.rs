use std::collections::HashSet;

#[path = "helpers/mod.rs"]
mod helpers;

use cielo_base::Interner;
use cielo_base::diagnostics::DiagnosticBag;
use cielo_base::span::Span;
use cielo_base::{EffectLabelId, ExprId, SourceId, SymbolId, VarId};
use cielo_ir::core::{
    CoreProgram, CoreTypeRef, ExprKind, ExprNode, FunctionDecl, HandlerDef, Literal, MatchArm,
    PrimitiveTypeRef, StmtKind, StmtNode,
};
use cielo_ir::effect::SortedEffectRow;
use cielo_staging::passes::normalize;
use cielo_staging::pipeline::phases::{ResidualFacts, SemanticTables, StagedCore, StagingReport};
use cielo_test_support::{PassConfig, PassHarness};
use helpers::ir::{contains_call_to, contains_if_stmt, reachable_stmt_count};

#[test]
fn normalize_shrink_is_monotone_and_idempotent() {
    let src = r#"
fn helper() -> Int {
  7
}

fn main() -> Int {
  let dead = 1;
  let x = helper();
  x
}
"#;
    let compiler = PassHarness::new(PassConfig::default());
    let mut interner = Interner::new();
    let residual = compiler.compile_source(src, SourceId::from_u32(0), &mut interner);

    let before_count = reachable_stmt_count(residual.program());
    let once = normalize::run(residual.clone());
    let once_count = reachable_stmt_count(once.program());
    let twice = normalize::run(once.clone());
    let twice_count = reachable_stmt_count(twice.program());

    assert!(
        once_count <= before_count,
        "normalize shrink phases should not increase reachable statement count ({once_count} > {before_count})"
    );
    assert_eq!(
        once_count, twice_count,
        "normalize should reach an idempotent statement-count fixpoint after one full sandwich run"
    );
    assert_eq!(
        format!("{:?}", once.program()),
        format!("{:?}", twice.program()),
        "running normalize twice should leave core program shape unchanged"
    );
}

#[test]
fn normalize_inlines_pure_call_expr_and_prunes_constant_if() {
    let span = Span::synthetic();
    let mut program = CoreProgram::new();

    let cond_true = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Bool(true)),
    });
    let cond_ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(cond_true),
    });
    let cond_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(10),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Bool),
        declared_effects: SortedEffectRow::empty(),
        body: cond_ret,
        ct_only: false,
        span,
    });

    let one = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(1)),
    });
    let two = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(2)),
    });
    let cond_var = VarId::from_u32(0);
    let cond_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(cond_var),
    });
    let then_ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(one),
    });
    let else_ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(two),
    });
    let branch = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::If {
            cond: cond_expr,
            then_branch: then_ret,
            else_branch: else_ret,
        },
    });
    let main_body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Call {
            result: cond_var,
            callee: cond_id,
            args: Vec::new(),
            effects: SortedEffectRow::empty(),
            next: branch,
        },
    });
    let main_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(11),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: main_body,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let normalized = normalize::run(residualized(program));
    let normalized_main = normalized
        .program()
        .function(main_id)
        .expect("main should exist")
        .body;

    assert!(
        !contains_if_stmt(normalized.program(), normalized_main),
        "after inlining the pure condition helper, normalize should prune the constant if branch"
    );
    assert!(
        !contains_call_to(normalized.program(), normalized_main, cond_id),
        "helper call should be removed after inline+shrink in normalize"
    );
}

#[test]
fn normalize_keeps_recursive_callee_callsites() {
    let span = Span::synthetic();
    let mut program = CoreProgram::new();

    let loop_placeholder_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(0)),
    });
    let loop_placeholder_body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(loop_placeholder_expr),
    });
    let loop_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(20),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: loop_placeholder_body,
        ct_only: false,
        span,
    });

    let loop_call_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::PureCall {
            callee: loop_id,
            args: Vec::new(),
        },
    });
    let loop_recursive_body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(loop_call_expr),
    });
    program.function_mut(loop_id).expect("loop").body = loop_recursive_body;

    let main_result = VarId::from_u32(1);
    let main_result_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(main_result),
    });
    let main_ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(main_result_expr),
    });
    let main_body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Call {
            result: main_result,
            callee: loop_id,
            args: Vec::new(),
            effects: SortedEffectRow::empty(),
            next: main_ret,
        },
    });
    let main_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(21),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: main_body,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let normalized = normalize::run(residualized(program));
    let normalized_main = normalized
        .program()
        .function(main_id)
        .expect("main should exist")
        .body;
    assert!(
        contains_call_to(normalized.program(), normalized_main, loop_id),
        "normalize must keep callsites for recursive callees"
    );
}

#[test]
fn normalize_eliminates_dead_let_binding() {
    let span = Span::synthetic();
    let mut program = CoreProgram::new();

    let dead_var = VarId::from_u32(30);
    let one = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(1)),
    });
    let two = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(2)),
    });
    let ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(two),
    });
    let root = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Let {
            binding: dead_var,
            value: one,
            next: ret,
        },
    });
    let main_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(31),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: root,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let normalized = normalize::run(residualized(program));
    let main_body = normalized
        .program()
        .function(main_id)
        .expect("main should exist")
        .body;
    assert!(
        matches!(
            normalized.program().stmt(main_body).map(|stmt| &stmt.kind),
            Some(StmtKind::Return(_))
        ),
        "dead let bindings should be removed in shrink phase"
    );
}

#[test]
fn normalize_commutes_val_return_to_let() {
    let span = Span::synthetic();
    let mut program = CoreProgram::new();

    let binding = VarId::from_u32(40);
    let value_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(9)),
    });
    let binding_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(binding),
    });
    let value_stmt = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(value_expr),
    });
    let next_stmt = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(binding_expr),
    });
    let root = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Val {
            binding,
            value: value_stmt,
            next: next_stmt,
        },
    });
    let main_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(41),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: root,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let normalized = normalize::run(residualized(program));
    let main_body = normalized
        .program()
        .function(main_id)
        .expect("main should exist")
        .body;
    let stmt = normalized.program().stmt(main_body).expect("root stmt");
    match &stmt.kind {
        StmtKind::Let {
            binding: out_binding,
            value,
            next,
        } => {
            assert_eq!(
                *out_binding, binding,
                "val->let should preserve binding var"
            );
            assert_eq!(
                *value, value_expr,
                "val->let should preserve returned value expression"
            );
            assert_eq!(*next, next_stmt, "val->let should preserve continuation");
        }
        other => panic!("expected Val(Return(_)) to commute into Let, got {other:?}"),
    }
}

#[test]
fn normalize_inlines_once_used_parameterized_call() {
    let span = Span::synthetic();
    let mut program = CoreProgram::new();

    let param = VarId::from_u32(50);
    let param_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(param),
    });
    let one = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(1)),
    });
    let add = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo_ir::core::BinaryOp::Add,
            lhs: param_expr,
            rhs: one,
        },
    });
    let inc_body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(add),
    });
    let inc_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(51),
        params: vec![param],
        param_types: vec![CoreTypeRef::Primitive(PrimitiveTypeRef::Int)],
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: inc_body,
        ct_only: false,
        span,
    });

    let arg = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(41)),
    });
    let result_var = VarId::from_u32(52);
    let result_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(result_var),
    });
    let main_ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(result_expr),
    });
    let main_body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Call {
            result: result_var,
            callee: inc_id,
            args: vec![arg],
            effects: SortedEffectRow::empty(),
            next: main_ret,
        },
    });
    let main_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(53),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: main_body,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let normalized = normalize::run(residualized(program));
    let normalized_main = normalized
        .program()
        .function(main_id)
        .expect("main should exist")
        .body;
    assert!(
        !contains_call_to(normalized.program(), normalized_main, inc_id),
        "once-used pure helper with args should inline into callsite"
    );

    let root_stmt = normalized
        .program()
        .stmt(normalized_main)
        .expect("normalized main root stmt");
    match &root_stmt.kind {
        StmtKind::Let { value, .. } => {
            assert!(
                !expr_contains_var(normalized.program(), *value, param),
                "inlined expression should substitute away callee parameter vars"
            );
        }
        other => panic!("expected inlined callsite to become Let, got {other:?}"),
    }
}

#[test]
fn normalize_reduces_known_match_and_materializes_binders() {
    let span = Span::synthetic();
    let mut program = CoreProgram::new();

    let enum_ty = SymbolId::from_u32(60);
    let tag = SymbolId::from_u32(61);
    let binder = VarId::from_u32(62);
    let payload = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(9)),
    });
    let scrutinee = program.push_expr(ExprNode {
        span,
        kind: ExprKind::MakeEnum {
            ty: enum_ty,
            variant: tag,
            fields: vec![payload],
        },
    });
    let binder_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(binder),
    });
    let arm_body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(binder_expr),
    });
    let default_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(0)),
    });
    let default_body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(default_expr),
    });
    let match_stmt = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Match {
            scrutinee,
            arms: vec![MatchArm {
                tag,
                binders: vec![binder],
                body: arm_body,
                span,
            }],
            default: Some(default_body),
        },
    });
    let main_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(63),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: match_stmt,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let normalized = normalize::run(residualized(program));
    let normalized_main = normalized
        .program()
        .function(main_id)
        .expect("main should exist")
        .body;
    let root_stmt = normalized
        .program()
        .stmt(normalized_main)
        .expect("normalized main root stmt");
    match &root_stmt.kind {
        StmtKind::Let {
            binding,
            value,
            next,
        } => {
            assert_eq!(
                *binding, binder,
                "known-match reduction should materialize arm binder via Let"
            );
            assert_eq!(
                *value, payload,
                "known-match reduction should bind the selected payload expression"
            );
            let next_stmt = normalized
                .program()
                .stmt(*next)
                .expect("known-match continuation");
            assert!(
                matches!(next_stmt.kind, StmtKind::Return(expr) if expr == binder_expr),
                "known-match reduction should continue with selected arm body"
            );
        }
        other => panic!("expected known match to reduce to Let+arm body, got {other:?}"),
    }
}

#[test]
fn normalize_speculatively_inlines_small_many_use_helper() {
    let span = Span::synthetic();
    let mut program = CoreProgram::new();

    let param = VarId::from_u32(70);
    let param_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(param),
    });
    let one = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(1)),
    });
    let inc = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo_ir::core::BinaryOp::Add,
            lhs: param_expr,
            rhs: one,
        },
    });
    let helper_body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(inc),
    });
    let helper_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(71),
        params: vec![param],
        param_types: vec![CoreTypeRef::Primitive(PrimitiveTypeRef::Int)],
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: helper_body,
        ct_only: false,
        span,
    });

    let arg_a = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(10)),
    });
    let arg_b = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(20)),
    });
    let left_var = VarId::from_u32(72);
    let right_var = VarId::from_u32(73);
    let left_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(left_var),
    });
    let right_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(right_var),
    });
    let sum_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo_ir::core::BinaryOp::Add,
            lhs: left_expr,
            rhs: right_expr,
        },
    });
    let ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(sum_expr),
    });
    let call_right = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Call {
            result: right_var,
            callee: helper_id,
            args: vec![arg_b],
            effects: SortedEffectRow::empty(),
            next: ret,
        },
    });
    let call_left = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Call {
            result: left_var,
            callee: helper_id,
            args: vec![arg_a],
            effects: SortedEffectRow::empty(),
            next: call_right,
        },
    });
    let main_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(74),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: call_left,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let normalized = normalize::run(residualized(program));
    let normalized_main = normalized
        .program()
        .function(main_id)
        .expect("main should exist")
        .body;
    assert!(
        !contains_call_to(normalized.program(), normalized_main, helper_id),
        "small many-used helper should be inlined during speculative inline phase"
    );
}

#[test]
fn normalize_respects_spec_inline_expr_size_budget() {
    let span = Span::synthetic();
    let mut program = CoreProgram::new();

    let param = VarId::from_u32(80);
    let mut body_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(param),
    });
    for offset in 0..32 {
        let rhs = program.push_expr(ExprNode {
            span,
            kind: ExprKind::Literal(Literal::Int(offset as i64)),
        });
        body_expr = program.push_expr(ExprNode {
            span,
            kind: ExprKind::Binary {
                op: cielo_ir::core::BinaryOp::Add,
                lhs: body_expr,
                rhs,
            },
        });
    }
    let helper_body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(body_expr),
    });
    let helper_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(81),
        params: vec![param],
        param_types: vec![CoreTypeRef::Primitive(PrimitiveTypeRef::Int)],
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: helper_body,
        ct_only: false,
        span,
    });

    let arg_a = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(3)),
    });
    let arg_b = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(4)),
    });
    let left_var = VarId::from_u32(82);
    let right_var = VarId::from_u32(83);
    let left_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(left_var),
    });
    let right_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(right_var),
    });
    let sum_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo_ir::core::BinaryOp::Add,
            lhs: left_expr,
            rhs: right_expr,
        },
    });
    let ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(sum_expr),
    });
    let call_right = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Call {
            result: right_var,
            callee: helper_id,
            args: vec![arg_b],
            effects: SortedEffectRow::empty(),
            next: ret,
        },
    });
    let call_left = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Call {
            result: left_var,
            callee: helper_id,
            args: vec![arg_a],
            effects: SortedEffectRow::empty(),
            next: call_right,
        },
    });
    let main_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(84),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: call_left,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let normalized = normalize::run(residualized(program));
    let normalized_main = normalized
        .program()
        .function(main_id)
        .expect("main should exist")
        .body;
    assert!(
        contains_call_to(normalized.program(), normalized_main, helper_id),
        "speculative inline should keep many-use helper calls when expression size exceeds budget"
    );
}

#[test]
fn normalize_preserves_used_handler_return_bindings() {
    let span = Span::synthetic();
    let mut program = CoreProgram::new();

    let zero = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(0)),
    });
    let main_ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(zero),
    });
    let main_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(90),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: main_ret,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let binding = VarId::from_u32(91);
    let binding_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(binding),
    });
    let payload = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(7)),
    });
    let handler_ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(binding_expr),
    });
    let handler_return_body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Let {
            binding,
            value: payload,
            next: handler_ret,
        },
    });
    let handler_id = program.add_handler(HandlerDef {
        effect: EffectLabelId::from_u32(0),
        return_param: VarId::from_u32(92),
        return_body: handler_return_body,
        clauses: Vec::new(),
        span,
    });

    let normalized = normalize::run(residualized(program));
    let handler = normalized
        .program()
        .handlers()
        .get(handler_id.index())
        .expect("handler should exist");
    let root_stmt = normalized
        .program()
        .stmt(handler.return_body)
        .expect("handler return body");
    match &root_stmt.kind {
        StmtKind::Let {
            binding: out_binding,
            value,
            next,
        } => {
            assert_eq!(
                *out_binding, binding,
                "handler return let binding should not be dropped when used"
            );
            assert_eq!(
                *value, payload,
                "handler return let payload should remain unchanged"
            );
            let next_stmt = normalized
                .program()
                .stmt(*next)
                .expect("handler return continuation");
            assert!(
                matches!(next_stmt.kind, StmtKind::Return(expr) if expr == binding_expr),
                "handler return continuation should keep the bound variable use"
            );
        }
        other => panic!("expected handler return body Let to remain, got {other:?}"),
    }
}

fn residualized(program: CoreProgram) -> StagedCore {
    let sema = SemanticTables::with_counts(program.exprs().len(), program.stmts().len());
    StagedCore::new(
        program,
        DiagnosticBag::default(),
        sema,
        ResidualFacts::default(),
        StagingReport::default(),
    )
}

fn expr_contains_var(program: &CoreProgram, root: ExprId, target: VarId) -> bool {
    let mut seen_exprs = HashSet::new();
    let mut stack = vec![root];
    while let Some(expr_id) = stack.pop() {
        if !seen_exprs.insert(expr_id) {
            continue;
        }
        let Some(expr) = program.expr(expr_id) else {
            continue;
        };
        match &expr.kind {
            ExprKind::Var(var) => {
                if *var == target {
                    return true;
                }
            }
            ExprKind::Unary { expr, .. } => stack.push(*expr),
            ExprKind::Binary { lhs, rhs, .. } => {
                stack.push(*lhs);
                stack.push(*rhs);
            }
            ExprKind::PureCall { args, .. }
            | ExprKind::MakeStruct { fields: args, .. }
            | ExprKind::MakeEnum { fields: args, .. } => stack.extend(args.iter().copied()),
            ExprKind::Literal(_) | ExprKind::Error(_) => {}
        }
    }
    false
}
