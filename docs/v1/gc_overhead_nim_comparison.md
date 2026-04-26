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
| `arc_raw` | CFG ARC with explicit transfer retain/release pairs | closest available: `--mm:arc --opt:none` |
| `arc_optimized` | CFG ARC with last-use sink and moved-field elimination | closest available: `--mm:arc -d:danger --opt:speed` |

Notes:

- Nim does not expose a direct "disable ARC semantic optimizer but keep ARC semantics" switch. The `arc_raw` mapping is an approximation.
- In Cielo both presets use the same CFG ownership planner. `arc_raw` materializes the
  transfer pairs; `arc_optimized` eliminates provably redundant pairs into moves. This
  isolates the ownership optimization without switching backends.
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

## Withdrawn: the February 22, 2026 wall-clock table

That table is gone. Every column measured something other than what it claimed,
and it invited the reading "Nim's ARC optimizer gives 8x, Cielo's gives 1x",
which nothing here supports.

- **Nim `arc_opt/arc_raw = 0.126` was `gcc -O0` vs `gcc -O3`, not Nim's ARC
  optimizer.** The build commands above are `--opt:none` vs
  `-d:danger --opt:speed`. The generated C is the same apart from overflow
  calls, with the same ARC call sites in both. `-O3` alone accounts for ~7.8x.
  The value baseline shows a *larger* `-O0` penalty, so the effect is not
  ARC-specific at all.
- **Cielo `ctor_churn` measured `fork`/`exec`.** The harness timed
  `Command::status()` while the case ran 300 allocations once. An empty
  `int main(){return 0;}` costs ~648 us; the ctor binaries measured 681-695 us,
  with a standard deviation of ~210 us. The Nim side of the same row ran 5000
  iterations.
- **`arc_raw/off < 1` was a page-fault artifact.** `off` never frees, so it pays
  kernel time for a growing heap. Split by user vs system time, ARC is ~1.35x
  slower than `off` in user time — the expected direction.
- **The threshold gates could not fail.** `GC_OVERHEAD_THRESHOLDS` capped
  `arc_raw/off` at 4.0-4.5 against measurements of 0.39-1.02.
  `GC_OPTIMIZER_THRESHOLDS` allowed `alias_churn` at 1.400 against a measured
  1.382 — permitting the optimizer to make code 40% slower.

## What to measure instead

The result worth reporting is hardware-independent and already available: on
`alias_churn` the planner goes from 900k retains + 1.2M releases to **0 retains
and one destroy per object**, which is the same op count Nim's ARC reaches.

Prefer the runtime's own counters (`ctor_allocations`, `ctor_frees`,
`retain_calls`, `release_calls` in `cielo_runtime.h`) over wall clock. They are
exact, machine-independent, and state the claim directly. Note they require
`-DCIELO_ARC_STATS`; without it the counters compile out so the C compiler can
fold away provably-dead retain/release pairs.

Before any wall-clock comparison is republished:

- give `ctor_churn` an in-program loop so it stops measuring process startup;
- report user time, or bound the `off` working set;
- add a case that scales, since the current four are 6-37 lines;
- keep `-O2` on both sides.
