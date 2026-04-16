use cielo_base::Interner;
use cielo_base::diagnostics::DiagnosticBag;
use cielo_base::span::Span;
use cielo_base::{EffectLabelId, ExprId, FuncId, HandlerId, SourceId, SymbolId, VarId};
use cielo_ir::core::{
    CoreProgram, CoreTypeRef, EffectDecl, ExprKind, ExprNode, FunctionDecl, HandlerDef, Literal,
    PrimitiveTypeRef, StmtKind, StmtNode,
};
use cielo_ir::effect::{EffectProperties, SortedEffectRow};
use cielo_ir::target::TargetSpec;
use cielo_staging::passes::{bta, ct_eval};
use cielo_staging::pipeline::phases::{
    BranchDecision, CtPropagated, MonomorphizationSummary, Monomorphized, SemanticTables,
};
use cielo_test_support::{PassConfig, PassHarness};

#[test]
fn cteval_folds_pure_call_with_literal_arguments() {
    let src = r#"
fn add2(x: Int) -> Int {
  x + 2
}

fn main() -> Int {
  add2(40)
}
"#;

    let compiler = PassHarness::new(PassConfig::default());
    let mut interner = Interner::new();
    let core = compiler.parse_and_lower_to_core(src, SourceId::from_u32(0), &mut interner);
    let staged = compiler.run_v1_ct_eval(core);

    let call_exprs = staged
        .program()
        .exprs()
        .iter()
        .enumerate()
        .filter_map(|(idx, expr)| {
            matches!(expr.kind, ExprKind::PureCall { .. }).then_some(ExprId::new(idx))
        })
        .collect::<Vec<_>>();
    assert!(
        !call_exprs.is_empty(),
        "fixture must contain at least one pure call expression"
    );

    assert!(
        call_exprs
            .iter()
            .any(|expr_id| staged.ct().ct_cache.get(expr_id) == Some(&Literal::Int(42))),
        "ct evaluator should fold known-arg pure call to literal 42"
    );
}

#[test]
fn cteval_folds_pure_call_through_let_chain_in_callee() {
    let src = r#"
fn bump_twice(x: Int) -> Int {
  let y = x + 1;
  y + 1
}

fn main() -> Int {
  bump_twice(40)
}
"#;

    let compiler = PassHarness::new(PassConfig::default());
    let mut interner = Interner::new();
    let core = compiler.parse_and_lower_to_core(src, SourceId::from_u32(0), &mut interner);
    let staged = compiler.run_v1_ct_eval(core);

    let folded_values = staged.ct().ct_cache.values().collect::<Vec<_>>();
    assert!(
        folded_values.contains(&&Literal::Int(42)),
        "ct evaluator should execute let-chain body and fold call result"
    );
}

#[test]
fn cteval_keeps_recursive_pure_call_runtime_when_cycle_detected() {
    let src = r#"
fn loop(x: Int) -> Int {
  loop(x)
}

fn main() -> Int {
  loop(1)
}
"#;

    let compiler = PassHarness::new(PassConfig::default());
    let mut interner = Interner::new();
    let core = compiler.parse_and_lower_to_core(src, SourceId::from_u32(0), &mut interner);
    let staged = compiler.run_v1_ct_eval(core);

    let recursive_calls = staged
        .program()
        .exprs()
        .iter()
        .enumerate()
        .filter_map(|(idx, expr)| {
            let ExprKind::PureCall { callee, .. } = expr.kind else {
                return None;
            };
            (callee.index() == 0).then_some(ExprId::new(idx))
        })
        .collect::<Vec<_>>();
    assert!(
        !recursive_calls.is_empty(),
        "fixture must contain recursive pure-call expressions"
    );

    for expr_id in recursive_calls {
        assert!(
            staged.ct().ct_cache.get(&expr_id).is_none(),
            "recursive call expression e{} should not be folded in v1 evaluator slice",
            expr_id.as_u32()
        );
    }
}

