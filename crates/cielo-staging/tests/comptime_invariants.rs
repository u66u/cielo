use std::collections::HashSet;

#[path = "helpers/mod.rs"]
mod helpers;

use cielo_base::Interner;
use cielo_base::{FuncId, HandlerId, SourceId};
use cielo_ir::core::Literal;
use cielo_ir::core::{ExprKind, StmtKind};
use cielo_test_support::{Compiler, CompilerConfig};
use helpers::bta::{reason_has_valid_func_ids, stage_has_valid_func_ids};
use helpers::ir::reachable_stmt_count;

#[test]
fn stage_a_evaluate_classify_invariants_hold() {
    let src = r#"
effect LocalState { fn tick() -> Int }

fn guard() -> Bool {
  true
}

fn main() -> Int {
  let flag = guard();
  handle {
    if flag {
      do LocalState.tick();
      1
    } else {
      0
    }
  } with LocalState {
    | tick(resume) => resume(7)
  }
}
"#;

    let compiler = Compiler::new(CompilerConfig::default());
    let mut interner = Interner::new();
    let core = compiler.parse_and_lower_to_core(src, SourceId::from_u32(0), &mut interner);
    let staged = compiler.run_v1_evaluate_classify(core);
    let func_count = staged.program().functions().len();
    let expr_count = staged.program().exprs().len();
    let stmt_count = staged.program().stmts().len();
    let handler_count = staged.program().handlers().len();

    assert_eq!(
        staged.bta().stage_of_expr.len(),
        expr_count,
        "stage table must classify every expression"
    );
    assert_eq!(
        staged.bta().knownness_of_expr.len(),
        expr_count,
        "knownness table must classify every expression"
    );
    assert_eq!(
        staged.bta().handler_discharge.len(),
        handler_count,
        "handler discharge table must classify every handler"
    );
    assert_eq!(
        staged.sema().type_of_expr.len(),
        expr_count,
        "sema.type_of_expr must stay aligned with expression count at Stage-A boundary"
    );
    assert_eq!(
        staged.sema().effects_of_expr.len(),
        expr_count,
        "sema.effects_of_expr must stay aligned with expression count at Stage-A boundary"
    );
    assert_eq!(
        staged.sema().ownership_of_expr.len(),
        expr_count,
        "sema.ownership_of_expr must stay aligned with expression count at Stage-A boundary"
    );
    assert_eq!(
        staged.sema().effects_of_stmt.len(),
        stmt_count,
        "sema.effects_of_stmt must stay aligned with statement count at Stage-A boundary"
    );

    assert!(
        staged
            .ct()
            .ct_cache
            .keys()
            .all(|expr_id| expr_id.index() < expr_count),
        "ct cache must never contain out-of-bounds expression ids"
    );
    assert!(
        staged
            .ct()
            .branch_decisions
            .keys()
            .all(|expr_id| expr_id.index() < expr_count),
        "branch decision table must never contain out-of-bounds expression ids"
    );
    assert!(
        staged
            .bta()
            .stage_of_expr
            .keys()
            .all(|expr_id| expr_id.index() < expr_count),
        "stage table must never contain out-of-bounds expression ids"
    );
    assert!(
        staged
            .bta()
            .knownness_of_expr
            .keys()
            .all(|expr_id| expr_id.index() < expr_count),
        "knownness table must never contain out-of-bounds expression ids"
    );
    assert!(
        staged
            .bta()
            .handler_discharge
            .keys()
            .all(|handler_id| handler_id.index() < handler_count),
        "handler discharge table must never contain out-of-bounds handler ids"
    );
    assert!(
        staged
            .bta()
            .clause_discharge
            .keys()
            .all(|handler_id| handler_id.index() < handler_count),
        "clause discharge table must never contain out-of-bounds handler ids"
    );
    assert!(
        staged
            .bta()
            .stage_of_expr
            .values()
            .all(|stage| stage_has_valid_func_ids(*stage, func_count))
            && staged
                .bta()
                .stage_of_var
                .values()
                .all(|stage| stage_has_valid_func_ids(*stage, func_count)),
        "Stage-A reasons must not contain out-of-bounds function ids"
    );
    assert!(
        staged.bta().handler_discharge.values().all(|discharge| {
            discharge
                .reason
                .is_none_or(|reason| reason_has_valid_func_ids(reason, func_count))
        }),
        "handler discharge reasons must not contain out-of-bounds function ids at Stage-A boundary"
    );
    assert!(
        staged.bta().clause_discharge.values().all(|clauses| {
            clauses.iter().all(|clause| {
                clause
                    .reason
                    .is_none_or(|reason| reason_has_valid_func_ids(reason, func_count))
            })
        }),
        "clause discharge reasons must not contain out-of-bounds function ids at Stage-A boundary"
    );

    for (expr_id, decision) in staged.ct().branch_decisions.iter() {
        match decision {
            cielo_staging::pipeline::phases::BranchDecision::LiveTrue => assert_eq!(
                staged.ct().ct_cache.get(&expr_id),
                Some(&Literal::Bool(true)),
                "LiveTrue branch decisions must correspond to known true condition literals"
            ),
            cielo_staging::pipeline::phases::BranchDecision::LiveFalse => assert_eq!(
                staged.ct().ct_cache.get(&expr_id),
                Some(&Literal::Bool(false)),
                "LiveFalse branch decisions must correspond to known false condition literals"
            ),
            cielo_staging::pipeline::phases::BranchDecision::Unknown => {}
        }
    }

    for (idx, handler) in staged.program().handlers().iter().enumerate() {
        let handler_id = HandlerId::new(idx);
        let clauses = staged
            .bta()
            .clause_discharge
            .get(&handler_id)
            .unwrap_or_else(|| panic!("missing clause discharge table entry for handler h{}", idx));
        assert_eq!(
            clauses.len(),
            handler.clauses.len(),
            "clause discharge count must match source handler clause count"
        );
    }
}

