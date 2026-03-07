use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use cielo::common::ids::SourceId;
use cielo::common::symbols::Interner;
use cielo::{Compiler, CompilerConfig, GcPreset};

const SOURCE_CTOR_CHURN: &str = r#"
enum Boxed { Wrap(Int) }

fn churn(n: Int, acc: Int) -> Int {
  if n == 0 {
    acc
  } else {
    let x = Wrap(n);
    match x {
      Wrap(v) => churn(n - 1, acc + v),
    }
  }
}

fn main() -> Int {
  let n = @runtime { 4000 };
  let s = churn(n, 0);
  s - s
}
"#;

const SOURCE_ALIAS_CHURN: &str = r#"
enum Pair { Pair(Int, Int) }

fn churn(n: Int, acc: Int) -> Int {
  if n == 0 {
    acc
  } else {
    let p = Pair(n, acc);
    let a = p;
    let b = a;
    match b {
      Pair(x, y) => churn(n - 1, x + y),
    }
  }
}

fn main() -> Int {
  let n = @runtime { 2500 };
  let s = churn(n, 0);
  s - s
}
"#;

const SOURCE_BRANCH_CHURN: &str = r#"
enum Boxed { Wrap(Int) }

fn churn(n: Int, acc: Int) -> Int {
  if n == 0 {
    acc
  } else {
    let x = Wrap(n);
    if n == 1 {
      match x {
        Wrap(v) => churn(n - 1, acc + v),
      }
    } else {
      match x {
        Wrap(v) => churn(n - 1, acc - v),
      }
    }
  }
}

fn main() -> Int {
  let n = @runtime { 3000 };
  let s = churn(n, 0);
  s - s
}
"#;

const DEFAULT_WARMUP_RUNS: usize = 3;
const DEFAULT_MEASURE_RUNS: usize = 15;
static TEMP_SUFFIX_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct GcOverheadThreshold {
    pub case: &'static str,
    pub preset: &'static str,
    pub max_runtime_relative_to_off: f64,
}

#[derive(Clone, PartialEq, Debug)]
pub struct GcOverheadViolation {
    pub case: String,
    pub preset: String,
    pub measured_runtime_relative_to_off: f64,
    pub max_runtime_relative_to_off: f64,
}

const GC_OVERHEAD_THRESHOLDS: &[GcOverheadThreshold] = &[
    GcOverheadThreshold {
        case: "ctor_churn",
        preset: "arc_raw",
        max_runtime_relative_to_off: 4.000,
    },
    GcOverheadThreshold {
        case: "ctor_churn",
        preset: "arc_optimized",
        max_runtime_relative_to_off: 3.000,
    },
    GcOverheadThreshold {
        case: "alias_churn",
        preset: "arc_raw",
        max_runtime_relative_to_off: 4.000,
    },
    GcOverheadThreshold {
        case: "alias_churn",
        preset: "arc_optimized",
        max_runtime_relative_to_off: 3.000,
    },
    GcOverheadThreshold {
        case: "branch_churn",
        preset: "arc_raw",
        max_runtime_relative_to_off: 4.500,
    },
    GcOverheadThreshold {
        case: "branch_churn",
        preset: "arc_optimized",
        max_runtime_relative_to_off: 3.500,
    },
];

pub fn gc_overhead_thresholds() -> &'static [GcOverheadThreshold] {
    GC_OVERHEAD_THRESHOLDS
}

pub fn gc_overhead_relative_limit(case: &str, preset: &str) -> Option<f64> {
    GC_OVERHEAD_THRESHOLDS
        .iter()
        .find(|threshold| threshold.case == case && threshold.preset == preset)
        .map(|threshold| threshold.max_runtime_relative_to_off)
}

pub fn check_gc_overhead_relative(
    case: &str,
    preset: &str,
    measured_runtime_relative_to_off: f64,
) -> Result<(), GcOverheadViolation> {
    let Some(limit) = gc_overhead_relative_limit(case, preset) else {
        return Ok(());
    };
    if measured_runtime_relative_to_off <= limit {
        Ok(())
    } else {
        Err(GcOverheadViolation {
            case: case.to_owned(),
            preset: preset.to_owned(),
            measured_runtime_relative_to_off,
            max_runtime_relative_to_off: limit,
        })
    }
}

#[derive(Clone, Copy)]
struct BenchCase {
    name: &'static str,
    source: &'static str,
}

#[derive(Clone, Copy)]
struct BenchPreset {
    name: &'static str,
    preset: GcPreset,
}

const CASES: &[BenchCase] = &[
    BenchCase {
        name: "ctor_churn",
        source: SOURCE_CTOR_CHURN,
    },
    BenchCase {
        name: "alias_churn",
        source: SOURCE_ALIAS_CHURN,
    },
    BenchCase {
        name: "branch_churn",
        source: SOURCE_BRANCH_CHURN,
    },
];

const PRESETS: &[BenchPreset] = &[
    BenchPreset {
        name: "off",
        preset: GcPreset::Off,
    },
    BenchPreset {
        name: "arc_raw",
        preset: GcPreset::ArcBenchRaw,
    },
    BenchPreset {
        name: "arc_optimized",
        preset: GcPreset::ArcBenchOptimized,
    },
];

#[derive(Clone, Debug)]
struct BuiltTarget {
    case: &'static str,
    preset: &'static str,
    bin_path: PathBuf,
    work_dir: PathBuf,
    compile_source_ms: f64,
    c_compile_ms: f64,
}

