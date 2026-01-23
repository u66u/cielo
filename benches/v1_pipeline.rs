use std::hint::black_box;
use std::time::{Duration, Instant};

use cielo::analysis::bench_thresholds::check_v1_pipeline_per_iter_ms;
use cielo::common::ids::SourceId;
use cielo::common::symbols::Interner;
use cielo::pipeline::compiler::V0PipelineTimings;
use cielo::{Compiler, CompilerConfig};

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

fn compile_case(case: BenchCase) -> V0PipelineTimings {
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let (residual, timings) =
        compiler.compile_source_v0_profiled(case.source, SourceId::from_u32(0), &mut interner);
    assert!(
        !residual.diagnostics().has_errors(),
        "benchmark source `{}` should compile without diagnostics errors",
        case.name
    );
    black_box(residual.program().functions().len());
    timings
}

#[derive(Clone, Copy, Debug, Default)]
struct BenchAccum {
    wall: Duration,
    stages: V0PipelineTimings,
}

fn run_iterations(case: BenchCase, iterations: usize) -> BenchAccum {
    let start = Instant::now();
    let mut stages = V0PipelineTimings::default();
    for _ in 0..iterations {
        let iteration = compile_case(case);
        stages.saturating_add_assign(iteration);
    }
    BenchAccum {
        wall: start.elapsed(),
        stages,
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

fn emit_case(
    case: BenchCase,
    warmup_iters: usize,
    measure_iters: usize,
    enforce_thresholds: bool,
) {
    let _ = run_iterations(case, warmup_iters);
    let measured = run_iterations(case, measure_iters);
    let elapsed = measured.wall;
    let per_iter = elapsed / (measure_iters as u32);
    let per_iter_ms = per_iter.as_secs_f64() * 1_000.0;
    let stage_per_iter = measured.stages.per_iteration(measure_iters as u32);

    println!("benchmark=v1_pipeline");
    println!("case={}", case.name);
    println!("warmup_iterations={warmup_iters}");
    println!("iterations={measure_iters}");
    println!("total_ms={:.3}", elapsed.as_secs_f64() * 1_000.0);
    println!("per_iter_ms={per_iter_ms:.3}");
    println!(
        "phase_parse_ms={:.3}",
        stage_per_iter.parse.as_secs_f64() * 1_000.0
    );
    println!(
        "phase_lower_ms={:.3}",
        stage_per_iter.lower.as_secs_f64() * 1_000.0
    );
    println!(
        "phase_typecheck_ms={:.3}",
        stage_per_iter.typecheck.as_secs_f64() * 1_000.0
    );
    println!(
        "phase_monomorphize_ms={:.3}",
        stage_per_iter.monomorphize.as_secs_f64() * 1_000.0
    );
    println!(
        "phase_ct_propagate_ms={:.3}",
        stage_per_iter.ct_propagate.as_secs_f64() * 1_000.0
    );
    println!(
        "phase_bta_ms={:.3}",
        stage_per_iter.bta.as_secs_f64() * 1_000.0
    );
    println!(
        "phase_residualize_ms={:.3}",
        stage_per_iter.residualize.as_secs_f64() * 1_000.0
    );

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
