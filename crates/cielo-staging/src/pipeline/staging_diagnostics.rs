use crate::pipeline::phases::{ResidualizeStats, Residualized, SpecializationStats};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StagingPassCounters {
    pub residualize: ResidualizeStats,
    pub specialize: SpecializationStats,
}

pub fn staging_pass_counters(residual: &Residualized) -> StagingPassCounters {
    StagingPassCounters {
        residualize: residual.report().residualize,
        specialize: residual.report().specialization,
    }
}

pub fn render_stage_b_counter_summary(counters: StagingPassCounters) -> String {
    format!(
        "residualize embedded_literals={}, pruned_if={}, pruned_match={}\n\
specialize candidates={}, created={}, reused={}, rewrites={}, skipped_shapes={}, skipped_limits={}",
        counters.residualize.embedded_literals,
        counters.residualize.pruned_if_branches,
        counters.residualize.pruned_match_branches,
        counters.specialize.candidates_seen,
        counters.specialize.created,
        counters.specialize.reused_existing,
        counters.specialize.rewrites,
        counters.specialize.skipped_varying_shapes,
        counters.specialize.skipped_limits
    )
}
