use std::path::Path;

use crate::passes::{bta, ct_propagate, handler_specialize, residualize};
use crate::pipeline::compiler::TargetSpec;
use crate::pipeline::phases::{BtaClassified, Monomorphized, Residualized};

/// v1 fused stage A: Evaluate+Classify.
pub fn evaluate_classify(
    mono: Monomorphized,
    target: TargetSpec,
    query_cache_path: Option<&Path>,
) -> BtaClassified {
    let ct = ct_propagate::run_with_query_cache(mono, target, query_cache_path);
    bta::run(ct)
}

/// v1 fused stage B: Residualize+Specialize.
pub fn residualize_specialize(classified: BtaClassified) -> Residualized {
    let residual = residualize::run(classified);
    handler_specialize::run(residual)
}