#[test]
fn cteval_updates_branch_decisions_for_folded_call_condition() {
    let src = r#"
fn always_true() -> Bool {
  true
}

fn main() -> Int {
  if always_true() {
    1
  } else {
    2
  }
}
"#;

    let compiler = PassHarness::new(PassConfig::default());
    let mut interner = Interner::new();
    let core = compiler.parse_and_lower_to_core(src, SourceId::from_u32(0), &mut interner);
    let staged = compiler.run_v1_ct_eval(core);

    let call_expr = staged
        .program()
        .exprs()
        .iter()
        .enumerate()
        .find_map(|(idx, expr)| {
            matches!(expr.kind, ExprKind::PureCall { .. }).then_some(ExprId::new(idx))
        })
        .expect("fixture must contain always_true() call");

    assert_eq!(
        staged.ct().ct_cache.get(&call_expr),
        Some(&Literal::Bool(true)),
        "ct evaluator should fold condition helper call to known boolean"
    );
    assert_eq!(
        staged.ct().branch_decisions.get(&call_expr),
        Some(&BranchDecision::LiveTrue),
        "branch decision table should include evaluator-folded boolean conditions"
    );
}

#[test]
fn cteval_does_not_fold_call_when_callee_uses_match_stmt() {
    let span = Span::synthetic();
    let mut program = CoreProgram::new();

    let scrutinee = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(0)),
    });
    let fallback_lit = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(7)),
    });
    let fallback_ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(fallback_lit),
    });
    let callee_body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Match {
            scrutinee,
            arms: Vec::new(),
            default: Some(fallback_ret),
        },
    });

    let callee = add_pure_int_function(&mut program, SymbolId::from_u32(100), callee_body, span);
    let call_expr = add_main_returning_call(&mut program, callee, span);

    let staged = run_ct_eval_on_program(program, TargetSpec::default());
    assert!(
        staged.ct().ct_cache.get(&call_expr).is_none(),
        "callee bodies containing Match should remain conservative in CTE-1"
    );
}

#[test]
fn cteval_does_not_fold_call_when_callee_uses_handle_stmt() {
    let span = Span::synthetic();
    let mut program = CoreProgram::new();

    let effect = add_minimal_effect(&mut program, SymbolId::from_u32(200), span);
    let handler_return_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(0)),
    });
    let handler_return = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(handler_return_expr),
    });
    let _handler = program.add_handler(HandlerDef {
        effect,
        return_param: VarId::from_u32(900),
        return_body: handler_return,
        clauses: Vec::new(),
        span,
    });

    let body_ret_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(9)),
    });
    let body_ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(body_ret_expr),
    });
    let callee_body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Handle {
            handler: HandlerId::new(0),
            body: body_ret,
            next: None,
        },
    });

    let callee = add_pure_int_function(&mut program, SymbolId::from_u32(201), callee_body, span);
    let call_expr = add_main_returning_call(&mut program, callee, span);

    let staged = run_ct_eval_on_program(program, TargetSpec::default());
    assert!(
        staged.ct().ct_cache.get(&call_expr).is_none(),
        "callee bodies containing Handle should remain conservative in CTE-1"
    );
}

#[test]
fn cteval_does_not_fold_call_when_callee_uses_perform_stmt() {
    let span = Span::synthetic();
    let mut program = CoreProgram::new();

    let effect = add_minimal_effect(&mut program, SymbolId::from_u32(300), span);
    let ret_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(1)),
    });
    let ret_stmt = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(ret_expr),
    });
    let callee_body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Perform {
            result: None,
            effect,
            operation: SymbolId::from_u32(301),
            args: Vec::new(),
            next: ret_stmt,
        },
    });

    let callee = add_pure_int_function(&mut program, SymbolId::from_u32(302), callee_body, span);
    let call_expr = add_main_returning_call(&mut program, callee, span);

    let staged = run_ct_eval_on_program(program, TargetSpec::default());
    assert!(
        staged.ct().ct_cache.get(&call_expr).is_none(),
        "callee bodies containing Perform should remain conservative in CTE-1"
    );
}

#[test]
fn cteval_does_not_fold_calls_to_effectful_callee() {
    let span = Span::synthetic();
    let mut program = CoreProgram::new();

    let effect = add_minimal_effect(&mut program, SymbolId::from_u32(400), span);
    let ret_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(9)),
    });
    let ret_stmt = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(ret_expr),
    });
    let callee = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(401),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::singleton(effect),
        body: ret_stmt,
        ct_only: false,
        span,
    });

    let call_expr = add_main_returning_call(&mut program, callee, span);
    let staged = run_ct_eval_on_program(program, TargetSpec::default());

    assert!(
        staged.ct().ct_cache.get(&call_expr).is_none(),
        "ct evaluator should not fold calls to functions with non-empty effect rows"
    );
}

