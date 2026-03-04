use std::collections::HashSet;
use std::path::Path;

use crate::passes::{bta, ct_eval, handler_specialize, residualize};
use crate::pipeline::compiler::TargetSpec;
use crate::pipeline::phases::{
    ArcResidualOpKind, BranchDecision, BtaClassified, Monomorphized, Reason, Residualized, Stage,
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
        sema.ownership_of_expr.len(),
        expr_count,
        "compiler bug: sema.ownership_of_expr length diverges from program.exprs length"
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
        sema.ownership_of_expr.len(),
        expr_count,
        "compiler bug: GraphCloner failed to map ownership_of_expr to cloned expressions"
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

    let mut seen_constant_keys = HashSet::new();
    let mut total_constant_bytes = 0usize;
    for entry in &residual_tables.constant_table.entries {
        assert!(
            seen_constant_keys.insert(entry.key.clone()),
            "compiler bug: residual constant_table contains duplicate key"
        );
        assert!(
            entry.estimated_size_bytes <= residual_tables.constant_table.entry_cap_bytes,
            "compiler bug: residual constant_table entry exceeds entry cap"
        );
        total_constant_bytes = total_constant_bytes.saturating_add(entry.estimated_size_bytes);
    }
    assert!(
        residual_tables.constant_table.total_size_bytes
            <= residual_tables.constant_table.unit_cap_bytes,
        "compiler bug: residual constant_table total size exceeds unit cap"
    );
    assert!(
        residual_tables.constant_table.total_size_bytes <= total_constant_bytes,
        "compiler bug: residual constant_table size accounting is inconsistent"
    );

    let arc_stats = residual_tables.arc_stats;
    assert!(
        arc_stats.final_retain_ops <= arc_stats.planned_retain_ops,
        "compiler bug: arc final retain count exceeds planned retain count"
    );
    assert!(
        arc_stats.final_release_ops <= arc_stats.planned_release_ops,
        "compiler bug: arc final release count exceeds planned release count"
    );
    assert_eq!(
        arc_stats
            .planned_retain_ops
            .saturating_sub(arc_stats.removed_retain_ops),
        arc_stats.final_retain_ops,
        "compiler bug: arc retain accounting mismatch"
    );
    assert_eq!(
        arc_stats
            .planned_release_ops
            .saturating_sub(arc_stats.removed_release_ops),
        arc_stats.final_release_ops,
        "compiler bug: arc release accounting mismatch"
    );

    let mut seen_arc_sites = HashSet::new();
    let mut retained_ops = 0u32;
    let mut released_ops = 0u32;
    for pair in residual_tables.arc_plan.ops.windows(2) {
        let lhs = pair[0];
        let rhs = pair[1];
        assert!(
            (
                lhs.stmt.index(),
                arc_op_kind_order(lhs.kind),
                arc_op_var(lhs.kind).index()
            ) < (
                rhs.stmt.index(),
                arc_op_kind_order(rhs.kind),
                arc_op_var(rhs.kind).index()
            ),
            "compiler bug: residual ARC plan ops are not sorted/deduplicated"
        );
    }
    for op in &residual_tables.arc_plan.ops {
        assert!(
            op.stmt.index() < program.stmts().len(),
            "compiler bug: residual ARC plan references out-of-bounds stmt s{}",
            op.stmt.as_u32()
        );
        let site_key = (op.stmt, arc_op_kind_order(op.kind), arc_op_var(op.kind));
        assert!(
            seen_arc_sites.insert(site_key),
            "compiler bug: residual ARC plan contains duplicate op at stmt s{} var v{}",
            op.stmt.as_u32(),
            arc_op_var(op.kind).as_u32()
        );
        match op.kind {
            ArcResidualOpKind::Retain { .. } => retained_ops = retained_ops.saturating_add(1),
            ArcResidualOpKind::Release { .. } => released_ops = released_ops.saturating_add(1),
        }
    }
    assert_eq!(
        retained_ops, arc_stats.final_retain_ops,
        "compiler bug: residual ARC retain plan count diverges from arc_stats.final_retain_ops"
    );
    assert_eq!(
        released_ops, arc_stats.final_release_ops,
        "compiler bug: residual ARC release plan count diverges from arc_stats.final_release_ops"
    );

    let orc_foundation = &residual_tables.orc_foundation;
    assert_eq!(
        orc_foundation.candidate_root_count as usize,
        orc_foundation.candidate_roots.len(),
        "compiler bug: ORC foundation root count mismatch"
    );
    assert_eq!(
        orc_foundation.candidate_link_count as usize,
        orc_foundation.candidate_links.len(),
        "compiler bug: ORC foundation link count mismatch"
    );
    for pair in orc_foundation.candidate_links.windows(2) {
        let lhs = pair[0];
        let rhs = pair[1];
        assert!(
            (lhs.0.index(), lhs.1.index()) < (rhs.0.index(), rhs.1.index()),
            "compiler bug: ORC foundation links are not sorted/deduplicated"
        );
    }

    let hazards = &residual_tables.borrow_hazards;
    assert_eq!(
        hazards.alias_fanout_count as usize,
        hazards.alias_fanout_sites.len(),
        "compiler bug: borrow hazard alias_fanout count mismatch"
    );
    assert_eq!(
        hazards.projection_count as usize,
        hazards.projection_sites.len(),
        "compiler bug: borrow hazard projection count mismatch"
    );
    assert_eq!(
        hazards.call_escape_count as usize,
        hazards.call_escape_sites.len(),
        "compiler bug: borrow hazard call_escape count mismatch"
    );
    assert_eq!(
        hazards.hotspots.len(),
        hazards.alias_fanout_sites.len()
            + hazards.projection_sites.len()
            + hazards.call_escape_sites.len(),
        "compiler bug: borrow hazard hotspot inventory mismatch"
    );
    for pair in hazards.hotspots.windows(2) {
        let lhs = pair[0];
        let rhs = pair[1];
        assert!(
            (lhs.stmt.index(), lhs.kind) < (rhs.stmt.index(), rhs.kind),
            "compiler bug: borrow hazard hotspots are not sorted/deduplicated"
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

fn arc_op_kind_order(kind: ArcResidualOpKind) -> u8 {
    match kind {
        ArcResidualOpKind::Retain { .. } => 0,
        ArcResidualOpKind::Release { .. } => 1,
    }
}

fn arc_op_var(kind: ArcResidualOpKind) -> crate::common::ids::VarId {
    match kind {
        ArcResidualOpKind::Retain { var } | ArcResidualOpKind::Release { var } => var,
    }
}
