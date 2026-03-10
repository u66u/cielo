# GC Overhead Comparison: Cielo vs Nim

This document defines the Nim equivalents for `benches/v1_gc_overhead.rs` and how to run apples-to-apples measurements against Cielo presets.

## Files

- Cielo benchmark: `benches/v1_gc_overhead.rs`
- Nim ARC benchmark: `benches/nim/gc_overhead_arc.nim`
- Nim value baseline (no `ref` RC traffic): `benches/nim/gc_overhead_value.nim`

Both Nim files support the same case flags:

- `-d:ctor_churn`
- `-d:alias_churn`
- `-d:branch_churn`

And runtime args:

1. `n` (default per case)
2. `batch` (default per case)
3. `runs` (default `1`)

## Preset mapping

| Cielo preset | Intent | Nim equivalent |
| --- | --- | --- |
| `off` | no ARC insertion/emission | `gc_overhead_value.nim` (value types) |
| `arc_raw` | ARC without Cielo ARC optimizer pass | closest available: `--mm:arc --opt:none` |
| `arc_optimized` | ARC with Cielo ARC optimizer pass | closest available: `--mm:arc -d:danger --opt:speed` |

Notes:

- Nim does not expose a direct "disable ARC semantic optimizer but keep ARC semantics" switch. The `arc_raw` mapping is an approximation.
- Nim `--mm:none` with `ref`-heavy code is not a drop-in equivalent to Cielo `off`; this is why the value baseline is separate.
- Cielo `off` in this benchmark does not release ctor allocations, so long-running cases can look slower than ARC due to heap growth. For optimizer comparisons, prefer `runtime_relative_to_arc_raw`.

## Build commands (Nim)

Example for `ctor_churn`:

```bash
mkdir -p target/nim

# Off-like baseline (no refcount traffic)
nim c -d:release --mm:none -d:ctor_churn \
  --nimcache:target/nimcache/ctor_value \
  -o:target/nim/ctor_value benches/nim/gc_overhead_value.nim

# ARC raw approximation
nim c -d:release --mm:arc --opt:none -d:ctor_churn \
  --nimcache:target/nimcache/ctor_arc_raw \
  -o:target/nim/ctor_arc_raw benches/nim/gc_overhead_arc.nim

# ARC optimized approximation
nim c -d:release --mm:arc -d:danger --opt:speed -d:ctor_churn \
  --nimcache:target/nimcache/ctor_arc_opt \
  -o:target/nim/ctor_arc_opt benches/nim/gc_overhead_arc.nim
```

Repeat for `alias_churn` and `branch_churn` by swapping `-d:ctor_churn`.

If `--mm:none` is unavailable in your Nim toolchain, use:

```bash
nim c -d:release --mm:arc -d:ctor_churn \
  -o:target/nim/ctor_value_fallback benches/nim/gc_overhead_value.nim
```

## Run commands (Nim)

Use the same defaults as Cielo first, then scale `runs` for stability.

```bash
target/nim/ctor_value 4000 300 1
target/nim/ctor_arc_raw 4000 300 1
target/nim/ctor_arc_opt 4000 300 1
```

Recommended measurement (if `hyperfine` is installed):

```bash
hyperfine --warmup 5 --runs 20 \
  -N \
  'target/nim/ctor_value 4000 300 5000' \
  'target/nim/ctor_arc_raw 4000 300 5000' \
  'target/nim/ctor_arc_opt 4000 300 5000'
```

## Cielo command

```bash
CIELO_GC_BENCH_WARMUP_RUNS=5 \
CIELO_GC_BENCH_MEASURE_RUNS=20 \
CIELO_GC_BENCH_ENFORCE_THRESHOLDS=1 \
cargo bench --bench v1_gc_overhead -- --nocapture
```

The Cielo benchmark output includes ARC op counters:

- `arc_planned_retain_ops`
- `arc_planned_release_ops`
- `arc_final_retain_ops`
- `arc_final_release_ops`

Use these to confirm each case is exercising ARC and not being optimized into a non-ARC path.

## Latest measured deltas (February 22, 2026)

Machine-local results from this repo state:

- Cielo command:
  - `CIELO_GC_BENCH_WARMUP_RUNS=5 CIELO_GC_BENCH_MEASURE_RUNS=25 CIELO_GC_BENCH_ENFORCE_THRESHOLDS=1 cargo bench --bench v1_gc_overhead -- --nocapture`
- Nim command pattern:
  - `hyperfine --warmup 3 --runs 12 -N '<value bin> ... 5000' '<arc_raw bin> ... 5000' '<arc_opt bin> ... 5000'`

| Case | Cielo `arc_raw/off` | Cielo `arc_optimized/arc_raw` | Nim `arc_raw/value` | Nim `arc_opt/arc_raw` |
| --- | ---: | ---: | ---: | ---: |
| `ctor_churn` | `0.901` | `1.018` | `33.58` | `0.126` |
| `alias_churn` | `0.424` | `1.382` | `29.14` | `0.126` |
| `branch_churn` | `0.394` | `0.998` | `25.28` | `0.163` |

Interpretation notes:

- Cielo `off` is not a no-allocation value baseline; it disables ARC insertion/emission, so ctor-heavy workloads accumulate unreleased heap objects and can run slower than ARC.
- The cleaner Cielo optimizer signal is `arc_optimized/arc_raw`.
- Nim `arc_opt/arc_raw` here includes backend optimizer differences (`--opt:none` vs `--opt:speed -d:danger`), so it is directional, not an exact semantic-isolation toggle.
