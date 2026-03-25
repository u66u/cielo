use cielo_ir::boundary::RuntimeModule;

use crate::{MemoryInput, RegionAlgorithm, RegionProfile, RuntimeManifest, RuntimeRequirement};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionModule {
    pub runtime: RuntimeModule,
    pub algorithm: RegionAlgorithm,
    pub regions: usize,
    pub constraints: usize,
    pub manifest: RuntimeManifest,
}

pub fn lower(input: MemoryInput<'_>, profile: RegionProfile) -> RegionModule {
    RegionModule {
        regions: input.runtime.allocations.max(1),
        constraints: input.runtime.calls + input.runtime.allocations,
        runtime: input.runtime.clone(),
        algorithm: profile.algorithm,
        manifest: RuntimeManifest::new([RuntimeRequirement::RegionAllocator]),
    }
}
