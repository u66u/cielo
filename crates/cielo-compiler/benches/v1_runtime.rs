use std::ffi::OsString;
use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use cielo_base::Interner;
use cielo_base::SourceId;
use cielo_test_support::{PassConfig, PassHarness};

const SOURCE_PURE_RUNTIME_LOOP: &str = r#"
fn pure_loop(n: Int, acc: Int) -> Int {
  if n == 0 { acc } else { pure_loop(n - 1, acc + 1) }
}

fn main() -> Int {
  let n = @runtime { 8000 };
  let x = pure_loop(n, 0);
  x - x
}
"#;

const SOURCE_UNHANDLED_PERFORM_LOOP: &str = r#"
effect Tick { fn hit() -> Int }

fn unhandled_loop(n: Int, acc: Int) -> Int with Tick {
  if n == 0 {
    acc
  } else {
    do Tick.hit();
    unhandled_loop(n - 1, acc + 1)
  }
}

fn main() -> Int {
  let n = @runtime { 8000 };
  let x = unhandled_loop(n, 0);
  x - x
}
"#;

const SOURCE_DIRECT_HANDLED_LOOP: &str = r#"
effect Tick { fn hit() -> Int }

fn direct_loop(n: Int, acc: Int) -> Int with Tick {
  if n == 0 {
    acc
  } else {
    do Tick.hit();
    direct_loop(n - 1, acc + 1)
  }
}

fn main() -> Int {
  let n = @runtime { 8000 };
  let x = handle { direct_loop(n, 0) } with Tick {
    | hit(resume) => resume(0)
  };
  x - x
}
"#;

const SOURCE_CONTROL_HANDLED_LOOP: &str = r#"
effect Tick { fn hit() -> Int }

fn control_loop(n: Int, acc: Int) -> Int with Tick {
  if n == 0 {
    acc
  } else {
    do Tick.hit();
    control_loop(n - 1, acc + 1)
  }
}

fn main() -> Int {
  let n = @runtime { 8000 };
  let x = handle { control_loop(n, 0) } with Tick {
    | hit(resume) => {
      let y = resume(0);
      y + 1
    }
  };
  x - x
}
"#;

const SOURCE_NESTED_DIRECT_SAME_EFFECT_LOOP: &str = r#"
effect Tick { fn hit() -> Int }

fn nested_direct_loop(n: Int, acc: Int) -> Int with Tick {
  if n == 0 {
    acc
  } else {
    do Tick.hit();
    nested_direct_loop(n - 1, acc + 1)
  }
}

fn main() -> Int {
  let n = @runtime { 8000 };
  let x = handle {
    let y = handle { nested_direct_loop(n, 0) } with Tick {
      | hit(resume) => resume(0)
    };
    do Tick.hit();
    y
  } with Tick {
    | hit(resume) => resume(0)
  };
  x - x
}
"#;

const SOURCE_NESTED_CONTROL_SAME_EFFECT_LOOP: &str = r#"
effect Tick { fn hit() -> Int }

fn nested_control_loop(n: Int, acc: Int) -> Int with Tick {
  if n == 0 {
    acc
  } else {
    do Tick.hit();
    nested_control_loop(n - 1, acc + 1)
  }
}

fn main() -> Int {
  let n = @runtime { 8000 };
  let x = handle {
    let y = handle { nested_control_loop(n, 0) } with Tick {
      | hit(resume) => {
        let z = resume(0);
        z + 1
      }
    };
    do Tick.hit();
    y
  } with Tick {
    | hit(resume) => {
      let z = resume(0);
      z + 1
    }
  };
  x - x
}
"#;

const DEFAULT_WARMUP_RUNS: usize = 5;
const DEFAULT_MEASURE_RUNS: usize = 25;
static TEMP_SUFFIX_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct V1RuntimeThreshold {
    pub case: &'static str,
    pub per_run_ms_max: f64,
    pub relative_to_pure_max: Option<f64>,
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
    V1RuntimeThreshold {
        case: "nested_direct_same_effect_loop",
        per_run_ms_max: 3.000,
        relative_to_pure_max: Some(1.350),
    },
    V1RuntimeThreshold {
        case: "nested_control_same_effect_loop",
        per_run_ms_max: 3.000,
        relative_to_pure_max: Some(1.350),
    },
];

pub fn v1_runtime_thresholds() -> &'static [V1RuntimeThreshold] {
    V1_RUNTIME_THRESHOLDS
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

#[derive(Clone, Copy)]
struct RuntimeBenchCase {
    name: &'static str,
    source: &'static str,
}

const CASES: &[RuntimeBenchCase] = &[
    RuntimeBenchCase {
        name: "pure_runtime_loop",
        source: SOURCE_PURE_RUNTIME_LOOP,
    },
    RuntimeBenchCase {
        name: "unhandled_perform_loop",
        source: SOURCE_UNHANDLED_PERFORM_LOOP,
    },
    RuntimeBenchCase {
        name: "direct_handled_loop",
        source: SOURCE_DIRECT_HANDLED_LOOP,
    },
    RuntimeBenchCase {
        name: "control_handled_loop",
        source: SOURCE_CONTROL_HANDLED_LOOP,
    },
    RuntimeBenchCase {
        name: "nested_direct_same_effect_loop",
        source: SOURCE_NESTED_DIRECT_SAME_EFFECT_LOOP,
    },
    RuntimeBenchCase {
        name: "nested_control_same_effect_loop",
        source: SOURCE_NESTED_CONTROL_SAME_EFFECT_LOOP,
    },
];

