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

use std::path::Path as FsPath;

use crate::passes::ct_common;
use crate::pipeline::compiler::TargetSpec;
use crate::pipeline::ct_query_cache::{
    CtQueryCacheSnapshot, deps_match, fingerprint_program, load_snapshot as load_query_snapshot,
    normalized_file_deps, save_snapshot as save_query_snapshot,
};
use crate::pipeline::phases::{CtPropagated, CtPropagationTables, Monomorphized};

pub fn run(mono: Monomorphized, target: TargetSpec) -> CtPropagated {
    run_with_query_cache(mono, target, None)
}

pub fn run_with_query_cache(
    mono: Monomorphized,
    target: TargetSpec,
    query_cache_path: Option<&FsPath>,
) -> CtPropagated {
    ct_common::assert_pre_staging_effects_concrete(mono.program());

    let mut ct = CtPropagationTables::default();
    ct.cache_key = ct_common::build_cache_key(target);
    ct.file_deps = ct_common::collect_file_deps(mono.program(), mono.sema());
    ct.file_deps = normalized_file_deps(ct.file_deps);

    let mut used_query_cache = false;
    if let Some(path) = query_cache_path {
        let program_fingerprint = fingerprint_program(mono.program());
        if let Ok(snapshot) = load_query_snapshot(path)
            && snapshot.program_fingerprint == program_fingerprint
            && snapshot.cache_key == ct.cache_key
            && deps_match(snapshot.file_deps.as_slice(), ct.file_deps.as_slice())
        {
            ct.ct_cache = snapshot.ct_cache;
            used_query_cache = true;
            ct.eval_stats.iterations = 1;
            ct.eval_stats.cache_hits = ct.ct_cache.len().try_into().unwrap_or(u32::MAX);
        }
    }

    if !used_query_cache {
        let (ct_cache, eval_stats) = ct_common::compute_ct_cache(mono.program(), target);
        ct.ct_cache = ct_cache;
        ct.eval_stats = eval_stats;
        if let Some(path) = query_cache_path {
            let snapshot = CtQueryCacheSnapshot {
                cache_key: ct.cache_key.clone(),
                file_deps: ct.file_deps.clone(),
                program_fingerprint: fingerprint_program(mono.program()),
                ct_cache: ct.ct_cache.clone(),
            };
            let _ = save_query_snapshot(path, &snapshot);
        }
    }

    ct.branch_decisions = ct_common::rebuild_branch_decisions(&ct.ct_cache);

    mono.into_ct_propagated(ct)
}
