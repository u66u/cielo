use cielo::common::ids::{HandlerId, SourceId};
use cielo::common::symbols::Interner;
use cielo::ir::core::Literal;
use cielo::pipeline::phases::{Reason, Stage};
use cielo::{Compiler, CompilerConfig};

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

    assert_eq!(
        staged.bta().stage_of_expr.len(),
        staged.program().exprs().len(),
        "stage table must classify every expression"
    );
    assert_eq!(
        staged.bta().knownness_of_expr.len(),
        staged.program().exprs().len(),
        "knownness table must classify every expression"
    );
    assert_eq!(
        staged.bta().handler_discharge.len(),
        staged.program().handlers().len(),
        "handler discharge table must classify every handler"
    );

    for (expr_id, decision) in staged.ct().branch_decisions.iter() {
        match decision {
            cielo::pipeline::phases::BranchDecision::LiveTrue => assert_eq!(
                staged.ct().ct_cache.get(&expr_id),
                Some(&Literal::Bool(true)),
                "LiveTrue branch decisions must correspond to known true condition literals"
            ),
            cielo::pipeline::phases::BranchDecision::LiveFalse => assert_eq!(
                staged.ct().ct_cache.get(&expr_id),
                Some(&Literal::Bool(false)),
                "LiveFalse branch decisions must correspond to known false condition literals"
            ),
            cielo::pipeline::phases::BranchDecision::Unknown => {}
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

    assert!(
        residual
            .program()
            .functions()
            .iter()
            .all(|function| function.declared_effects.is_empty()),
        "residualized functions must erase declared effects before normalize/lowering"
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

fn stage_has_valid_func_ids(stage: Stage, func_count: usize) -> bool {
    match stage {
        Stage::Ct => true,
        Stage::Rt(reason) => reason_has_valid_func_ids(reason, func_count),
    }
}

fn reason_has_valid_func_ids(reason: Reason, func_count: usize) -> bool {
    match reason {
        Reason::Parameter { func, .. } | Reason::CtOnlyWithRuntimeArgs(func) => {
            func.index() < func_count
        }
        _ => true,
    }
}
