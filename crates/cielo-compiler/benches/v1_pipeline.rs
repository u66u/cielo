use std::hint::black_box;
use std::time::{Duration, Instant};

use cielo::{Compiler, CompilerConfig};
use cielo_base::SourceId;

const SOURCE_EXAMPLE: &str = include_str!("../examples/v1_test.cielo");
const SOURCE_DIRECT_RESUME: &str = r#"
effect LocalState { fn tick() -> Int }
fn main() -> Int {
  let x = handle { do LocalState.tick(); 9 } with LocalState {
    | tick(resume) => resume(41)
  };
  x
}
"#;
const SOURCE_CONTROL_RESUME: &str = r#"
effect LocalState { fn tick() -> Int }
fn main() -> Int {
  let x = handle { do LocalState.tick(); 9 } with LocalState {
    | tick(resume) => {
      let y = resume(41);
      y + 1
    }
  };
  x
}
"#;
const SOURCE_MIXED_HANDLER: &str = r#"
effect LocalState {
  fn tick() -> Int
  fn bump(flag: Bool) -> Int
}

fn main() -> Int {
  let x = handle {
    do LocalState.tick();
    do LocalState.bump(true);
    5
  } with LocalState {
    | tick(resume) => resume(40)
    | bump(flag, resume) => {
      if flag {
        let y = resume(1);
        y + 1
      } else {
        resume(2)
      }
    }
  };
  x
}
"#;

const DEFAULT_WARMUP_ITERS: usize = 5;
const DEFAULT_MEASURE_ITERS: usize = 25;

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

#[derive(Clone, Copy)]
struct BenchCase {
    name: &'static str,
    source: &'static str,
}

const CASES: &[BenchCase] = &[
    BenchCase {
        name: "example",
        source: SOURCE_EXAMPLE,
    },
    BenchCase {
        name: "direct_resume",
        source: SOURCE_DIRECT_RESUME,
    },
    BenchCase {
        name: "control_resume",
        source: SOURCE_CONTROL_RESUME,
    },
    BenchCase {
        name: "mixed_handler",
        source: SOURCE_MIXED_HANDLER,
    },
];

fn compile_case(case: BenchCase) {
    let compiler = Compiler::new(CompilerConfig::default());
    let source = compiler.source(case.source, SourceId::from_u32(0));
    let staged = compiler.staged(source);
    assert!(
        !staged.staged.diagnostics().has_errors(),
        "benchmark source `{}` should compile without diagnostics errors",
        case.name
    );
    black_box(staged.staged.program().functions().len());
}

#[derive(Clone, Copy, Debug, Default)]
struct BenchAccum {
    wall: Duration,
}

fn run_iterations(case: BenchCase, iterations: usize) -> BenchAccum {
    let start = Instant::now();
    for _ in 0..iterations {
        compile_case(case);
    }
    BenchAccum {
        wall: start.elapsed(),
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn env_bool(name: &str, default: bool) -> bool {
    std::env::var(name)
        .ok()
        .map(|raw| raw.trim().to_ascii_lowercase())
        .map(|raw| match raw.as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => default,
        })
        .unwrap_or(default)
}

fn emit_case(case: BenchCase, warmup_iters: usize, measure_iters: usize, enforce_thresholds: bool) {
    let _ = run_iterations(case, warmup_iters);
    let measured = run_iterations(case, measure_iters);
    let elapsed = measured.wall;
    let per_iter = elapsed / (measure_iters as u32);
    let per_iter_ms = per_iter.as_secs_f64() * 1_000.0;

    println!("benchmark=v1_pipeline");
    println!("case={}", case.name);
    println!("warmup_iterations={warmup_iters}");
    println!("iterations={measure_iters}");
    println!("total_ms={:.3}", elapsed.as_secs_f64() * 1_000.0);
    println!("per_iter_ms={per_iter_ms:.3}");
    if enforce_thresholds
        && let Err(violation) = check_v1_pipeline_per_iter_ms(case.name, per_iter_ms)
    {
        panic!(
            "benchmark case `{}` exceeded threshold: measured {:.3}ms/iter > limit {:.3}ms/iter",
            violation.case, violation.measured_per_iter_ms, violation.per_iter_ms_max
        );
    }
}

fn main() {
    let warmup_iters = env_usize("CIELO_BENCH_WARMUP_ITERS", DEFAULT_WARMUP_ITERS);
    let measure_iters = env_usize("CIELO_BENCH_MEASURE_ITERS", DEFAULT_MEASURE_ITERS);
    let enforce_thresholds = env_bool("CIELO_BENCH_ENFORCE_THRESHOLDS", false);
    for case in CASES {
        emit_case(*case, warmup_iters, measure_iters, enforce_thresholds);
    }
}