#[test]
fn stage_b_residualize_specialize_invariants_hold() {
    let src = r#"
effect Console { fn print(s: String) -> () }

fn loop() -> Int with Console {
  loop()
}

fn main() -> Int {
  let x = handle { loop() } with Console {
    | print(s) => 0
  };
  x
}
"#;

    let compiler = Compiler::new(CompilerConfig::default());
    let mut interner = Interner::new();
    let core = compiler.parse_and_lower_to_core(src, SourceId::from_u32(1), &mut interner);
    let staged = compiler.run_v1_evaluate_classify(core);
    let residual = compiler.run_v1_residualize_specialize(staged);
    let func_count = residual.program().functions().len();
    let expr_count = residual.program().exprs().len();
    let stmt_count = residual.program().stmts().len();
    let handler_count = residual.program().handlers().len();

    assert!(
        residual
            .program()
            .functions()
            .iter()
            .all(|function| function.declared_effects.is_empty()),
        "residualized functions must erase declared effects before normalize/lowering"
    );
    assert_eq!(
        residual.sema().type_of_expr.len(),
        expr_count,
        "sema.type_of_expr must stay aligned with residual expressions"
    );
    assert_eq!(
        residual.sema().effects_of_expr.len(),
        expr_count,
        "sema.effects_of_expr must stay aligned with residual expressions"
    );
    assert_eq!(
        residual.sema().ownership_of_expr.len(),
        expr_count,
        "sema.ownership_of_expr must stay aligned with residual expressions"
    );
    assert_eq!(
        residual.sema().effects_of_stmt.len(),
        stmt_count,
        "sema.effects_of_stmt must stay aligned with residual statements"
    );
    assert_eq!(
        residual.bta().stage_of_expr.len(),
        expr_count,
        "bta.stage_of_expr must classify every expression after specialization remap"
    );
    assert_eq!(
        residual.bta().knownness_of_expr.len(),
        expr_count,
        "bta.knownness_of_expr must classify every expression after specialization remap"
    );
    assert_eq!(
        residual.bta().handler_discharge.len(),
        handler_count,
        "bta.handler_discharge must classify every handler after specialization remap"
    );
    assert_eq!(
        residual.bta().clause_discharge.len(),
        handler_count,
        "bta.clause_discharge must classify every handler after specialization remap"
    );
    assert!(
        residual
            .ct()
            .ct_cache
            .keys()
            .all(|expr_id| expr_id.index() < expr_count),
        "ct cache must not retain out-of-bounds expression ids after specialization remap"
    );
    assert!(
        residual
            .ct()
            .branch_decisions
            .keys()
            .all(|expr_id| expr_id.index() < expr_count),
        "branch decision table must not retain out-of-bounds expression ids after specialization remap"
    );

    assert!(
        residual
            .residual()
            .function_effect_summary
            .keys()
            .all(|id| id.index() < func_count),
        "residual function-effect summary keys must stay within compacted function id bounds"
    );
    assert!(
        residual
            .mono()
            .source_to_mono
            .iter()
            .all(|(source, monos)| {
                source.index() < func_count && monos.iter().all(|mono| mono.index() < func_count)
            }),
        "monomorphization summary ids must stay within compacted function id bounds"
    );
    assert!(
        residual
            .bta()
            .stage_of_expr
            .keys()
            .all(|expr_id| expr_id.index() < expr_count),
        "bta.stage_of_expr must not contain out-of-bounds expression ids after specialization remap"
    );
    assert!(
        residual
            .bta()
            .knownness_of_expr
            .keys()
            .all(|expr_id| expr_id.index() < expr_count),
        "bta.knownness_of_expr must not contain out-of-bounds expression ids after specialization remap"
    );
    assert!(
        residual
            .bta()
            .handler_discharge
            .keys()
            .all(|handler_id| handler_id.index() < handler_count),
        "bta.handler_discharge must not contain out-of-bounds handler ids after specialization remap"
    );
    assert!(
        residual
            .bta()
            .clause_discharge
            .keys()
            .all(|handler_id| handler_id.index() < handler_count),
        "bta.clause_discharge must not contain out-of-bounds handler ids after specialization remap"
    );
    for (handler_idx, handler) in residual.program().handlers().iter().enumerate() {
        let handler_id = HandlerId::new(handler_idx);
        let clauses = residual
            .bta()
            .clause_discharge
            .get(&handler_id)
            .expect("missing clause discharge table entry");
        assert_eq!(
            clauses.len(),
            handler.clauses.len(),
            "handler clause discharge arity must remain aligned after specialization remap"
        );
    }
    assert!(
        residual
            .bta()
            .stage_of_expr
            .values()
            .all(|stage| stage_has_valid_func_ids(*stage, func_count))
            && residual
                .bta()
                .stage_of_var
                .values()
                .all(|stage| stage_has_valid_func_ids(*stage, func_count)),
        "BTA stage reasons must not retain stale function ids after specialization pruning"
    );
    assert!(
        residual.bta().handler_discharge.values().all(|discharge| {
            discharge
                .reason
                .is_none_or(|reason| reason_has_valid_func_ids(reason, func_count))
        }),
        "handler discharge reasons must not retain stale function ids after specialization pruning"
    );
    assert!(
        residual.bta().clause_discharge.values().all(|clauses| {
            clauses.iter().all(|clause| {
                clause
                    .reason
                    .is_none_or(|reason| reason_has_valid_func_ids(reason, func_count))
            })
        }),
        "handler clause discharge reasons must not retain stale function ids after specialization pruning"
    );
}