#[test]
fn cteval_accepts_comptime_stage_blocks_inside_callee() {
    let src = r#"
fn helper() -> Int {
  let y = @comptime { 40 + 2 };
  y
}

fn main() -> Int {
  helper()
}
"#;

    let compiler = PassHarness::new(PassConfig::default());
    let mut interner = Interner::new();
    let core = compiler.parse_and_lower_to_core(src, SourceId::from_u32(0), &mut interner);
    let staged = compiler.run_v1_ct_eval(core);

    let helper_call = staged
        .program()
        .exprs()
        .iter()
        .enumerate()
        .find_map(|(idx, expr)| match expr.kind {
            ExprKind::PureCall { callee, .. } if callee.index() == 0 => Some(ExprId::new(idx)),
            _ => None,
        })
        .expect("fixture must contain helper call from main");

    assert_eq!(
        staged.ct().ct_cache.get(&helper_call),
        Some(&Literal::Int(42)),
        "@comptime stage blocks should stay evaluable in CT evaluator path"
    );
}

#[test]
fn cteval_rejects_runtime_stage_blocks_inside_callee() {
    let src = r#"
fn helper() -> Int {
  let y = @runtime { 40 + 2 };
  y
}

fn main() -> Int {
  helper()
}
"#;

    let compiler = PassHarness::new(PassConfig::default());
    let mut interner = Interner::new();
    let core = compiler.parse_and_lower_to_core(src, SourceId::from_u32(0), &mut interner);
    let staged = compiler.run_v1_ct_eval(core);

    let helper_call = staged
        .program()
        .exprs()
        .iter()
        .enumerate()
        .find_map(|(idx, expr)| match expr.kind {
            ExprKind::PureCall { callee, .. } if callee.index() == 0 => Some(ExprId::new(idx)),
            _ => None,
        })
        .expect("fixture must contain helper call from main");

    assert!(
        staged.ct().ct_cache.get(&helper_call).is_none(),
        "@runtime stage blocks should keep enclosing call non-foldable in CT evaluator path"
    );
}

#[test]
fn cteval_call_depth_budget_is_deterministic() {
    let src = deep_call_chain_source(33);
    let compiler = PassHarness::new(PassConfig::default());

    let mut interner_a = Interner::new();
    let core_a =
        compiler.parse_and_lower_to_core(src.as_str(), SourceId::from_u32(0), &mut interner_a);
    let staged_a = compiler.run_v1_ct_eval(core_a);

    let mut interner_b = Interner::new();
    let core_b =
        compiler.parse_and_lower_to_core(src.as_str(), SourceId::from_u32(1), &mut interner_b);
    let staged_b = compiler.run_v1_ct_eval(core_b);

    let root_call_a = staged_a
        .program()
        .exprs()
        .iter()
        .enumerate()
        .find_map(|(idx, expr)| match expr.kind {
            ExprKind::PureCall { callee, .. } if callee.index() == 0 => Some(ExprId::new(idx)),
            _ => None,
        })
        .expect("expected main to call f0");
    let root_call_b = staged_b
        .program()
        .exprs()
        .iter()
        .enumerate()
        .find_map(|(idx, expr)| match expr.kind {
            ExprKind::PureCall { callee, .. } if callee.index() == 0 => Some(ExprId::new(idx)),
            _ => None,
        })
        .expect("expected main to call f0");

    assert!(
        staged_a.ct().ct_cache.get(&root_call_a).is_none()
            && staged_b.ct().ct_cache.get(&root_call_b).is_none(),
        "call chains beyond depth budget must remain non-folded"
    );

    let cache_snapshot_a = staged_a
        .ct()
        .ct_cache
        .iter()
        .map(|(expr_id, literal)| (expr_id.as_u32(), format!("{literal:?}")))
        .collect::<Vec<_>>();
    let cache_snapshot_b = staged_b
        .ct()
        .ct_cache
        .iter()
        .map(|(expr_id, literal)| (expr_id.as_u32(), format!("{literal:?}")))
        .collect::<Vec<_>>();

    assert_eq!(
        cache_snapshot_a, cache_snapshot_b,
        "ct cache entries should be deterministic across repeated deep-call runs"
    );
    assert_eq!(
        staged_a.ct().eval_stats,
        staged_b.ct().eval_stats,
        "evaluator statistics should be deterministic across repeated deep-call runs"
    );
}

