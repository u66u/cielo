# V1 Pipeline Benchmarks

## Run Protocol

- Command: `cargo bench --bench v1_pipeline`
- Warmup iterations per case: `5` (default `CIELO_BENCH_WARMUP_ITERS`)
- Measured iterations per case: `25` (default `CIELO_BENCH_MEASURE_ITERS`)
- Threshold gate (optional): `CIELO_BENCH_ENFORCE_THRESHOLDS=1`
- CI trend/regression workflow: `.github/workflows/v1-bench.yml`
- Harness mode: custom `harness = false` benchmark executable

Host snapshot:

- Timestamp source: latest commit time (`git show -s --format=%cI HEAD`)
- Timestamp value: `2026-01-23T08:52:46+03:00` (`55203cf`)
- Toolchain: `rustc 1.93.0 (254b59607 2026-01-19)`
- Kernel/arch: `Linux 6.17.5-arch1-1 x86_64 GNU/Linux`

## Baseline Results

Raw command output:

```text
benchmark=v1_pipeline
case=example
warmup_iterations=5
iterations=25
total_ms=2.284
per_iter_ms=0.091
phase_parse_ms=0.015
phase_lower_ms=0.007
phase_typecheck_ms=0.008
phase_monomorphize_ms=0.000
phase_ct_propagate_ms=0.001
phase_bta_ms=0.044
phase_residualize_ms=0.012
benchmark=v1_pipeline
case=direct_resume
warmup_iterations=5
iterations=25
total_ms=0.430
per_iter_ms=0.017
phase_parse_ms=0.005
phase_lower_ms=0.001
phase_typecheck_ms=0.002
phase_monomorphize_ms=0.000
phase_ct_propagate_ms=0.000
phase_bta_ms=0.004
phase_residualize_ms=0.002
benchmark=v1_pipeline
case=control_resume
warmup_iterations=5
iterations=25
total_ms=0.460
per_iter_ms=0.018
phase_parse_ms=0.005
phase_lower_ms=0.001
phase_typecheck_ms=0.002
phase_monomorphize_ms=0.000
phase_ct_propagate_ms=0.000
phase_bta_ms=0.005
phase_residualize_ms=0.002
benchmark=v1_pipeline
case=mixed_handler
warmup_iterations=5
iterations=25
total_ms=1.129
per_iter_ms=0.045
phase_parse_ms=0.008
phase_lower_ms=0.003
phase_typecheck_ms=0.005
phase_monomorphize_ms=0.000
phase_ct_propagate_ms=0.001
phase_bta_ms=0.022
phase_residualize_ms=0.004
```

Wall-clock summary:

| Case | Total ms (25 iters) | Per-iter ms |
| --- | ---: | ---: |
| `example` | 2.284 | 0.091 |
| `direct_resume` | 0.430 | 0.017 |
| `control_resume` | 0.460 | 0.018 |
| `mixed_handler` | 1.129 | 0.045 |

Per-phase summary (ms per iteration):

| Case | Parse | Lower | Typecheck | Mono | CT | BTA | Residualize |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `example` | 0.015 | 0.007 | 0.008 | 0.000 | 0.001 | 0.044 | 0.012 |
| `direct_resume` | 0.005 | 0.001 | 0.002 | 0.000 | 0.000 | 0.004 | 0.002 |
| `control_resume` | 0.005 | 0.001 | 0.002 | 0.000 | 0.000 | 0.005 | 0.002 |
| `mixed_handler` | 0.008 | 0.003 | 0.005 | 0.000 | 0.001 | 0.022 | 0.004 |
