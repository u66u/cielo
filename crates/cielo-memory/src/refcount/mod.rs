use cielo_ir::boundary::RuntimeModule;

use crate::{MemoryInput, RefcountAlgorithm, RefcountProfile, RuntimeManifest, RuntimeRequirement};

pub mod analysis;
pub mod passes;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefcountModule {
    pub runtime: RuntimeModule,
    pub algorithm: RefcountAlgorithm,
    pub retain_release_sites: usize,
    pub manifest: RuntimeManifest,
}

pub fn lower(input: MemoryInput<'_>, profile: RefcountProfile) -> RefcountModule {
    RefcountModule {
        retain_release_sites: input.runtime.allocations.saturating_mul(2),
        runtime: input.runtime.clone(),
        algorithm: profile.algorithm,
        manifest: RuntimeManifest::new([RuntimeRequirement::ReferenceCounting]),
    }
}
