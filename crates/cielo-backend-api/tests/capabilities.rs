use cielo_backend_api::{BackendCapabilities, RuntimeManifest, RuntimeRequirement};

#[test]
fn capability_check_reports_only_missing_runtime_support() {
    let manifest = RuntimeManifest::new([
        RuntimeRequirement::TracingCollector,
        RuntimeRequirement::Safepoints,
        RuntimeRequirement::StackMaps,
    ]);
    let capabilities = BackendCapabilities::new([
        RuntimeRequirement::TracingCollector,
        RuntimeRequirement::Safepoints,
    ]);

    assert_eq!(
        capabilities.missing(&manifest),
        vec![RuntimeRequirement::StackMaps]
    );
    assert!(!capabilities.supports(&manifest));
}
