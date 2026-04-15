use bitflags::bitflags;

/// The memory implementations that exist today.
///
/// New strategies get their own configuration type and enum variant when
/// their lowering is implemented. ARC flags therefore never leak into a
/// tracing collector or a region allocator.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MemoryStrategy {
    Unmanaged,
    ReferenceCounting(ArcConfig),
}

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct ArcFeatures: u32 {
        const INSERTION = 1 << 0;
        const OPTIMIZATION = 1 << 1;
        const EMISSION = 1 << 2;
        const VERIFIER = 1 << 3;
        const BORROW_HAZARD_DIAGNOSTICS = 1 << 4;
        const EMIT_TRACE_COMMENTS = 1 << 5;
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ArcConfig {
    pub features: ArcFeatures,
}

impl ArcConfig {
    pub fn insertion_enabled(self) -> bool {
        self.features.contains(ArcFeatures::INSERTION)
    }

    pub fn optimization_enabled(self) -> bool {
        self.insertion_enabled() && self.features.contains(ArcFeatures::OPTIMIZATION)
    }

    pub fn emission_enabled(self) -> bool {
        self.features.contains(ArcFeatures::EMISSION)
    }

    pub fn verify_enabled(self) -> bool {
        self.emission_enabled() && self.features.contains(ArcFeatures::VERIFIER)
    }

    pub fn borrow_hazard_diagnostics_enabled(self) -> bool {
        self.features
            .contains(ArcFeatures::BORROW_HAZARD_DIAGNOSTICS)
    }

    pub fn emit_trace_enabled(self) -> bool {
        self.emission_enabled() && self.features.contains(ArcFeatures::EMIT_TRACE_COMMENTS)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MemoryPreset {
    Unmanaged,
    ArcRaw,
    ArcOptimized,
    ArcNoVerify,
    ArcBenchRaw,
    ArcBenchOptimized,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct MemoryProfile {
    pub strategy: MemoryStrategy,
}

impl Default for MemoryProfile {
    fn default() -> Self {
        Self::from_preset(MemoryPreset::ArcOptimized)
    }
}

impl MemoryProfile {
    pub const fn from_preset(preset: MemoryPreset) -> Self {
        let strategy = match preset {
            MemoryPreset::Unmanaged => MemoryStrategy::Unmanaged,
            MemoryPreset::ArcRaw => MemoryStrategy::ReferenceCounting(ArcConfig {
                features: ArcFeatures::INSERTION.union(ArcFeatures::EMISSION),
            }),
            MemoryPreset::ArcOptimized => MemoryStrategy::ReferenceCounting(ArcConfig {
                features: ArcFeatures::INSERTION
                    .union(ArcFeatures::OPTIMIZATION)
                    .union(ArcFeatures::EMISSION)
                    .union(ArcFeatures::VERIFIER)
                    .union(ArcFeatures::BORROW_HAZARD_DIAGNOSTICS)
                    .union(ArcFeatures::EMIT_TRACE_COMMENTS),
            }),
            MemoryPreset::ArcNoVerify => MemoryStrategy::ReferenceCounting(ArcConfig {
                features: ArcFeatures::INSERTION
                    .union(ArcFeatures::OPTIMIZATION)
                    .union(ArcFeatures::EMISSION)
                    .union(ArcFeatures::BORROW_HAZARD_DIAGNOSTICS)
                    .union(ArcFeatures::EMIT_TRACE_COMMENTS),
            }),
            MemoryPreset::ArcBenchRaw => MemoryStrategy::ReferenceCounting(ArcConfig {
                features: ArcFeatures::INSERTION.union(ArcFeatures::EMISSION),
            }),
            MemoryPreset::ArcBenchOptimized => MemoryStrategy::ReferenceCounting(ArcConfig {
                features: ArcFeatures::INSERTION
                    .union(ArcFeatures::OPTIMIZATION)
                    .union(ArcFeatures::EMISSION),
            }),
        };
        Self { strategy }
    }

    pub fn reference_counting(self) -> Option<ArcConfig> {
        match self.strategy {
            MemoryStrategy::Unmanaged => None,
            MemoryStrategy::ReferenceCounting(config) => Some(config),
        }
    }

    pub fn reference_counting_mut(&mut self) -> Option<&mut ArcConfig> {
        match &mut self.strategy {
            MemoryStrategy::Unmanaged => None,
            MemoryStrategy::ReferenceCounting(config) => Some(config),
        }
    }
}
