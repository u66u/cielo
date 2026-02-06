use std::path::Path;

use crate::passes::{bta, ct_propagate, handler_specialize, residualize};
use crate::pipeline::compiler::TargetSpec;
use crate::pipeline::phases::{BranchDecision, BtaClassified, Monomorphized, Residualized};

/// v1 fused stage A: Evaluate+Classify.
pub fn evaluate_classify(
    mono: Monomorphized,
    target: TargetSpec,
    query_cache_path: Option<&Path>,
) -> BtaClassified {
    let ct = ct_propagate::run_with_query_cache(mono, target, query_cache_path);
    let classified = bta::run(ct);
    assert_evaluate_classify_invariants(&classified);
    classified
}

/// v1 fused stage B: Residualize+Specialize.
pub fn residualize_specialize(classified: BtaClassified) -> Residualized {
    let residual = residualize::run(classified);
    let residual = handler_specialize::run(residual);
    assert_residualize_specialize_invariants(&residual);
    residual
}

fn assert_evaluate_classify_invariants(classified: &BtaClassified) {
    let program = classified.program();
    let ct = classified.ct();
    let bta = classified.bta();
    let sema = classified.sema();

    assert_eq!(
        bta.stage_of_expr.len(),
        program.exprs().len(),
        "compiler bug: stage table must classify every expression at Evaluate+Classify boundary"
    );
    assert_eq!(
        bta.knownness_of_expr.len(),
        program.exprs().len(),
        "compiler bug: knownness table must classify every expression at Evaluate+Classify boundary"
    );

    // if these fall out of sync, Normalizer/Linearizer will panic out-of-bounds.
    assert_eq!(
        sema.type_of_expr.len(),
        program.exprs().len(),
        "compiler bug: sema.type_of_expr length diverges from program.exprs length"
    );
    assert_eq!(
        sema.effects_of_expr.len(),
        program.exprs().len(),
        "compiler bug: sema.effects_of_expr length diverges from program.exprs length"
    );
    assert_eq!(
        sema.effects_of_stmt.len(),
        program.stmts().len(),
        "compiler bug: sema.effects_of_stmt length diverges from program.stmts length"
    );

    for (expr_id, decision) in ct.branch_decisions.iter() {
        let literal = ct.ct_cache.get(&expr_id).unwrap_or_else(|| {
            panic!("compiler bug: branch decision without ct literal for e{expr_id}")
        });
        match (decision, literal) {
            (BranchDecision::LiveTrue, crate::ir::core::Literal::Bool(true))
            | (BranchDecision::LiveFalse, crate::ir::core::Literal::Bool(false)) => {}
            (BranchDecision::Unknown, _) => {}
            _ => panic!(
                "compiler bug: inconsistent branch decision at e{}: {:?} vs {:?}",
                expr_id.as_u32(),
                decision,
                literal
            ),
        }
    }

    assert_eq!(
        bta.handler_discharge.len(),
        program.handlers().len(),
        "compiler bug: handler discharge table must classify every handler"
    );
    for (idx, handler) in program.handlers().iter().enumerate() {
        let handler_id = crate::common::ids::HandlerId::new(idx);
        let clauses = bta.clause_discharge.get(&handler_id).unwrap_or_else(|| {
            panic!(
                "compiler bug: missing clause discharge for handler h{}",
                handler_id.as_u32()
            )
        });
        assert_eq!(
            clauses.len(),
            handler.clauses.len(),
            "compiler bug: clause discharge arity mismatch for handler h{}",
            handler_id.as_u32()
        );
    }
}

fn assert_residualize_specialize_invariants(residual: &crate::pipeline::phases::Residualized) {
    let program = residual.program();
    let bta = residual.bta();
    let sema = residual.sema();

    for function in program.functions() {
        assert!(
            function.declared_effects.is_empty(),
            "compiler bug: residualized functions must erase declared effects before normalize/lowering"
        );
    }

    //after specialization, the IR should've grown. Ensure the cloner successfully
    // expanded the tables to cover the newly cloned Stmt and Expr nodes!
    assert_eq!(
        bta.stage_of_expr.len(),
        program.exprs().len(),
        "compiler bug: GraphCloner failed to map Stage to cloned expressions"
    );
    assert_eq!(
        sema.type_of_expr.len(),
        program.exprs().len(),
        "compiler bug: GraphCloner failed to map TypeId to cloned expressions"
    );
    assert_eq!(
        sema.effects_of_stmt.len(),
        program.stmts().len(),
        "compiler bug: GraphCloner failed to map effects_of_stmt to cloned statements"
    );
}