#[test]
fn cteval_plus_bta_matches_baseline_on_program_without_pure_calls() {
    let src = r#"
fn main() -> Int {
  let x = @runtime { 1 + 2 };
  let y = x + 3;
  y
}
"#;

    let compiler = PassHarness::new(PassConfig::default());
    let mut interner_eval = Interner::new();
    let core_eval =
        compiler.parse_and_lower_to_core(src, SourceId::from_u32(0), &mut interner_eval);
    let ct_eval_state = compiler.run_v1_ct_eval(core_eval);
    let eval_classified = bta::run(ct_eval_state);

    let mut interner_base = Interner::new();
    let core_base =
        compiler.parse_and_lower_to_core(src, SourceId::from_u32(1), &mut interner_base);
    let base_classified = compiler.run_v1_evaluate_classify(core_base);

    assert_eq!(
        eval_classified.bta().stage_of_expr.len(),
        base_classified.bta().stage_of_expr.len(),
        "parity fixture should classify equal expression table sizes"
    );
    for (expr_id, baseline_stage) in base_classified.bta().stage_of_expr.iter() {
        assert_eq!(
            eval_classified.bta().stage_of_expr.get(&expr_id),
            Some(baseline_stage),
            "ct_eval+bta should match baseline stage for expression e{}",
            expr_id.as_u32()
        );
    }
}

#[test]
fn cteval_plus_bta_never_demotes_baseline_ct_expressions() {
    let src = r#"
fn add2(x: Int) -> Int {
  x + 2
}
fn main() -> Int {
  let a = 1 + 2;
  let b = add2(40);
  a + b
}
"#;

    let compiler = PassHarness::new(PassConfig::default());
    let mut interner_eval = Interner::new();
    let core_eval =
        compiler.parse_and_lower_to_core(src, SourceId::from_u32(0), &mut interner_eval);
    let ct_eval_state = compiler.run_v1_ct_eval(core_eval);
    let eval_classified = bta::run(ct_eval_state);

    let mut interner_base = Interner::new();
    let core_base =
        compiler.parse_and_lower_to_core(src, SourceId::from_u32(1), &mut interner_base);
    let base_classified = compiler.run_v1_evaluate_classify(core_base);

    for (expr_id, baseline_stage) in base_classified.bta().stage_of_expr.iter() {
        if !matches!(baseline_stage, cielo_staging::pipeline::phases::Stage::Ct) {
            continue;
        }
        assert!(
            matches!(
                eval_classified.bta().stage_of_expr.get(&expr_id),
                Some(cielo_staging::pipeline::phases::Stage::Ct)
            ),
            "ct_eval+bta must not demote baseline Ct expression e{}",
            expr_id.as_u32()
        );
    }
}

fn run_ct_eval_on_program(program: CoreProgram, target: TargetSpec) -> CtPropagated {
    let sema = SemanticTables::with_counts(program.exprs().len(), program.stmts().len());
    let mono = Monomorphized::new(
        program,
        DiagnosticBag::default(),
        sema,
        MonomorphizationSummary::default(),
    );
    ct_eval::run(mono, target)
}

fn add_pure_int_function(
    program: &mut CoreProgram,
    name: SymbolId,
    body: cielo_base::StmtId,
    span: Span,
) -> FuncId {
    program.add_function(FunctionDecl {
        name,
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body,
        ct_only: false,
        span,
    })
}

fn add_main_returning_call(program: &mut CoreProgram, callee: FuncId, span: Span) -> ExprId {
    let call_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::PureCall {
            callee,
            args: Vec::new(),
        },
    });
    let ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(call_expr),
    });
    let main = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(999),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: ret,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main]);
    call_expr
}

fn add_minimal_effect(program: &mut CoreProgram, name: SymbolId, span: Span) -> EffectLabelId {
    program.add_effect(EffectDecl {
        label: EffectLabelId::from_u32(0),
        name,
        properties: EffectProperties::default(),
        operations: Vec::new(),
        span,
    })
}

fn deep_call_chain_source(depth: usize) -> String {
    let mut src = String::new();
    for idx in (0..=depth).rev() {
        if idx == depth {
            src.push_str(format!("fn f{idx}(x: Int) -> Int {{ x }}\n\n").as_str());
        } else {
            src.push_str(format!("fn f{idx}(x: Int) -> Int {{ f{}(x) }}\n\n", idx + 1).as_str());
        }
    }
    src.push_str("fn main() -> Int { f0(1) }\n");
    src
}
