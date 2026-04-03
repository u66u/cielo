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

fn consume(v: Boxed) -> Int {
  let one = 1;
  let two = one + 1;
  let three = two + 1;
  let four = three + 1;
  let five = four + 1;
  match v {
    Wrap(n) => n + five,
  }
}

fn run_batch(batch: Int, n: Int, acc: Int) -> Int {
  if batch == 0 {
    acc
  } else {
    let x = Wrap(n);
    let iter = consume(x);
    run_batch(batch - 1, n - 1, acc + iter)
  }
}

fn main() -> Int {
  let n = @runtime { 4000 };
  let batch = @runtime { 300 };
  let s = run_batch(batch, n, 0);
  s
}
"#;

const SOURCE_ALIAS_CHURN: &str = r#"
enum Pair { Pair(Int, Int) }

fn consume(v: Pair, bias: Int) -> Int {
  let one = 1;
  let two = one + 1;
  match v {
    Pair(x, y) => x + y + two + bias,
  }
}

fn churn_once(n: Int, acc: Int) -> Int {
  if n == 0 {
    acc
  } else {
    let p = Pair(n, acc);
    let a = p;
    let b = a;
    let next = consume(b, 0);
    churn_once(n - 1, next)
  }
}

fn run_batch(batch: Int, n: Int, acc: Int) -> Int {
  if batch == 0 {
    acc
  } else {
    let iter = churn_once(n, 0);
    run_batch(batch - 1, n, acc + iter)
  }
}

fn main() -> Int {
  let n = @runtime { 2500 };
  let batch = @runtime { 120 };
  let s = run_batch(batch, n, 0);
  s
}
"#;

const SOURCE_BRANCH_CHURN: &str = r#"
enum Boxed { Wrap(Int) }

fn consume(v: Boxed, dir: Bool, acc: Int) -> Int {
  let one = 1;
  let two = one + 1;
  let three = two + 1;
  match v {
    Wrap(n) => if dir { acc + n + three } else { acc - n - three },
  }
}

fn churn_once(n: Int, acc: Int) -> Int {
  if n == 0 {
    acc
  } else {
    let x = Wrap(n);
    let dir = n == 1;
    let next = consume(x, dir, acc);
    churn_once(n - 1, next)
  }
}

fn run_batch(batch: Int, n: Int, acc: Int) -> Int {
  if batch == 0 {
    acc
  } else {
    let iter = churn_once(n, 0);
    run_batch(batch - 1, n, acc + iter)
  }
}

fn main() -> Int {
  let n = @runtime { 3000 };
  let batch = @runtime { 90 };
  let s = run_batch(batch, n, 0);
  s
}
"#;

const SOURCE_SINK_COPY_MOVE_CHURN: &str = r#"
enum Boxed { Wrap(Int) }
enum PairArg { PairArg(Boxed, Int) }

fn consume(v: Boxed) -> Int {
  let one = 1;
  let two = one + 1;
  match v {
    Wrap(n) => n + two,
  }
}

fn mix(p: PairArg, n: Int) -> Int {
  match p {
    PairArg(kept, bias) => n + bias,
  }
}

fn mix_rev(n: Int, p: PairArg) -> Int {
  mix(p, n)
}

fn churn_once(n: Int, acc: Int) -> Int {
  if n == 0 {
    acc
  } else {
    let x = Wrap(n);
    let ok = mix(PairArg(x, 1), consume(x));

    let y = Wrap(n + 1);
    let blocked = mix_rev(consume(y), PairArg(y, 2));

    let z = Wrap(n + 2);
    let dup_left = consume(z);
    let dup_right = consume(z);
    let dup = dup_left + dup_right;

    let a = Wrap(n + 3);
    let b = a;
    let c = b;
    let alias_tail = consume(c);

    let step = ok + blocked + dup + alias_tail + acc;
    churn_once(n - 1, step)
  }
}

fn run_batch(batch: Int, n: Int, acc: Int) -> Int {
  if batch == 0 {
    acc
  } else {
    let iter = churn_once(n, 0);
    run_batch(batch - 1, n, acc + iter)
  }
}

