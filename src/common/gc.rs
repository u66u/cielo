use bitflags::bitflags;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GcMode {
    Off,
    Arc,
}

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct GcFeatureFlags: u32 {
        const ARC_INSERTION = 1 << 0;
        const ARC_OPTIMIZATION = 1 << 1;
        const ARC_EMISSION = 1 << 2;
        const ARC_VERIFIER = 1 << 3;
        const BORROW_HAZARD_DIAGNOSTICS = 1 << 4;
        const ARC_EMIT_TRACE_COMMENTS = 1 << 5;
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GcPreset {
    Off,
    ArcRaw,
    ArcOptimized,
    ArcNoVerify,
    ArcBenchRaw,
    ArcBenchOptimized,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GcConfig {
    pub mode: GcMode,
    pub features: GcFeatureFlags,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self::from_preset(GcPreset::ArcOptimized)
    }
}

impl GcConfig {
    pub const fn from_preset(preset: GcPreset) -> Self {
        match preset {
            GcPreset::Off => Self {
                mode: GcMode::Off,
                features: GcFeatureFlags::empty(),
            },
            GcPreset::ArcRaw => Self {
                mode: GcMode::Arc,
                features: GcFeatureFlags::ARC_INSERTION.union(GcFeatureFlags::ARC_EMISSION),
            },
            GcPreset::ArcOptimized => Self {
                mode: GcMode::Arc,
                features: GcFeatureFlags::ARC_INSERTION
                    .union(GcFeatureFlags::ARC_OPTIMIZATION)
                    .union(GcFeatureFlags::ARC_EMISSION)
                    .union(GcFeatureFlags::ARC_VERIFIER)
                    .union(GcFeatureFlags::BORROW_HAZARD_DIAGNOSTICS)
                    .union(GcFeatureFlags::ARC_EMIT_TRACE_COMMENTS),
            },
            GcPreset::ArcNoVerify => Self {
                mode: GcMode::Arc,
                features: GcFeatureFlags::ARC_INSERTION
                    .union(GcFeatureFlags::ARC_OPTIMIZATION)
                    .union(GcFeatureFlags::ARC_EMISSION)
                    .union(GcFeatureFlags::BORROW_HAZARD_DIAGNOSTICS)
                    .union(GcFeatureFlags::ARC_EMIT_TRACE_COMMENTS),
            },
            GcPreset::ArcBenchRaw => Self {
                mode: GcMode::Arc,
                features: GcFeatureFlags::ARC_INSERTION.union(GcFeatureFlags::ARC_EMISSION),
            },
            GcPreset::ArcBenchOptimized => Self {
                mode: GcMode::Arc,
                features: GcFeatureFlags::ARC_INSERTION
                    .union(GcFeatureFlags::ARC_OPTIMIZATION)
                    .union(GcFeatureFlags::ARC_EMISSION),
            },
        }
    }

    pub fn gc_enabled(self) -> bool {
        matches!(self.mode, GcMode::Arc)
    }

    pub fn arc_insertion_enabled(self) -> bool {
        self.gc_enabled() && self.features.contains(GcFeatureFlags::ARC_INSERTION)
    }

    pub fn arc_optimization_enabled(self) -> bool {
        self.arc_insertion_enabled() && self.features.contains(GcFeatureFlags::ARC_OPTIMIZATION)
    }

    pub fn arc_emission_enabled(self) -> bool {
        self.gc_enabled() && self.features.contains(GcFeatureFlags::ARC_EMISSION)
    }

    pub fn arc_verify_enabled(self) -> bool {
        self.arc_emission_enabled() && self.features.contains(GcFeatureFlags::ARC_VERIFIER)
    }

    pub fn borrow_hazard_diagnostics_enabled(self) -> bool {
        self.gc_enabled()
            && self
                .features
                .contains(GcFeatureFlags::BORROW_HAZARD_DIAGNOSTICS)
    }

    pub fn arc_emit_trace_enabled(self) -> bool {
        self.arc_emission_enabled()
            && self
                .features
                .contains(GcFeatureFlags::ARC_EMIT_TRACE_COMMENTS)
    }
}
