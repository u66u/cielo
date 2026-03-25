use cielo_backend_api::{RuntimeManifest, RuntimeRequirement};

#[test]
fn c_backend_rejects_unimplemented_tracing_runtime() {
    let tracing = RuntimeManifest::new([
        RuntimeRequirement::TracingCollector,
        RuntimeRequirement::Safepoints,
        RuntimeRequirement::StackMaps,
    ]);

    assert!(!cielo_backend_c::capabilities().supports(&tracing));
}