#[test]
fn stage_c_normalize_invariants_hold() {
    let src = r#"
fn helper(x: Int) -> Int {
  x + 1
}

fn loop() -> Int {
  loop()
}

fn main() -> Int {
  let x = helper(41);
  let y = loop();
  x
}
"#;

    let compiler = Compiler::new(CompilerConfig::default());
    let mut interner = Interner::new();
    let core = compiler.parse_and_lower_to_core(src, SourceId::from_u32(2), &mut interner);
    let staged = compiler.run_v1_evaluate_classify(core);
    let residual = compiler.run_v1_residualize_specialize(staged);

    let before_stmt_count = reachable_stmt_count(residual.program());
    let once = compiler.run_v1_normalize(residual.clone());
    let twice = compiler.run_v1_normalize(once.clone());
    let once_stmt_count = reachable_stmt_count(once.program());
    let twice_stmt_count = reachable_stmt_count(twice.program());

    assert!(
        once_stmt_count <= before_stmt_count,
        "normalize shrink phases should not increase statement count ({once_stmt_count} > {before_stmt_count})"
    );
    assert_eq!(
        once_stmt_count, twice_stmt_count,
        "normalize should be idempotent after one shrink-inline-shrink run"
    );
    assert_eq!(
        format!("{:?}", once.program()),
        format!("{:?}", twice.program()),
        "normalizing an already normalized program should preserve IR shape"
    );

    let recursive_funcs = collect_direct_recursive_funcs(once.program());
    assert!(
        !recursive_funcs.is_empty(),
        "test fixture should contain at least one recursive function"
    );
    let callees = collect_stmt_call_targets(once.program());
    assert!(
        !callees.is_empty(),
        "test fixture should retain runtime calls after normalize"
    );
    assert!(
        callees
            .iter()
            .all(|callee| recursive_funcs.contains(callee)),
        "once-used non-recursive helpers should be inlined; remaining calls should target recursive functions only"
    );
}

