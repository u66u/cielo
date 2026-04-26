#[path = "../benches/v1_gc_overhead.rs"]
#[allow(dead_code)]
mod v1_gc_overhead;

#[test]
fn gc_overhead_thresholds_cover_all_cases_and_presets() {
    let cases = [
        "ctor_churn",
        "alias_churn",
        "branch_churn",
        "sink_copy_move_churn",
    ];
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
        ("ctor_churn", "arc_raw", 1.000),
        ("ctor_churn", "arc_optimized", 1.100),
        ("alias_churn", "arc_raw", 0.600),
        ("alias_churn", "arc_optimized", 0.700),
        ("branch_churn", "arc_raw", 0.450),
        ("branch_churn", "arc_optimized", 0.450),
        ("sink_copy_move_churn", "arc_raw", 1.000),
        ("sink_copy_move_churn", "arc_optimized", 1.100),
    ];
    for (case, preset, relative_to_off) in baseline_relative {
        assert!(
            v1_gc_overhead::check_gc_overhead_relative(case, preset, relative_to_off).is_ok(),
            "gc overhead baseline should fit threshold for case `{case}` preset `{preset}`"
        );
    }
}

#[test]
fn gc_optimizer_thresholds_cover_all_cases() {
    let cases = [
        "ctor_churn",
        "alias_churn",
        "branch_churn",
        "sink_copy_move_churn",
    ];
    for case in cases {
        assert!(
            v1_gc_overhead::gc_optimizer_relative_to_raw_limit(case).is_some(),
            "missing gc optimizer threshold for case `{case}`"
        );
    }
}

#[test]
fn gc_optimizer_baselines_fit_thresholds() {
    let baseline_relative_to_raw = [
        ("ctor_churn", 1.100),
        ("alias_churn", 1.340),
        ("branch_churn", 1.100),
        ("sink_copy_move_churn", 1.100),
    ];
    for (case, relative_to_raw) in baseline_relative_to_raw {
        assert!(
            v1_gc_overhead::check_gc_optimizer_relative_to_raw(case, relative_to_raw).is_ok(),
            "gc optimizer baseline should fit threshold for case `{case}`"
        );
    }
}

/// On alias_churn the planner eliminates every retain. That is the actual
/// claim, and unlike a wall-clock ratio it is exact on any machine.
#[test]
fn alias_churn_optimizes_away_every_retain() {
    assert!(
        v1_gc_overhead::check_gc_final_retain_ops("alias_churn", "arc_optimized", 0).is_ok(),
        "the optimized preset should emit no retains on alias_churn"
    );
}

#[test]
fn op_count_threshold_checker_rejects_regressions() {
    let violation = v1_gc_overhead::check_gc_final_retain_ops("alias_churn", "arc_optimized", 1)
        .expect_err("a single surviving retain is a regression");
    assert_eq!(violation.case, "alias_churn");
    assert_eq!(violation.max_final_retain_ops, 0);
    assert_eq!(violation.measured_final_retain_ops, 1);
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
fn gc_optimizer_threshold_checker_rejects_regressions() {
    let violation = v1_gc_overhead::check_gc_optimizer_relative_to_raw("ctor_churn", 3.0)
        .expect_err("gc optimizer regression above threshold should be rejected");
    assert_eq!(violation.case, "ctor_churn");
    assert!(
        violation.measured_runtime_relative_to_arc_raw > violation.max_runtime_relative_to_arc_raw
    );
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

    let optimizer_thresholds = v1_gc_overhead::gc_optimizer_thresholds();
    assert!(
        !optimizer_thresholds.is_empty(),
        "gc optimizer threshold table should stay explicit and non-empty"
    );
    assert!(
        optimizer_thresholds
            .iter()
            .all(|row| row.max_runtime_relative_to_arc_raw > 0.0),
        "gc optimizer thresholds should always be positive"
    );
}

#[test]
fn gc_overhead_presets_activate_expected_arc_paths() {
    let cases = [
        "ctor_churn",
        "alias_churn",
        "branch_churn",
        "sink_copy_move_churn",
    ];
    for case in cases {
        let off = v1_gc_overhead::gc_overhead_case_arc_stats(case, "off")
            .expect("off preset stats should compile");
        assert_eq!(
            off.planned_retain_ops + off.planned_release_ops,
            0,
            "off preset should not plan ARC ops for case `{case}`"
        );
        assert_eq!(
            off.final_retain_ops + off.final_release_ops,
            0,
            "off preset should not emit ARC ops for case `{case}`"
        );

        let raw = v1_gc_overhead::gc_overhead_case_arc_stats(case, "arc_raw")
            .expect("arc_raw preset stats should compile");
        assert!(
            raw.planned_retain_ops + raw.planned_release_ops > 0,
            "arc_raw preset should plan ARC ops for case `{case}`"
        );
        assert!(
            raw.final_retain_ops + raw.final_release_ops > 0,
            "arc_raw preset should keep ARC ops through emission for case `{case}`"
        );

        let optimized = v1_gc_overhead::gc_overhead_case_arc_stats(case, "arc_optimized")
            .expect("arc_optimized preset stats should compile");
        assert!(
            optimized.planned_retain_ops + optimized.planned_release_ops > 0,
            "arc_optimized preset should plan ARC ops for case `{case}`"
        );
        assert!(
            optimized.final_retain_ops + optimized.final_release_ops > 0,
            "arc_optimized preset should keep ARC ops through emission for case `{case}`"
        );
    }
}
