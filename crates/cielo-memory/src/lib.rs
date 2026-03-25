//! Replaceable memory-management families.
//!
//! Each family owns its profile, analyses, and result type.  They share only
//! the neutral runtime input and runtime-manifest vocabulary.

mod profile;
pub mod refcount;
pub mod regions;
pub mod tracing;

use cielo_ir::boundary::RuntimeModule;

pub use cielo_backend_api::{RuntimeManifest, RuntimeRequirement};
pub use profile::{
    MemoryModel, MemoryProfile, RefcountAlgorithm, RefcountProfile, RegionAlgorithm, RegionProfile,
    TracingCollector, TracingProfile,
};
pub use refcount::RefcountModule;
pub use regions::RegionModule;
pub use tracing::TracingModule;

// Compatibility namespaces for the ARC implementation while callers migrate
// to `refcount::{analysis,passes}`.
pub mod analysis {
    pub use crate::refcount::analysis::cfg_liveness;
}

pub mod common {
    pub mod gc {
        pub use cielo_staging::common::gc::*;
    }

    pub mod ids {
        pub use cielo_base::ids::*;
    }
}

pub mod ir {
    pub use cielo_ir::{cfg, core, linear, walk};
}

pub mod passes {
    pub use crate::refcount::passes::cfg_arc;
}

pub mod pipeline {
    pub mod phases {
        pub use cielo_staging::pipeline::phases::*;
    }
}

pub mod sema {
    pub mod ownership {
        pub use cielo_staging::sema::ownership::*;
    }
}

#[derive(Clone, Copy, Debug)]
pub struct MemoryInput<'a> {
    pub runtime: &'a RuntimeModule,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryModule {
    ReferenceCounting(RefcountModule),
    Tracing(TracingModule),
    Regions(RegionModule),
}

/// Compatibility facade for code that used the original single strategy
/// trait. The database does not use this trait: it requests the concrete
/// family query directly, preserving separate Salsa memo entries.
pub trait MemoryStrategy {
    fn lower(&self, input: MemoryInput<'_>) -> MemoryModule;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ReferenceCounting;

impl MemoryStrategy for ReferenceCounting {
    fn lower(&self, input: MemoryInput<'_>) -> MemoryModule {
        MemoryModule::ReferenceCounting(refcount::lower(input, Default::default()))
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Tracing;

impl MemoryStrategy for Tracing {
    fn lower(&self, input: MemoryInput<'_>) -> MemoryModule {
        MemoryModule::Tracing(tracing::lower(input, Default::default()))
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Regions;

impl MemoryStrategy for Regions {
    fn lower(&self, input: MemoryInput<'_>) -> MemoryModule {
        MemoryModule::Regions(regions::lower(input, Default::default()))
    }
}

impl MemoryModule {
    pub fn runtime(&self) -> &RuntimeModule {
        match self {
            Self::ReferenceCounting(module) => &module.runtime,
            Self::Tracing(module) => &module.runtime,
            Self::Regions(module) => &module.runtime,
        }
    }

    pub fn manifest(&self) -> &RuntimeManifest {
        match self {
            Self::ReferenceCounting(module) => &module.manifest,
            Self::Tracing(module) => &module.manifest,
            Self::Regions(module) => &module.manifest,
        }
    }
}

/// Thin convenience dispatcher for callers that do not need Salsa.  The
/// database uses separate family queries instead, so each result is cached
/// independently.
pub fn lower(input: MemoryInput<'_>, profile: MemoryProfile) -> MemoryModule {
    match profile.model {
        MemoryModel::ReferenceCounting => {
            MemoryModule::ReferenceCounting(refcount::lower(input, profile.refcount))
        }
        MemoryModel::Tracing => MemoryModule::Tracing(tracing::lower(input, profile.tracing)),
        MemoryModel::Regions => MemoryModule::Regions(regions::lower(input, profile.regions)),
    }
}