impl Drop for BuiltTarget {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.work_dir.as_path());
    }
}

#[derive(Clone, Debug)]
struct Measurement {
    case: &'static str,
    preset: &'static str,
    compile_source_ms: f64,
    c_compile_ms: f64,
    runtime_per_run_ms: f64,
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

fn build_target(case: BenchCase, bench_preset: BenchPreset, source_id: u32) -> BuiltTarget {
    let compile_start = Instant::now();
    let mut interner = Interner::new();
    let config = CompilerConfig::default().with_gc_preset(bench_preset.preset);
    let compiler = Compiler::new(config);
    let compiled =
        compiler.compile_source_to_c(case.source, SourceId::from_u32(source_id), &mut interner);
    let compile_source_ms = compile_start.elapsed().as_secs_f64() * 1_000.0;
    assert!(
        !compiled.residual.diagnostics().has_errors(),
        "gc overhead bench source `{}` preset `{}` should compile without diagnostics errors",
        case.name,
        bench_preset.name
    );

    let stamp = TEMP_SUFFIX_COUNTER.fetch_add(1, Ordering::Relaxed);
    let work_dir = std::env::temp_dir().join(format!(
        "cielo_v1_gc_overhead_{}_{}_{}_{}",
        case.name,
        bench_preset.name,
        std::process::id(),
        stamp
    ));
    fs::create_dir_all(work_dir.as_path()).expect("failed to create gc overhead temp dir");

    let c_path = work_dir.join("case.c");
    let bin_path = work_dir.join("case.bin");
    fs::write(c_path.as_path(), compiled.c_source.as_bytes())
        .expect("failed to write gc overhead C file");

    let c_compile_start = Instant::now();
    let compile = Command::new(cc_command())
        .arg("-std=c11")
        .arg("-O2")
        .arg(c_path.as_path())
        .arg("-o")
        .arg(bin_path.as_path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to invoke C compiler for gc overhead bench");
    let c_compile_ms = c_compile_start.elapsed().as_secs_f64() * 1_000.0;
    if !compile.status.success() {
        panic!(
            "C compile failed for gc overhead bench case `{}` preset `{}`:\n{}",
            case.name,
            bench_preset.name,
            String::from_utf8_lossy(compile.stderr.as_slice())
        );
    }

    BuiltTarget {
        case: case.name,
        preset: bench_preset.name,
        bin_path,
        work_dir,
        compile_source_ms,
        c_compile_ms,
    }
}

fn run_binary_once(path: &Path) {
    let status = Command::new(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("failed to launch gc overhead benchmark binary");
    assert!(
        status.success(),
        "gc overhead benchmark binary exited with non-zero status: {:?}",
        status.code()
    );
}

fn run_binary_iterations(path: &Path, iterations: usize) -> f64 {
    let start = Instant::now();
    for _ in 0..iterations {
        run_binary_once(path);
    }
    start.elapsed().as_secs_f64() * 1_000.0 / iterations as f64
}

fn main() {
    let warmup_runs = env_usize("CIELO_GC_BENCH_WARMUP_RUNS", DEFAULT_WARMUP_RUNS);
    let measure_runs = env_usize("CIELO_GC_BENCH_MEASURE_RUNS", DEFAULT_MEASURE_RUNS);
    let enforce_thresholds = env_bool("CIELO_GC_BENCH_ENFORCE_THRESHOLDS", false);

    let mut source_id = 0u32;
    let mut built = Vec::new();
    for case in CASES {
        for preset in PRESETS {
            built.push(build_target(*case, *preset, source_id));
            source_id = source_id.saturating_add(1);
        }
    }

    let mut measurements = Vec::new();
    for target in &built {
        let _ = run_binary_iterations(target.bin_path.as_path(), warmup_runs);
        let runtime_per_run_ms = run_binary_iterations(target.bin_path.as_path(), measure_runs);
        measurements.push(Measurement {
            case: target.case,
            preset: target.preset,
            compile_source_ms: target.compile_source_ms,
            c_compile_ms: target.c_compile_ms,
            runtime_per_run_ms,
        });
    }

    let mut off_runtime_by_case = HashMap::new();
    for row in &measurements {
        if row.preset == "off" {
            off_runtime_by_case.insert(row.case, row.runtime_per_run_ms);
        }
    }

    for row in &measurements {
        let off_runtime_ms = off_runtime_by_case.get(row.case).copied().unwrap_or(1.0);
        let relative_to_off = if off_runtime_ms > 0.0 {
            row.runtime_per_run_ms / off_runtime_ms
        } else {
            1.0
        };
        println!("benchmark=v1_gc_overhead");
        println!("case={}", row.case);
        println!("preset={}", row.preset);
        println!("warmup_runs={warmup_runs}");
        println!("runs={measure_runs}");
        println!("compile_source_ms={:.3}", row.compile_source_ms);
        println!("c_compile_ms={:.3}", row.c_compile_ms);
        println!("runtime_per_run_ms={:.3}", row.runtime_per_run_ms);
        println!("runtime_relative_to_off={:.3}", relative_to_off);

        if enforce_thresholds && row.preset != "off" {
            if let Err(violation) =
                check_gc_overhead_relative(row.case, row.preset, relative_to_off)
            {
                panic!(
                    "gc overhead threshold exceeded for case `{}` preset `{}`: measured {:.3} > limit {:.3}",
                    violation.case,
                    violation.preset,
                    violation.measured_runtime_relative_to_off,
                    violation.max_runtime_relative_to_off
                );
            }
        }
    }
}
