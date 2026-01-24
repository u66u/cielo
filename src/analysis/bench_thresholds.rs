#[derive(Clone, Copy, PartialEq, Debug)]
pub struct V1PipelineThreshold {
    pub case: &'static str,
    pub per_iter_ms_max: f64,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct V1RuntimeThreshold {
    pub case: &'static str,
    pub per_run_ms_max: f64,
    pub relative_to_pure_max: Option<f64>,
}

#[derive(Clone, PartialEq, Debug)]
pub struct ThresholdViolation {
    pub case: String,
    pub measured_per_iter_ms: f64,
    pub per_iter_ms_max: f64,
}

#[derive(Clone, PartialEq, Debug)]
pub struct RuntimeThresholdViolation {
    pub case: String,
    pub measured_per_run_ms: f64,
    pub per_run_ms_max: f64,
}

#[derive(Clone, PartialEq, Debug)]
pub struct RuntimeRelativeViolation {
    pub case: String,
    pub measured_relative_to_pure: f64,
    pub relative_to_pure_max: f64,
}

const V1_PIPELINE_THRESHOLDS: &[V1PipelineThreshold] = &[
    V1PipelineThreshold {
        case: "example",
        per_iter_ms_max: 0.100,
    },
    V1PipelineThreshold {
        case: "direct_resume",
        per_iter_ms_max: 0.030,
    },
    V1PipelineThreshold {
        case: "control_resume",
        per_iter_ms_max: 0.030,
    },
    V1PipelineThreshold {
        case: "mixed_handler",
        per_iter_ms_max: 0.060,
    },
];

const V1_RUNTIME_THRESHOLDS: &[V1RuntimeThreshold] = &[
    V1RuntimeThreshold {
        case: "pure_runtime_loop",
        per_run_ms_max: 2.500,
        relative_to_pure_max: None,
    },
    V1RuntimeThreshold {
        case: "unhandled_perform_loop",
        per_run_ms_max: 2.500,
        relative_to_pure_max: Some(1.200),
    },
    V1RuntimeThreshold {
        case: "direct_handled_loop",
        per_run_ms_max: 2.500,
        relative_to_pure_max: Some(1.200),
    },
    V1RuntimeThreshold {
        case: "control_handled_loop",
        per_run_ms_max: 2.500,
        relative_to_pure_max: Some(1.200),
    },
];

pub fn v1_pipeline_thresholds() -> &'static [V1PipelineThreshold] {
    V1_PIPELINE_THRESHOLDS
}

pub fn v1_runtime_thresholds() -> &'static [V1RuntimeThreshold] {
    V1_RUNTIME_THRESHOLDS
}

pub fn v1_pipeline_per_iter_ms_limit(case: &str) -> Option<f64> {
    V1_PIPELINE_THRESHOLDS
        .iter()
        .find(|threshold| threshold.case == case)
        .map(|threshold| threshold.per_iter_ms_max)
}

pub fn v1_runtime_per_run_ms_limit(case: &str) -> Option<f64> {
    V1_RUNTIME_THRESHOLDS
        .iter()
        .find(|threshold| threshold.case == case)
        .map(|threshold| threshold.per_run_ms_max)
}

pub fn v1_runtime_relative_to_pure_limit(case: &str) -> Option<f64> {
    V1_RUNTIME_THRESHOLDS
        .iter()
        .find(|threshold| threshold.case == case)
        .and_then(|threshold| threshold.relative_to_pure_max)
}

pub fn check_v1_pipeline_per_iter_ms(
    case: &str,
    measured_per_iter_ms: f64,
) -> Result<(), ThresholdViolation> {
    let Some(limit) = v1_pipeline_per_iter_ms_limit(case) else {
        return Ok(());
    };
    if measured_per_iter_ms <= limit {
        Ok(())
    } else {
        Err(ThresholdViolation {
            case: case.to_owned(),
            measured_per_iter_ms,
            per_iter_ms_max: limit,
        })
    }
}

pub fn check_v1_runtime_per_run_ms(
    case: &str,
    measured_per_run_ms: f64,
) -> Result<(), RuntimeThresholdViolation> {
    let Some(limit) = v1_runtime_per_run_ms_limit(case) else {
        return Ok(());
    };
    if measured_per_run_ms <= limit {
        Ok(())
    } else {
        Err(RuntimeThresholdViolation {
            case: case.to_owned(),
            measured_per_run_ms,
            per_run_ms_max: limit,
        })
    }
}

pub fn check_v1_runtime_relative_to_pure(
    case: &str,
    measured_relative_to_pure: f64,
) -> Result<(), RuntimeRelativeViolation> {
    let Some(limit) = v1_runtime_relative_to_pure_limit(case) else {
        return Ok(());
    };
    if measured_relative_to_pure <= limit {
        Ok(())
    } else {
        Err(RuntimeRelativeViolation {
            case: case.to_owned(),
            measured_relative_to_pure,
            relative_to_pure_max: limit,
        })
    }
}
