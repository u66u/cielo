# V1 Performance Thresholds

This document defines v1 benchmark thresholds used for regression checks.

- Timestamp source: latest commit time (not system clock)
- Snapshot command: `git show -s --format=%cI HEAD`
- Snapshot value: `2026-01-23T07:13:37+03:00`

## Bench Command

- Command: `cargo bench --bench v1_pipeline`
- Warmup iterations: `CIELO_BENCH_WARMUP_ITERS` (default `5`)
- Measure iterations: `CIELO_BENCH_MEASURE_ITERS` (default `25`)
- Threshold gate: `CIELO_BENCH_ENFORCE_THRESHOLDS=1`

## Per-Case Thresholds

| Case | Max per-iter ms |
| --- | ---: |
| `example` | 0.100 |
| `direct_resume` | 0.030 |
| `control_resume` | 0.030 |
| `mixed_handler` | 0.060 |

## Notes

- Threshold values include headroom above current v1 baselines.
- When refreshing thresholds, update the snapshot value using latest commit time.
- Do not use wall-clock timestamps for benchmark/document snapshot metadata.
