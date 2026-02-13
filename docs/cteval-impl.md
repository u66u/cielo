### CTE-v2 (deferred)

1. `Outcome::Partial` and partial residualization support.
2. Query-db architecture migration (Salsa-style dependencies).
3. Flow-sensitive mutable-state staging.

## File Map

Planned code ownership:

1. `src/passes/ct_eval.rs` - evaluator engine + Stage A execution logic.
2. `src/passes/comptime.rs` - fused entrypoint wiring + boundary assertions.
3. `src/pipeline/compiler.rs` - pipeline calls switched to evaluator entrypoint.
4. `src/pipeline/phases.rs` - phase data structure evolution (if needed).
5. `tests/ct_eval.rs` - evaluator behavior unit/integration tests.
6. `tests/comptime_invariants.rs` - stage-boundary invariant assertions.
7. `tests/ct_query_cache.rs` - cache parity + invalidation matrix.

## Test Plan (Required)

### Evaluator Semantics

1. Pure known-arg function call folds to literal.
2. Recursive pure call respects fuel/recursion bound and remains stuck.
3. Effectful callee remains runtime-stuck with effect reason.
4. Stage directives force stage as specified.

### Invariants

1. Stage tables and knownness tables are full coverage.
2. Out-of-bounds ids in carried tables never appear.
3. Branch decision consistency with folded booleans is enforced.
4. Handler discharge tables match handler arena arity.

### Cache

1. cold miss computes values.
2. warm hit restores values and avoids reevaluation.
3. dep-content change invalidates.
4. target/policy/fingerprint divergence invalidates deterministically.

## Acceptance Criteria (v1 Stage A)

1. CT evaluator is the canonical Stage A entrypoint in compiler pipeline.
2. Existing v1 test suite passes.
3. New evaluator tests pass and encode stage invariants explicitly.
4. Fused Stage A and split pipeline remain parity-equivalent on regression corpus.

## Open Questions (tracked, not blocking CTE-1)

1. Exact fuel defaults and user configurability surface.
2. Extent of v1 constructor known-value support before `Outcome::Partial`.
3. Whether to expose evaluator-level metrics counters beyond existing stats struct.