fn main() -> Int {
  let n = @runtime { 500 };
  let batch = @runtime { 60 };
  let s = run_batch(batch, n, 0);
  s
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

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct GcOptimizerThreshold {
    pub case: &'static str,
    pub max_runtime_relative_to_arc_raw: f64,
}

#[derive(Clone, PartialEq, Debug)]
pub struct GcOptimizerViolation {
    pub case: String,
    pub measured_runtime_relative_to_arc_raw: f64,
    pub max_runtime_relative_to_arc_raw: f64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct GcBenchArcStats {
    pub planned_retain_ops: u32,
    pub planned_release_ops: u32,
    pub final_retain_ops: u32,
    pub final_release_ops: u32,
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
    GcOverheadThreshold {
        case: "sink_copy_move_churn",
        preset: "arc_raw",
        max_runtime_relative_to_off: 4.500,
    },
    GcOverheadThreshold {
        case: "sink_copy_move_churn",
        preset: "arc_optimized",
        max_runtime_relative_to_off: 3.500,
    },
];

const GC_OPTIMIZER_THRESHOLDS: &[GcOptimizerThreshold] = &[
    GcOptimizerThreshold {
        case: "ctor_churn",
        max_runtime_relative_to_arc_raw: 1.200,
    },
    GcOptimizerThreshold {
        case: "alias_churn",
        max_runtime_relative_to_arc_raw: 1.400,
    },
    GcOptimizerThreshold {
        case: "branch_churn",
        max_runtime_relative_to_arc_raw: 1.200,
    },
    GcOptimizerThreshold {
        case: "sink_copy_move_churn",
        max_runtime_relative_to_arc_raw: 1.250,
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

pub fn gc_optimizer_thresholds() -> &'static [GcOptimizerThreshold] {
    GC_OPTIMIZER_THRESHOLDS
}

pub fn gc_optimizer_relative_to_raw_limit(case: &str) -> Option<f64> {
    GC_OPTIMIZER_THRESHOLDS
        .iter()
        .find(|threshold| threshold.case == case)
        .map(|threshold| threshold.max_runtime_relative_to_arc_raw)
}

pub fn check_gc_optimizer_relative_to_raw(
    case: &str,
    measured_runtime_relative_to_arc_raw: f64,
) -> Result<(), GcOptimizerViolation> {
    let Some(limit) = gc_optimizer_relative_to_raw_limit(case) else {
        return Ok(());
    };
    if measured_runtime_relative_to_arc_raw <= limit {
        Ok(())
    } else {
        Err(GcOptimizerViolation {
            case: case.to_owned(),
            measured_runtime_relative_to_arc_raw,
            max_runtime_relative_to_arc_raw: limit,
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
    BenchCase {
        name: "sink_copy_move_churn",
        source: SOURCE_SINK_COPY_MOVE_CHURN,
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
    arc_stats: GcBenchArcStats,
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
    arc_stats: GcBenchArcStats,
}

pub fn gc_overhead_case_arc_stats(case: &str, preset: &str) -> Option<GcBenchArcStats> {
    let case = CASES.iter().copied().find(|entry| entry.name == case)?;
    let preset = PRESETS.iter().copied().find(|entry| entry.name == preset)?;
    let mut interner = Interner::new();
    let config = CompilerConfig::default().with_gc_preset(preset.preset);
    let compiler = Compiler::new(config);
    let compiled = compiler.compile_source_to_c(case.source, SourceId::from_u32(0), &mut interner);
    if compiled.residual.diagnostics().has_errors() {
        return None;
    }
    Some(extract_arc_stats(&compiled))
}

fn extract_arc_stats(compiled: &cielo::CompiledC) -> GcBenchArcStats {
    let arc_stats = compiled.memory.arc;
    GcBenchArcStats {
        planned_retain_ops: arc_stats.planned_retain_ops,
        planned_release_ops: arc_stats.planned_release_ops,
        final_retain_ops: arc_stats.final_retain_ops,
        final_release_ops: arc_stats.final_release_ops,
    }
}

fn assert_preset_coverage(case: &str, preset: &str, arc_stats: GcBenchArcStats) {
    let planned_total = arc_stats
        .planned_retain_ops
        .saturating_add(arc_stats.planned_release_ops);
    let final_total = arc_stats
        .final_retain_ops
        .saturating_add(arc_stats.final_release_ops);
    if preset == "off" {
        assert_eq!(
            planned_total, 0,
            "gc overhead case `{case}` preset `off` should not plan ARC ops"
        );
        assert_eq!(
            final_total, 0,
            "gc overhead case `{case}` preset `off` should not emit ARC ops"
        );
        return;
    }
    assert!(
        planned_total > 0,
        "gc overhead case `{case}` preset `{preset}` should plan ARC ops to make overhead meaningful"
    );
    assert!(
        final_total > 0,
        "gc overhead case `{case}` preset `{preset}` should retain ARC ops after optimization"
    );
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
    let arc_stats = extract_arc_stats(&compiled);
    assert_preset_coverage(case.name, bench_preset.name, arc_stats);

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
        .arg("-O3")
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
        arc_stats,
    }
}

fn run_binary_once(path: &Path, case: &str, preset: &str) {
    let status = Command::new(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("failed to launch gc overhead benchmark binary");
    assert!(
        status.code().is_some(),
        "gc overhead benchmark binary terminated by signal for case `{case}` preset `{preset}`"
    );
}

fn run_binary_iterations(path: &Path, case: &str, preset: &str, iterations: usize) -> f64 {
    let start = Instant::now();
    for _ in 0..iterations {
        run_binary_once(path, case, preset);
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
        let _ = run_binary_iterations(
            target.bin_path.as_path(),
            target.case,
            target.preset,
            warmup_runs,
        );
        let runtime_per_run_ms = run_binary_iterations(
            target.bin_path.as_path(),
            target.case,
            target.preset,
            measure_runs,
        );
        measurements.push(Measurement {
            case: target.case,
            preset: target.preset,
            compile_source_ms: target.compile_source_ms,
            c_compile_ms: target.c_compile_ms,
            runtime_per_run_ms,
            arc_stats: target.arc_stats,
        });
    }

    let mut off_runtime_by_case = HashMap::new();
    let mut arc_raw_runtime_by_case = HashMap::new();
    for row in &measurements {
        if row.preset == "off" {
            off_runtime_by_case.insert(row.case, row.runtime_per_run_ms);
        } else if row.preset == "arc_raw" {
            arc_raw_runtime_by_case.insert(row.case, row.runtime_per_run_ms);
        }
    }

    for row in &measurements {
        let off_runtime_ms = off_runtime_by_case.get(row.case).copied().unwrap_or(1.0);
        let relative_to_off = if off_runtime_ms > 0.0 {
            row.runtime_per_run_ms / off_runtime_ms
        } else {
            1.0
        };
        let arc_raw_runtime_ms = arc_raw_runtime_by_case
            .get(row.case)
            .copied()
            .unwrap_or(row.runtime_per_run_ms);
        let relative_to_arc_raw = if arc_raw_runtime_ms > 0.0 {
            row.runtime_per_run_ms / arc_raw_runtime_ms
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
        println!("runtime_relative_to_arc_raw={:.3}", relative_to_arc_raw);
        println!(
            "arc_planned_retain_ops={}",
            row.arc_stats.planned_retain_ops
        );
        println!(
            "arc_planned_release_ops={}",
            row.arc_stats.planned_release_ops
        );
        println!("arc_final_retain_ops={}", row.arc_stats.final_retain_ops);
        println!("arc_final_release_ops={}", row.arc_stats.final_release_ops);

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
            if row.preset == "arc_optimized"
                && let Err(violation) =
                    check_gc_optimizer_relative_to_raw(row.case, relative_to_arc_raw)
            {
                panic!(
                    "gc optimizer threshold exceeded for case `{}`: optimized/raw measured {:.3} > limit {:.3}",
                    violation.case,
                    violation.measured_runtime_relative_to_arc_raw,
                    violation.max_runtime_relative_to_arc_raw
                );
            }
        }
    }
}
