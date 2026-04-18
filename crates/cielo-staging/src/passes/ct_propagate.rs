// Pass 4/9: ct_propagate (compile-time constant propagation)
//
// Inputs:
// - Monomorphized Core program
//
// Outputs:
// - CtPropagationTables (`ct_cache`, branch decisions, file deps, cache key)
//
// Invariants:
// - Only pure Expr nodes are evaluated
// - Cache entries are deterministic literals keyed by ExprId
// - Integer evaluation follows target bit width semantics
//
// Diagnostics:
// - None in v0
//
// Complexity:
// - O(expr_count * fixpoint_iters), with small bounded iter count in practice

use crate::passes::ct_common;
use crate::pipeline::phases::{CtFileDep, CtPropagated, CtPropagationTables, Monomorphized};
use cielo_ir::target::TargetSpec;

pub fn run(mono: Monomorphized, target: TargetSpec, file_deps: Vec<CtFileDep>) -> CtPropagated {
    ct_common::assert_pre_staging_effects_concrete(mono.program());

    let mut ct = CtPropagationTables::default();
    ct.cache_key = ct_common::build_cache_key(target);
    ct.file_deps = file_deps;
    let (ct_cache, eval_stats) = ct_common::compute_ct_cache(mono.program(), target);
    ct.ct_cache = ct_cache;
    ct.eval_stats = eval_stats;

    ct.branch_decisions = ct_common::rebuild_branch_decisions(&ct.ct_cache);

    mono.into_ct_propagated(ct)
}
