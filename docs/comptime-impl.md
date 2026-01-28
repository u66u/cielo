# Comptime Implementation Plan

v1 active, v2 roadmap.

this mirrors `docs/effects-impl.md` but for the comptime/staging pipeline.

---

## Recommended Baseline

Use the fused conceptual model from `docs/v1/comptime_passes.md`:

1. Evaluate+Classify
2. Residualize+Specialize
3. Normalize

Implementation note:

- current code is still split into tactical modules (`ct_propagate`, `bta`, `residualize`,
  `handler_specialize`) and then linearization.
- we keep that split for iteration speed, but enforce fused-stage contracts at boundaries.

---

## Why This Wins

1. aligns with current architecture and phase typing (`src/pipeline/phases.rs`)
2. keeps v1 semantics conservative and debuggable
3. makes v2 upgrades explicit (query db, partial values, totality) without rewriting v1

---

## Dependencies Required To Begin

### Global prerequisites

1. Concrete effects assertion at pre-staging boundary (no unresolved effect labels/rows).
2. Stable phase structs with consumed transition boundaries.
3. Existing benchmark harness + staging diagnostics plumbing.
4. Deterministic cache keying for ct query snapshots and file deps.

### v1 prerequisites

1. monomorphization output is concrete for types/effects.
2. reason/provenance propagation through staging tables.
3. bounded specialization policy with id-remap integrity checks.

### v2 prerequisites

1. v1 stage contracts complete and stable.
2. query dependency graph hooks for incremental invalidation.
3. partial-value representation and residualization policy.

---

## Feature To Function Map (Paper -> Mechanism -> Code)

This is the source-of-truth checklist to detect plan/literature deviation.

### Evaluate+Classify

- [ ] `EC-1` outcome lattice + reason provenance
  Mechanism:
  - staging-by-evaluation style Known/Stuck classification
  Sources:
  - Kovacs 2022, 2024
  Functions:
  - `src/passes/ct_propagate.rs:run_with_query_cache`
  - `src/passes/bta.rs:run`
  TODO impl points:
  - unify surface entry under `evaluate_classify(...)`
  - add stage-boundary invariants that panic on compiler bugs

- [ ] `EC-2` ct file-dep invalidation discipline
  Mechanism:
  - deterministic dependency hashing + cache-key guards
  Sources:
  - Salsa query model (invalidation shape), rustc const-eval caching patterns
  Functions:
  - `src/pipeline/ct_query_cache.rs`
  - `src/passes/ct_propagate.rs:collect_file_deps`
  TODO impl points:
  - make invalidation reason reporting first-class in staging diagnostics

- [ ] `EC-3` resumptive control classification feeds lowering
  Mechanism:
  - clause discharge + reason fidelity carried to linearize boundary
  Sources:
  - Effekt references in `docs/v1/implementation_ref.md`
  Functions:
  - `src/passes/bta.rs:classify_handler_discharge`
  TODO impl points:
  - tighten pass-boundary checks to prevent stale discharge metadata

### Residualize+Specialize

- [ ] `RS-1` known-value embedding + persistability gate
  Mechanism:
  - residualize only when values are staged known and boundary-safe
  Sources:
  - Partial evaluation baseline (Jones/Gomard/Sestoft)
  Functions:
  - `src/passes/residualize.rs:apply_ct_residualization`
  TODO impl points:
  - enrich boundary diagnostics with role-level context everywhere

- [ ] `RS-2` bounded specialization + recursive retarget
  Mechanism:
  - static-argument style specialization with bounded key-space
  Sources:
  - Effekt static-arguments/Reachable references
  Functions:
  - `src/passes/handler_specialize.rs:run`
  - `src/passes/handler_specialize.rs:specialize_handle_wrapped_calls`
  TODO impl points:
  - report specialization counters in staging report

- [ ] `RS-3` remap integrity across side tables
  Mechanism:
  - compacted FuncId remap stays phase-consistent
  Sources:
  - phase-integrity policy in `docs/v1/decisions.md`
  Functions:
  - `src/passes/handler_specialize.rs:run`
  - `src/pipeline/phases.rs:*::remap_func_ids`
  TODO impl points:
  - add explicit invariants for every carried table

### Normalize

- [ ] `NZ-1` shrink rules to fixpoint
  Mechanism:
  - shrinking-only reductions, monotone size decrease
  Sources:
  - Appel & Jim 1997
  Functions:
  - target module: `src/passes/normalize.rs` (to add)
  TODO impl points:
  - implement shrink rules with usage gating and stmt-level safety checks

- [ ] `NZ-2` one-shot speculative inline
  Mechanism:
  - usage-gated inline between shrink phases
  Sources:
  - SML.NET shrinking/inline pattern (as cited in implementation refs)
  Functions:
  - target module: `src/passes/normalize.rs` (to add)
  TODO impl points:
  - inline thresholds + recursion guard + code-size guardrails

- [ ] `NZ-3` post-inline shrink + idempotence checks
  Mechanism:
  - shrink-inline-shrink sandwich closes growth loop
  Sources:
  - Appel & Jim + existing v1 decisions
  Functions:
  - target module: `src/passes/normalize.rs` (to add)
  TODO impl points:
  - idempotence/property tests against residual interpreter oracle

---