fn collect_direct_recursive_funcs(program: &cielo_ir::core::CoreProgram) -> HashSet<FuncId> {
    let mut recursive = HashSet::new();
    for (idx, function) in program.functions().iter().enumerate() {
        let func_id = FuncId::new(idx);
        if function_calls_target(program, function.body, func_id) {
            recursive.insert(func_id);
        }
    }
    recursive
}

fn collect_stmt_call_targets(program: &cielo_ir::core::CoreProgram) -> HashSet<FuncId> {
    let mut callees = HashSet::new();
    for function in program.functions() {
        collect_stmt_callees(program, function.body, &mut callees);
    }
    callees
}

fn collect_stmt_callees(
    program: &cielo_ir::core::CoreProgram,
    root: cielo_base::StmtId,
    out: &mut HashSet<FuncId>,
) {
    let mut stack = vec![root];
    let mut seen_stmts = HashSet::new();
    let mut seen_exprs = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen_stmts.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if let StmtKind::Call { callee, .. } = stmt.kind {
            out.insert(callee);
        }
        for expr_id in stmt.child_exprs() {
            collect_expr_callees(program, expr_id, out, &mut seen_exprs);
        }
        stack.extend(stmt.child_stmts());
    }
}

fn collect_expr_callees(
    program: &cielo_ir::core::CoreProgram,
    root: cielo_base::ExprId,
    out: &mut HashSet<FuncId>,
    seen: &mut HashSet<cielo_base::ExprId>,
) {
    if !seen.insert(root) {
        return;
    }
    let Some(expr) = program.expr(root) else {
        return;
    };
    match &expr.kind {
        ExprKind::PureCall { callee, args } => {
            out.insert(*callee);
            for arg in args {
                collect_expr_callees(program, *arg, out, seen);
            }
        }
        ExprKind::Unary { expr, .. } => collect_expr_callees(program, *expr, out, seen),
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_expr_callees(program, *lhs, out, seen);
            collect_expr_callees(program, *rhs, out, seen);
        }
        ExprKind::MakeStruct { fields, .. } | ExprKind::MakeEnum { fields, .. } => {
            for field in fields {
                collect_expr_callees(program, *field, out, seen);
            }
        }
        ExprKind::Var(_) | ExprKind::Literal(_) | ExprKind::Error(_) => {}
    }
}

fn function_calls_target(
    program: &cielo_ir::core::CoreProgram,
    root: cielo_base::StmtId,
    target: FuncId,
) -> bool {
    let mut stack = vec![root];
    let mut seen_stmts = HashSet::new();
    let mut seen_exprs = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen_stmts.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if matches!(stmt.kind, StmtKind::Call { callee, .. } if callee == target) {
            return true;
        }
        for expr_id in stmt.child_exprs() {
            if expr_calls_target(program, expr_id, target, &mut seen_exprs) {
                return true;
            }
        }
        stack.extend(stmt.child_stmts());
    }
    false
}

fn expr_calls_target(
    program: &cielo_ir::core::CoreProgram,
    root: cielo_base::ExprId,
    target: FuncId,
    seen: &mut HashSet<cielo_base::ExprId>,
) -> bool {
    if !seen.insert(root) {
        return false;
    }
    let Some(expr) = program.expr(root) else {
        return false;
    };
    match &expr.kind {
        ExprKind::PureCall { callee, args } => {
            if *callee == target {
                return true;
            }
            args.iter()
                .copied()
                .any(|arg| expr_calls_target(program, arg, target, seen))
        }
        ExprKind::Unary { expr, .. } => expr_calls_target(program, *expr, target, seen),
        ExprKind::Binary { lhs, rhs, .. } => {
            expr_calls_target(program, *lhs, target, seen)
                || expr_calls_target(program, *rhs, target, seen)
        }
        ExprKind::MakeStruct { fields, .. } | ExprKind::MakeEnum { fields, .. } => fields
            .iter()
            .copied()
            .any(|field| expr_calls_target(program, field, target, seen)),
        ExprKind::Var(_) | ExprKind::Literal(_) | ExprKind::Error(_) => false,
    }
}
