use std::hint::black_box;
use std::time::{Duration, Instant};

use cielo::common::ids::SourceId;
use cielo::common::symbols::Interner;
use cielo::{Compiler, CompilerConfig};

const SOURCE: &str = include_str!("../examples/v1_test.cielo");
const WARMUP_ITERS: usize = 5;
const MEASURE_ITERS: usize = 25;

fn compile_v1_example() {
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source_v0(SOURCE, SourceId::from_u32(0), &mut interner);
    black_box(residual.program().functions().len());
}

fn run_iterations(iterations: usize) -> Duration {
    let start = Instant::now();
    for _ in 0..iterations {
        compile_v1_example();
    }
    start.elapsed()
}

fn main() {
    let _ = run_iterations(WARMUP_ITERS);
    let elapsed = run_iterations(MEASURE_ITERS);
    let per_iter = elapsed / (MEASURE_ITERS as u32);

    println!("benchmark=v1_pipeline_skeleton");
    println!("iterations={MEASURE_ITERS}");
    println!("total_ms={:.3}", elapsed.as_secs_f64() * 1_000.0);
    println!("per_iter_ms={:.3}", per_iter.as_secs_f64() * 1_000.0);
}
