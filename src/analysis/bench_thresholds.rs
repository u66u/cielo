#[derive(Clone, Copy, PartialEq, Debug)]
pub struct V1PipelineThreshold {
    pub case: &'static str,
    pub per_iter_ms_max: f64,
}

#[derive(Clone, PartialEq, Debug)]
pub struct ThresholdViolation {
    pub case: String,
    pub measured_per_iter_ms: f64,
    pub per_iter_ms_max: f64,
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

pub fn v1_pipeline_thresholds() -> &'static [V1PipelineThreshold] {
    V1_PIPELINE_THRESHOLDS
}

pub fn v1_pipeline_per_iter_ms_limit(case: &str) -> Option<f64> {
    V1_PIPELINE_THRESHOLDS
        .iter()
        .find(|threshold| threshold.case == case)
        .map(|threshold| threshold.per_iter_ms_max)
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
