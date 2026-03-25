use cielo_ir::boundary::RuntimeModule;

use crate::{MemoryInput, RuntimeManifest, RuntimeRequirement, TracingCollector, TracingProfile};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TracingModule {
    pub runtime: RuntimeModule,
    pub collector: TracingCollector,
    pub safepoints: usize,
    pub root_maps: usize,
    pub manifest: RuntimeManifest,
}

pub fn lower(input: MemoryInput<'_>, profile: TracingProfile) -> TracingModule {
    TracingModule {
        root_maps: input.runtime.calls,
        safepoints: input.runtime.calls + input.runtime.suspension_points,
        runtime: input.runtime.clone(),
        collector: profile.collector,
        manifest: RuntimeManifest::new([
            RuntimeRequirement::TracingCollector,
            RuntimeRequirement::WriteBarriers,
            RuntimeRequirement::Safepoints,
            RuntimeRequirement::StackMaps,
        ]),
    }
}
