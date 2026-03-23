//! Contract between machine lowering, runtime support, and emitters.

use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RuntimeRequirement {
    ReferenceCounting,
    TracingCollector,
    RegionAllocator,
    WriteBarriers,
    Safepoints,
    StackMaps,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuntimeManifest {
    requirements: BTreeSet<RuntimeRequirement>,
}

impl RuntimeManifest {
    pub fn new(requirements: impl IntoIterator<Item = RuntimeRequirement>) -> Self {
        Self {
            requirements: requirements.into_iter().collect(),
        }
    }

    pub fn requirements(&self) -> &BTreeSet<RuntimeRequirement> {
        &self.requirements
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BackendCapabilities {
    runtime: BTreeSet<RuntimeRequirement>,
}

impl BackendCapabilities {
    pub fn new(runtime: impl IntoIterator<Item = RuntimeRequirement>) -> Self {
        Self {
            runtime: runtime.into_iter().collect(),
        }
    }

    pub fn missing(&self, manifest: &RuntimeManifest) -> Vec<RuntimeRequirement> {
        manifest
            .requirements()
            .difference(&self.runtime)
            .copied()
            .collect()
    }

    pub fn supports(&self, manifest: &RuntimeManifest) -> bool {
        self.missing(manifest).is_empty()
    }
}

/// Backend-facing summary.  Strategy-specific region constraints, root maps,
/// or ownership facts have already been translated into explicit operations
/// before this boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineModule {
    pub runtime: RuntimeManifest,
    pub memory_operations: usize,
    pub word_size_bits: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artifact {
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendError {
    UnsupportedRuntime(Vec<RuntimeRequirement>),
    Emission(String),
}

pub trait Backend {
    fn capabilities(&self) -> BackendCapabilities;

    fn emit(&self, module: &MachineModule) -> Result<Artifact, BackendError>;

    fn validate(&self, module: &MachineModule) -> Result<(), BackendError> {
        let missing = self.capabilities().missing(&module.runtime);
        if missing.is_empty() {
            Ok(())
        } else {
            Err(BackendError::UnsupportedRuntime(missing))
        }
    }
}
