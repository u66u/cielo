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

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    pub struct ArcOptLevel: u32 {
        const SAME_STMT_PAIR_ELIM = 1 << 0;
        const CFG_REDUNDANT_RELEASE_ELIM = 1 << 1;
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
    pub arc_opt_level: ArcOptLevel,
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
                arc_opt_level: ArcOptLevel::empty(),
            },
            GcPreset::ArcRaw => Self {
                mode: GcMode::Arc,
                features: GcFeatureFlags::ARC_INSERTION.union(GcFeatureFlags::ARC_EMISSION),
                arc_opt_level: ArcOptLevel::empty(),
            },
            GcPreset::ArcOptimized => Self {
                mode: GcMode::Arc,
                features: GcFeatureFlags::ARC_INSERTION
                    .union(GcFeatureFlags::ARC_OPTIMIZATION)
                    .union(GcFeatureFlags::ARC_EMISSION)
                    .union(GcFeatureFlags::ARC_VERIFIER)
                    .union(GcFeatureFlags::BORROW_HAZARD_DIAGNOSTICS)
                    .union(GcFeatureFlags::ARC_EMIT_TRACE_COMMENTS),
                arc_opt_level: ArcOptLevel::SAME_STMT_PAIR_ELIM
                    .union(ArcOptLevel::CFG_REDUNDANT_RELEASE_ELIM),
            },
            GcPreset::ArcNoVerify => Self {
                mode: GcMode::Arc,
                features: GcFeatureFlags::ARC_INSERTION
                    .union(GcFeatureFlags::ARC_OPTIMIZATION)
                    .union(GcFeatureFlags::ARC_EMISSION)
                    .union(GcFeatureFlags::BORROW_HAZARD_DIAGNOSTICS)
                    .union(GcFeatureFlags::ARC_EMIT_TRACE_COMMENTS),
                arc_opt_level: ArcOptLevel::SAME_STMT_PAIR_ELIM
                    .union(ArcOptLevel::CFG_REDUNDANT_RELEASE_ELIM),
            },
            GcPreset::ArcBenchRaw => Self {
                mode: GcMode::Arc,
                features: GcFeatureFlags::ARC_INSERTION.union(GcFeatureFlags::ARC_EMISSION),
                arc_opt_level: ArcOptLevel::empty(),
            },
            GcPreset::ArcBenchOptimized => Self {
                mode: GcMode::Arc,
                features: GcFeatureFlags::ARC_INSERTION
                    .union(GcFeatureFlags::ARC_OPTIMIZATION)
                    .union(GcFeatureFlags::ARC_EMISSION),
                arc_opt_level: ArcOptLevel::SAME_STMT_PAIR_ELIM
                    .union(ArcOptLevel::CFG_REDUNDANT_RELEASE_ELIM),
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

    pub fn effective_arc_opt_level(self) -> ArcOptLevel {
        if self.arc_optimization_enabled() {
            self.arc_opt_level
        } else {
            ArcOptLevel::empty()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ArcOptLevel, GcConfig, GcPreset};

    #[test]
    fn gc_preset_off_disables_arc_pipeline() {
        let config = GcConfig::from_preset(GcPreset::Off);
        assert!(
            !config.gc_enabled(),
            "off preset should disable GC entirely"
        );
        assert!(
            !config.arc_insertion_enabled(),
            "off preset should not plan ARC ops"
        );
        assert!(
            !config.arc_emission_enabled(),
            "off preset should not emit ARC runtime calls"
        );
        assert!(
            config.effective_arc_opt_level().is_empty(),
            "off preset should not enable ARC optimizer flags"
        );
    }

    #[test]
    fn gc_preset_arc_bench_raw_keeps_arc_without_optimizer() {
        let config = GcConfig::from_preset(GcPreset::ArcBenchRaw);
        assert!(config.gc_enabled(), "bench raw preset should keep ARC on");
        assert!(
            config.arc_insertion_enabled(),
            "bench raw preset should still materialize ARC ownership ops"
        );
        assert!(
            !config.arc_optimization_enabled(),
            "bench raw preset should disable ARC optimization passes"
        );
        assert!(
            config.arc_emission_enabled(),
            "bench raw preset should keep ARC runtime call emission enabled"
        );
        assert!(
            !config.arc_verify_enabled(),
            "bench raw preset should disable ARC verifier for compile overhead isolation"
        );
    }

    #[test]
    fn gc_preset_arc_optimized_enables_full_arc_opt_level() {
        let config = GcConfig::from_preset(GcPreset::ArcOptimized);
        let expected = ArcOptLevel::SAME_STMT_PAIR_ELIM | ArcOptLevel::CFG_REDUNDANT_RELEASE_ELIM;
        assert!(config.gc_enabled(), "optimized preset should keep ARC on");
        assert!(
            config.arc_optimization_enabled(),
            "optimized preset should enable ARC optimization pass"
        );
        assert_eq!(
            config.effective_arc_opt_level(),
            expected,
            "optimized preset should enable both local and cfg ARC elimination flags"
        );
        assert!(
            config.borrow_hazard_diagnostics_enabled(),
            "optimized preset should keep hazard diagnostics enabled"
        );
    }
}
