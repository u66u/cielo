#[path = "../benches/v1_runtime.rs"]
#[allow(dead_code)]
mod v1_runtime;

#[test]
fn runtime_thresholds_cover_all_runtime_benchmark_cases() {
    let cases = [
        "pure_runtime_loop",
        "unhandled_perform_loop",
        "direct_handled_loop",
        "control_handled_loop",
        "nested_direct_same_effect_loop",
        "nested_control_same_effect_loop",
    ];
    for case in cases {
        assert!(
            v1_runtime::v1_runtime_per_run_ms_limit(case).is_some(),
            "missing runtime threshold for benchmark case `{case}`"
        );
    }
}

#[test]
fn runtime_baselines_fit_thresholds() {
    let baseline_per_run_ms = [
        ("pure_runtime_loop", 1.396),
        ("unhandled_perform_loop", 1.400),
        ("direct_handled_loop", 1.314),
        ("control_handled_loop", 1.301),
        ("nested_direct_same_effect_loop", 1.552),
        ("nested_control_same_effect_loop", 1.588),
    ];
    for (case, per_run_ms) in baseline_per_run_ms {
        assert!(
            v1_runtime::check_v1_runtime_per_run_ms(case, per_run_ms).is_ok(),
            "runtime baseline for `{case}` should satisfy configured threshold"
        );
    }
}

#[test]
fn runtime_relative_baselines_fit_thresholds() {
    let baseline_relative = [
        ("pure_runtime_loop", 1.000),
        ("unhandled_perform_loop", 1.003),
        ("direct_handled_loop", 0.941),
        ("control_handled_loop", 0.931),
        ("nested_direct_same_effect_loop", 1.112),
        ("nested_control_same_effect_loop", 1.137),
    ];
    for (case, relative) in baseline_relative {
        assert!(
            v1_runtime::check_v1_runtime_relative_to_pure(case, relative).is_ok(),
            "runtime relative baseline for `{case}` should satisfy configured threshold"
        );
    }
}

#[test]
fn runtime_threshold_checker_rejects_regressions() {
    let abs_violation = v1_runtime::check_v1_runtime_per_run_ms("direct_handled_loop", 10.0)
        .expect_err("absolute runtime regression should be rejected");
    assert_eq!(abs_violation.case, "direct_handled_loop");
    assert!(abs_violation.measured_per_run_ms > abs_violation.per_run_ms_max);

    let rel_violation =
        v1_runtime::check_v1_runtime_relative_to_pure("unhandled_perform_loop", 2.0)
            .expect_err("relative runtime regression should be rejected");
    assert_eq!(rel_violation.case, "unhandled_perform_loop");
    assert!(rel_violation.measured_relative_to_pure > rel_violation.relative_to_pure_max);
}

#[test]
fn runtime_threshold_table_is_non_empty() {
    let thresholds = v1_runtime::v1_runtime_thresholds();
    assert!(
        !thresholds.is_empty(),
        "runtime threshold table should stay explicit and non-empty"
    );
    assert!(
        thresholds.iter().all(|row| row.per_run_ms_max > 0.0),
        "runtime thresholds should always be positive"
    );
    assert!(
        v1_runtime::v1_runtime_relative_to_pure_limit("pure_runtime_loop").is_none(),
        "pure case should remain the reference and not have relative threshold limits"
    );
}
