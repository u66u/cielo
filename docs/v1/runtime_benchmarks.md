# V1 Runtime Benchmarks

Tracks runtime execution speed for currently active v1 effect paths.

## Run Protocol

- Command: `cargo bench --bench v1_runtime`
- Warmup runs per case: `5` (default `CIELO_RUNTIME_BENCH_WARMUP_RUNS`)
- Measured runs per case: `25` (default `CIELO_RUNTIME_BENCH_MEASURE_RUNS`)
- Threshold gate (optional): `CIELO_RUNTIME_BENCH_ENFORCE_THRESHOLDS=1`

Host snapshot:

- Timestamp source: latest commit time (`git show -s --format=%cI HEAD`)
- Timestamp value: `2026-01-23T15:55:44+03:00` (`5b3a376`)

## Baseline Results

Raw command output:

```text
benchmark=v1_runtime
case=pure_runtime_loop
warmup_runs=5
runs=25
total_ms=34.912
per_run_ms=1.396
benchmark=v1_runtime
case=unhandled_perform_loop
warmup_runs=5
runs=25
total_ms=35.004
per_run_ms=1.400
benchmark=v1_runtime
case=direct_handled_loop
warmup_runs=5
runs=25
total_ms=32.849
per_run_ms=1.314
benchmark=v1_runtime
case=control_handled_loop
warmup_runs=5
runs=25
total_ms=32.513
per_run_ms=1.301
relative_to_pure=pure_runtime_loop 1.000
relative_to_pure=unhandled_perform_loop 1.003
relative_to_pure=direct_handled_loop 0.941
relative_to_pure=control_handled_loop 0.931
```

Summary:

| Case | Total ms (25 runs) | Per-run ms | Relative to pure |
| --- | ---: | ---: | ---: |
| `pure_runtime_loop` | 34.912 | 1.396 | 1.000 |
| `unhandled_perform_loop` | 35.004 | 1.400 | 1.003 |
| `direct_handled_loop` | 32.849 | 1.314 | 0.941 |
| `control_handled_loop` | 32.513 | 1.301 | 0.931 |

Runtime thresholds:

| Case | Max per-run ms | Max relative to pure |
| --- | ---: | ---: |
| `pure_runtime_loop` | 2.500 | n/a |
| `unhandled_perform_loop` | 2.500 | 1.200 |
| `direct_handled_loop` | 2.500 | 1.200 |
| `control_handled_loop` | 2.500 | 1.200 |
