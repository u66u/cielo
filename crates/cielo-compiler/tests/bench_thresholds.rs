#[path = "../benches/v1_pipeline.rs"]
#[allow(dead_code)]
mod v1_pipeline;

#[test]
fn v1_thresholds_cover_all_benchmark_cases() {
    let cases = [
        "example",
        "direct_resume",
        "control_resume",
        "mixed_handler",
    ];
    for case in cases {
        assert!(
            v1_pipeline::v1_pipeline_per_iter_ms_limit(case).is_some(),
            "missing threshold for benchmark case `{case}`"
        );
    }
}

#[test]
fn v1_baseline_measurements_fit_thresholds() {
    let baseline_per_iter_ms = [
        ("example", 0.057),
        ("direct_resume", 0.013),
        ("control_resume", 0.012),
        ("mixed_handler", 0.029),
    ];
    for (case, per_iter_ms) in baseline_per_iter_ms {
        assert!(
            v1_pipeline::check_v1_pipeline_per_iter_ms(case, per_iter_ms).is_ok(),
            "baseline for `{case}` should satisfy configured threshold"
        );
    }
}

#[test]
fn threshold_checker_rejects_regressions() {
    let violation = v1_pipeline::check_v1_pipeline_per_iter_ms("direct_resume", 1.000)
        .expect_err("regression above threshold should be rejected");
    assert_eq!(violation.case, "direct_resume");
    assert!(violation.measured_per_iter_ms > violation.per_iter_ms_max);
}

#[test]
fn thresholds_slice_is_stable_and_non_empty() {
    let thresholds = v1_pipeline::v1_pipeline_thresholds();
    assert!(
        !thresholds.is_empty(),
        "threshold table should stay explicit and non-empty"
    );
    assert!(
        thresholds.iter().all(|row| row.per_iter_ms_max > 0.0),
        "thresholds should always be positive"
    );
}