## v1 Implementation Plan (Active)

Priority order:

1. Stage A: fused Evaluate+Classify entrypoint + explicit contracts
2. Stage B: fused Residualize+Specialize entrypoint + metadata integrity
3. Stage C: introduce Normalize pass skeleton + first shrink rules
4. Stage D: diagnostics and provenance hardening
5. Stage E: perf gates + differential/metamorphic suite hardening

### V1-Stage A: Evaluate+Classify entrypoint

Depends on:

- v1 prerequisites #1 and #2

Deliverable:

- one explicit fused API for ct evaluation + staging classification
- unchanged semantics vs current split execution

DoD:

- fused and split execution produce equivalent emitted C on coverage corpus
- pass boundary asserts remain active at pre-staging input

Test targets:

- query cache hit/miss path equivalence
- ct file dep hash changes invalidate staged outcomes

### V1-Stage B: Residualize+Specialize entrypoint

Depends on:

- V1-Stage A

Deliverable:

- one explicit fused API for residualize + bounded specialize
- remap integrity checks on all downstream tables

DoD:

- recursive specialization retarget remains correct
- all remapped ids remain in bounds after pruning

Test targets:

- wrapper specialization recursion case
- varying return-clause case still aborts specialization in v1

### V1-Stage C: Normalize pass delivery

Depends on:

- V1-Stage B

Deliverable:

- `normalize` pass module integrated after residualize+specialize
- shrink-inline-shrink with conservative v1 subset

DoD:

- shrink phase never increases node count
- recursive functions never speculatively inlined

Test targets:

- dead binding elimination
- val/let commutation safety
- no semantic drift on differential runtime tests

### V1-Stage D: Diagnostics and provenance hardening

Depends on:

- V1-Stage A through C

Deliverable:

- stage-root-cause rollups with stable reason chains
- boundary diagnostics include exact boundary role metadata

DoD:

- staging report summarizes top RT causes deterministically
- provenance chains survive specialization remap

Test targets:

- ct-only call with rt args
- non-persistable boundary crossings

### V1-Stage E: Perf + regression gates

Depends on:

- V1-Stage A through D

Deliverable:

- thresholded pass-level timings and regression suite expansion
- metamorphic checks around staging invariants

DoD:

- no threshold regressions on representative corpus
- no staged/residual semantic divergence in seeded differential tests

Test targets:

- alpha-renaming invariance of staged outcomes
- dead-code insertion invariance for discardable contexts

---

## v2 Roadmap (Deferred, Not Active)

Priority order:

1. query architecture migration (salsa-style dependency tracking)
2. partially-static data (`Outcome::Partial`)
3. flow-sensitive mutable-state staging
4. per-clause partial discharge
5. totality checker for fuel bypass
6. background comptime execution model

### V2-Stage 1: Query architecture

Deliverable:

- query-db backbone for staging queries and invalidation

Primary references:

- Salsa docs + repo

### V2-Stage 2: Partial values

Deliverable:

- mixed known/stuck data propagation across constructors and field reads

Primary references:

- Yallop/von Glehn/Kammar 2018

### V2-Stage 3: Flow-sensitive mutable staging

Deliverable:

- program-point-sensitive variable outcome tracking with join semantics

Primary references:

- `docs/v2/comptime_additions.md`

### V2-Stage 4: Per-clause conditional discharge

Deliverable:

- conditional discharge status keyed by argument patterns

Primary references:

- Effekt selective control/residualization patterns

### V2-Stage 5: Totality + fuel bypass

Deliverable:

- proven-total functions bypass fuel limits

Primary references:

- sized-types literature listed in `docs/effects-impl.md`

### V2-Stage 6: Background comptime

Deliverable:

- async comptime blocks with cached promotion from RT fallback to CT

Primary references:

- query/invalidation model from V2-Stage 1

---

## Oracle Cookbook

For each stage change:

1. run reference staging evaluator (split modules are current oracle)
2. run fused stage entrypoint
3. compare normalized residual outputs (and emitted C where appropriate)
4. run metamorphic perturbations:
   - alpha renaming
   - dead-code insertion (discardable-only)
   - equivalent control-flow reshaping

---

## References

Staging and partial evaluation:

1. Jones, Gomard, Sestoft. *Partial Evaluation and Automatic Program Generation*.
   https://www.itu.dk/~sestoft/pebook/pebook.html
2. Kovacs. *Staged Compilation with Two-Level Type Theory* (ICFP 2022).
   https://doi.org/10.1145/3547641
3. Kovacs. *Closure-Free Functional Programming in a Two-Level Type Theory* (ICFP 2024).
   https://doi.org/10.1145/3674648

Normalization:

4. Appel, Jim. *Shrinking Lambda Expressions in Linear Time*.
   https://doi.org/10.1017/S0956796897002839

Query/incrementality:

5. Salsa overview.
   https://salsa-rs.github.io/salsa/how_salsa_works.html
6. Salsa repository.
   https://github.com/salsa-rs/salsa

Partially-static data:

7. Yallop, von Glehn, Kammar. *Partially-static data as free extension of algebras*.
   https://doi.org/10.1145/3236795

Implementation references:

8. `docs/v1/comptime_passes.md`
9. `docs/v1/implementation_ref.md`
10. `docs/v2/comptime_additions.md`