#[derive(Debug)]
struct BuiltCase {
    name: &'static str,
    bin_path: PathBuf,
    work_dir: PathBuf,
}

impl Drop for BuiltCase {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.work_dir.as_path());
    }
}

fn cc_command() -> OsString {
    std::env::var_os("CC").unwrap_or_else(|| OsString::from("cc"))
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

fn build_case(case: RuntimeBenchCase, source_id: u32) -> BuiltCase {
    let mut interner = Interner::new();
    let compiler = PassHarness::new(PassConfig::default());
    let compiled =
        compiler.compile_source_v0_to_c(case.source, SourceId::from_u32(source_id), &mut interner);
    assert!(
        !compiled.residual.diagnostics().has_errors(),
        "runtime benchmark source `{}` should compile without diagnostics errors",
        case.name
    );

    let stamp = TEMP_SUFFIX_COUNTER.fetch_add(1, Ordering::Relaxed);
    let work_dir = std::env::temp_dir().join(format!(
        "cielo_v1_runtime_bench_{}_{}_{}",
        case.name,
        std::process::id(),
        stamp
    ));
    fs::create_dir_all(work_dir.as_path()).expect("failed to create runtime bench temp dir");

    let c_path = work_dir.join("case.c");
    let bin_path = work_dir.join("case.bin");
    fs::write(c_path.as_path(), compiled.c_source.as_bytes())
        .expect("failed to write runtime benchmark C file");

    let compile = Command::new(cc_command())
        .arg("-std=c11")
        .arg("-O3")
        .arg(c_path.as_path())
        .arg("-o")
        .arg(bin_path.as_path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to invoke C compiler for runtime benchmark");
    if !compile.status.success() {
        panic!(
            "C compile failed for runtime benchmark case `{}`:\n{}",
            case.name,
            String::from_utf8_lossy(compile.stderr.as_slice())
        );
    }

    BuiltCase {
        name: case.name,
        bin_path,
        work_dir,
    }
}

fn run_binary_once(path: &Path) {
    let status = Command::new(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("failed to launch runtime benchmark binary");
    assert!(
        status.success(),
        "runtime benchmark binary exited with non-zero status: {:?}",
        status.code()
    );
}

fn run_binary_iterations(path: &Path, iterations: usize) -> Duration {
    let start = Instant::now();
    for _ in 0..iterations {
        run_binary_once(path);
    }
    start.elapsed()
}

fn main() {
    let warmup_runs = env_usize("CIELO_RUNTIME_BENCH_WARMUP_RUNS", DEFAULT_WARMUP_RUNS);
    let measure_runs = env_usize("CIELO_RUNTIME_BENCH_MEASURE_RUNS", DEFAULT_MEASURE_RUNS);
    let enforce_thresholds = env_bool("CIELO_RUNTIME_BENCH_ENFORCE_THRESHOLDS", false);

    let built_cases = CASES
        .iter()
        .enumerate()
        .map(|(idx, case)| build_case(*case, idx as u32))
        .collect::<Vec<_>>();

    let mut results = Vec::with_capacity(built_cases.len());
    for built in &built_cases {
        let _ = run_binary_iterations(built.bin_path.as_path(), warmup_runs);
        let elapsed = run_binary_iterations(built.bin_path.as_path(), measure_runs);
        let per_run_ms = elapsed.as_secs_f64() * 1_000.0 / measure_runs as f64;

        println!("benchmark=v1_runtime");
        println!("case={}", built.name);
        println!("warmup_runs={warmup_runs}");
        println!("runs={measure_runs}");
        println!("total_ms={:.3}", elapsed.as_secs_f64() * 1_000.0);
        println!("per_run_ms={per_run_ms:.3}");
        if enforce_thresholds
            && let Err(violation) = check_v1_runtime_per_run_ms(built.name, per_run_ms)
        {
            panic!(
                "runtime benchmark case `{}` exceeded threshold: measured {:.3}ms/run > limit {:.3}ms/run",
                violation.case, violation.measured_per_run_ms, violation.per_run_ms_max
            );
        }

        results.push((built.name, per_run_ms));
        black_box(elapsed);
    }

    if let Some((_, pure_ms)) = results
        .iter()
        .find(|(name, _)| *name == "pure_runtime_loop")
    {
        for (name, per_run_ms) in &results {
            let relative = per_run_ms / pure_ms;
            println!("relative_to_pure={} {:.3}", name, relative);
            if enforce_thresholds
                && let Err(violation) = check_v1_runtime_relative_to_pure(name, relative)
            {
                panic!(
                    "runtime benchmark case `{}` exceeded relative threshold: measured {:.3} > limit {:.3}",
                    violation.case,
                    violation.measured_relative_to_pure,
                    violation.relative_to_pure_max
                );
            }
        }
    }
}
