use cielo_staging::{GcConfig, GcPreset};

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
fn gc_preset_arc_optimized_enables_cfg_move_optimization() {
    let config = GcConfig::from_preset(GcPreset::ArcOptimized);
    assert!(config.gc_enabled(), "optimized preset should keep ARC on");
    assert!(
        config.arc_optimization_enabled(),
        "optimized preset should enable ARC optimization pass"
    );
    assert!(
        config.borrow_hazard_diagnostics_enabled(),
        "optimized preset should keep hazard diagnostics enabled"
    );
}
