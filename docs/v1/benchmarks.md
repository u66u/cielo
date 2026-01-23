- Command: `cargo bench --bench v1_pipeline`
- Warmup iterations per case: `5` (default `CIELO_BENCH_WARMUP_ITERS`)
- Measured iterations per case: `25` (default `CIELO_BENCH_MEASURE_ITERS`)
- Harness mode: custom `harness = false` benchmark executable

- Timestamp: `2026-01-19 13:06:38 UTC`
- Toolchain: `rustc 1.93.0 (254b59607 2026-01-19)`
- Kernel/arch: `Linux 6.17.5-arch1-1 x86_64 GNU/Linux`

```text
benchmark=v1_pipeline
case=example
warmup_iterations=5
iterations=25
total_ms=1.433
per_iter_ms=0.057
benchmark=v1_pipeline
case=direct_resume
warmup_iterations=5
iterations=25
total_ms=0.319
per_iter_ms=0.013
benchmark=v1_pipeline
case=control_resume
warmup_iterations=5
iterations=25
total_ms=0.298
per_iter_ms=0.012
benchmark=v1_pipeline
case=mixed_handler
warmup_iterations=5
iterations=25
total_ms=0.726
per_iter_ms=0.029
```

| Case | Total ms (25 iters) | Per-iter ms |
| --- | ---: | ---: |
| `example` | 1.433 | 0.057 |
| `direct_resume` | 0.319 | 0.013 |
| `control_resume` | 0.298 | 0.012 |
| `mixed_handler` | 0.726 | 0.029 |

