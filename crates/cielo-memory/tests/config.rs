use cielo_memory::{ArcFeatures, MemoryPreset, MemoryProfile, MemoryStrategy};

#[test]
fn unmanaged_profile_has_no_arc_configuration() {
    let profile = MemoryProfile::from_preset(MemoryPreset::Unmanaged);
    assert_eq!(profile.strategy, MemoryStrategy::Unmanaged);
    assert!(profile.reference_counting().is_none());
}

#[test]
fn raw_arc_profile_inserts_and_emits_without_optimizing() {
    let profile = MemoryProfile::from_preset(MemoryPreset::ArcBenchRaw);
    let config = profile
        .reference_counting()
        .expect("raw ARC preset should select reference counting");
    assert!(config.insertion_enabled());
    assert!(!config.optimization_enabled());
    assert!(config.emission_enabled());
    assert!(!config.verify_enabled());
}

#[test]
fn optimized_arc_profile_keeps_diagnostics_and_verification() {
    let profile = MemoryProfile::from_preset(MemoryPreset::ArcOptimized);
    let config = profile
        .reference_counting()
        .expect("optimized ARC preset should select reference counting");
    assert!(config.optimization_enabled());
    assert!(config.borrow_hazard_diagnostics_enabled());
    assert!(config.verify_enabled());
    assert!(config.features.contains(ArcFeatures::EMIT_TRACE_COMMENTS));
}
