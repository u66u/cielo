#[path = "../benches/v1_gc_overhead.rs"]
#[allow(dead_code)]
mod v1_gc_overhead;

#[test]
fn gc_overhead_thresholds_cover_all_cases_and_presets() {
    let cases = ["ctor_churn", "alias_churn", "branch_churn"];
    let presets = ["arc_raw", "arc_optimized"];
    for case in cases {
        for preset in presets {
            assert!(
                v1_gc_overhead::gc_overhead_relative_limit(case, preset).is_some(),
                "missing gc overhead threshold for case `{case}` preset `{preset}`"
            );
        }
    }
}

#[test]
fn gc_overhead_baselines_fit_thresholds() {
    let baseline_relative = [
        ("ctor_churn", "arc_raw", 2.600),
        ("ctor_churn", "arc_optimized", 1.900),
        ("alias_churn", "arc_raw", 2.700),
        ("alias_churn", "arc_optimized", 2.000),
        ("branch_churn", "arc_raw", 3.000),
        ("branch_churn", "arc_optimized", 2.400),
    ];
    for (case, preset, relative_to_off) in baseline_relative {
        assert!(
            v1_gc_overhead::check_gc_overhead_relative(case, preset, relative_to_off).is_ok(),
            "gc overhead baseline should fit threshold for case `{case}` preset `{preset}`"
        );
    }
}

#[test]
fn gc_overhead_threshold_checker_rejects_regressions() {
    let violation = v1_gc_overhead::check_gc_overhead_relative("ctor_churn", "arc_raw", 99.0)
        .expect_err("gc overhead regression above threshold should be rejected");
    assert_eq!(violation.case, "ctor_churn");
    assert_eq!(violation.preset, "arc_raw");
    assert!(violation.measured_runtime_relative_to_off > violation.max_runtime_relative_to_off);
}

#[test]
fn gc_overhead_threshold_table_is_non_empty_and_positive() {
    let thresholds = v1_gc_overhead::gc_overhead_thresholds();
    assert!(
        !thresholds.is_empty(),
        "gc overhead threshold table should stay explicit and non-empty"
    );
    assert!(
        thresholds
            .iter()
            .all(|row| row.max_runtime_relative_to_off > 0.0),
        "gc overhead thresholds should always be positive"
    );
    assert!(
        v1_gc_overhead::gc_overhead_relative_limit("ctor_churn", "off").is_none(),
        "`off` is the reference baseline and must not have overhead threshold limits"
    );
}
