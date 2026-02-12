use std::path::Path;

use crate::passes::{bta, ct_eval, handler_specialize, residualize};
use crate::pipeline::compiler::TargetSpec;
use crate::pipeline::phases::{
    BranchDecision, BtaClassified, Monomorphized, Reason, Residualized, Stage,
};

/// v1 fused stage A: Evaluate+Classify.
pub fn evaluate_classify(
    mono: Monomorphized,
    target: TargetSpec,
    query_cache_path: Option<&Path>,
) -> BtaClassified {
    let ct = ct_eval::run_with_query_cache(mono, target, query_cache_path);
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
    let expr_count = program.exprs().len();
    let stmt_count = program.stmts().len();
    let handler_count = program.handlers().len();

    assert_eq!(
        bta.stage_of_expr.len(),
        expr_count,
        "compiler bug: stage table must classify every expression at Evaluate+Classify boundary"
    );
    assert_eq!(
        bta.knownness_of_expr.len(),
        expr_count,
        "compiler bug: knownness table must classify every expression at Evaluate+Classify boundary"
    );

    // if these fall out of sync, Normalizer/Linearizer will panic out-of-bounds.
    assert_eq!(
        sema.type_of_expr.len(),
        expr_count,
        "compiler bug: sema.type_of_expr length diverges from program.exprs length"
    );
    assert_eq!(
        sema.effects_of_expr.len(),
        expr_count,
        "compiler bug: sema.effects_of_expr length diverges from program.exprs length"
    );
    assert_eq!(
        sema.effects_of_stmt.len(),
        stmt_count,
        "compiler bug: sema.effects_of_stmt length diverges from program.stmts length"
    );

    for expr_id in ct.ct_cache.keys() {
        assert!(
            expr_id.index() < expr_count,
            "compiler bug: ct_cache contains out-of-bounds expr id e{} at Evaluate+Classify boundary",
            expr_id.as_u32()
        );
    }
    for expr_id in ct.branch_decisions.keys() {
        assert!(
            expr_id.index() < expr_count,
            "compiler bug: branch_decisions contains out-of-bounds expr id e{} at Evaluate+Classify boundary",
            expr_id.as_u32()
        );
    }
    for expr_id in bta.stage_of_expr.keys() {
        assert!(
            expr_id.index() < expr_count,
            "compiler bug: bta.stage_of_expr contains out-of-bounds expr id e{} at Evaluate+Classify boundary",
            expr_id.as_u32()
        );
    }
    for expr_id in bta.knownness_of_expr.keys() {
        assert!(
            expr_id.index() < expr_count,
            "compiler bug: bta.knownness_of_expr contains out-of-bounds expr id e{} at Evaluate+Classify boundary",
            expr_id.as_u32()
        );
    }

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
        handler_count,
        "compiler bug: handler discharge table must classify every handler"
    );
    for handler_id in bta.handler_discharge.keys() {
        assert!(
            handler_id.index() < handler_count,
            "compiler bug: bta.handler_discharge contains out-of-bounds handler id h{}",
            handler_id.as_u32()
        );
    }
    for handler_id in bta.clause_discharge.keys() {
        assert!(
            handler_id.index() < handler_count,
            "compiler bug: bta.clause_discharge contains out-of-bounds handler id h{}",
            handler_id.as_u32()
        );
    }
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
    let ct = residual.ct();
    let bta = residual.bta();
    let sema = residual.sema();
    let mono = residual.mono();
    let residual_tables = residual.residual();
    let expr_count = program.exprs().len();
    let stmt_count = program.stmts().len();
    let func_count = program.functions().len();
    let handler_count = program.handlers().len();

    for function in program.functions() {
        assert!(
            function.declared_effects.is_empty(),
            "compiler bug: residualized functions must erase declared effects before normalize/lowering"
        );
    }

    // after specialization, side tables must stay aligned with the rewritten graph.
    assert_eq!(
        bta.stage_of_expr.len(),
        expr_count,
        "compiler bug: GraphCloner failed to map Stage to cloned expressions"
    );
    assert_eq!(
        bta.knownness_of_expr.len(),
        expr_count,
        "compiler bug: GraphCloner failed to map Knownness to cloned expressions"
    );
    assert_eq!(
        sema.type_of_expr.len(),
        expr_count,
        "compiler bug: GraphCloner failed to map TypeId to cloned expressions"
    );
    assert_eq!(
        sema.effects_of_expr.len(),
        expr_count,
        "compiler bug: GraphCloner failed to map effects_of_expr to cloned expressions"
    );
    assert_eq!(
        sema.effects_of_stmt.len(),
        stmt_count,
        "compiler bug: GraphCloner failed to map effects_of_stmt to cloned statements"
    );

    for expr_id in ct.ct_cache.keys() {
        assert!(
            expr_id.index() < expr_count,
            "compiler bug: ct_cache contains out-of-bounds expr id e{} after residualize+specialize",
            expr_id.as_u32()
        );
    }
    for expr_id in ct.branch_decisions.keys() {
        assert!(
            expr_id.index() < expr_count,
            "compiler bug: branch_decisions contains out-of-bounds expr id e{} after residualize+specialize",
            expr_id.as_u32()
        );
    }

    for (source, monos) in &mono.source_to_mono {
        assert!(
            source.index() < func_count,
            "compiler bug: monomorphization source f{} out of bounds after residualize+specialize",
            source.as_u32()
        );
        for mono_id in monos {
            assert!(
                mono_id.index() < func_count,
                "compiler bug: monomorphization instance f{} out of bounds after residualize+specialize",
                mono_id.as_u32()
            );
        }
    }

    for expr_id in bta.stage_of_expr.keys() {
        assert!(
            expr_id.index() < expr_count,
            "compiler bug: bta.stage_of_expr contains out-of-bounds expr id e{} after residualize+specialize",
            expr_id.as_u32()
        );
    }
    for expr_id in bta.knownness_of_expr.keys() {
        assert!(
            expr_id.index() < expr_count,
            "compiler bug: bta.knownness_of_expr contains out-of-bounds expr id e{} after residualize+specialize",
            expr_id.as_u32()
        );
    }
    for stage in bta.stage_of_expr.values() {
        assert_stage_reason_in_bounds(*stage, func_count, "bta.stage_of_expr");
    }
    for stage in bta.stage_of_var.values() {
        assert_stage_reason_in_bounds(*stage, func_count, "bta.stage_of_var");
    }

    for handler_id in bta.handler_discharge.keys() {
        assert!(
            handler_id.index() < handler_count,
            "compiler bug: bta.handler_discharge contains out-of-bounds handler id h{} after residualize+specialize",
            handler_id.as_u32()
        );
    }
    for handler_id in bta.clause_discharge.keys() {
        assert!(
            handler_id.index() < handler_count,
            "compiler bug: bta.clause_discharge contains out-of-bounds handler id h{} after residualize+specialize",
            handler_id.as_u32()
        );
    }
    for discharge in bta.handler_discharge.values() {
        if let Some(reason) = discharge.reason {
            assert_reason_in_bounds(reason, func_count, "bta.handler_discharge");
        }
    }
    for (handler_id, clauses) in bta.clause_discharge.iter() {
        let expected = program
            .handlers()
            .get(handler_id.index())
            .map(|handler| handler.clauses.len())
            .unwrap_or(0);
        assert_eq!(
            clauses.len(),
            expected,
            "compiler bug: bta.clause_discharge arity mismatch for handler h{} after residualize+specialize",
            handler_id.as_u32()
        );
        for clause in clauses {
            if let Some(reason) = clause.reason {
                assert_reason_in_bounds(reason, func_count, "bta.clause_discharge");
            }
        }
    }

    for func_id in residual_tables.function_effect_summary.keys() {
        assert!(
            func_id.index() < func_count,
            "compiler bug: residual function_effect_summary key f{} out of bounds after residualize+specialize",
            func_id.as_u32()
        );
    }
}

fn assert_stage_reason_in_bounds(stage: Stage, function_count: usize, context: &str) {
    if let Stage::Rt(reason) = stage {
        assert_reason_in_bounds(reason, function_count, context);
    }
}

fn assert_reason_in_bounds(reason: Reason, function_count: usize, context: &str) {
    match reason {
        Reason::Parameter { func, .. } | Reason::CtOnlyWithRuntimeArgs(func) => {
            assert!(
                func.index() < function_count,
                "compiler bug: {context} references out-of-bounds function f{} (functions={function_count})",
                func.as_u32()
            );
        }
        Reason::UnclassifiedRuntime
        | Reason::DependsOnVar(_)
        | Reason::EffectNotDischarged(_)
        | Reason::HandlerIsRuntime(_)
        | Reason::BranchOnRuntime(_)
        | Reason::NotPersistable(_)
        | Reason::UserForcedRuntime => {}
    }
}
