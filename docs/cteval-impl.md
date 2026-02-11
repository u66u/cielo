# CT Evaluator Implementation Plan

v1 active, v2 roadmap.

## Scope

This document specifies the value-level compile-time evaluator (CT evaluator)
that powers Stage A (`Evaluate+Classify`) in the v1 pipeline.

It is aligned with:

- `docs/comptime-impl.md`
- `docs/v1/comptime_passes.md`
- `docs/v1/implementation_ref.md`

This document is implementation-facing and decision-complete for the evaluator
itself. Type-level macro/declaration synthesis (struct/impl/etc) remains a
separate typecheck-phase track and is not implemented inside this evaluator.

## Status

- Current codebase still contains tactical split passes (`ct_propagate` + `bta`).
- v1 target is a fused conceptual Stage A with explicit contracts at boundaries.
- This document defines the migration path from tactical split to a real
  evaluator while preserving existing v1/v2 phase boundaries.

## Goals

1. Replace expression-only constant folding with a real CT evaluator for v1.
2. Evaluate pure/CT-eligible function calls when arguments are known.
3. Produce stable staging outcomes (`Ct` vs `Rt(reason)`) with provenance.
4. Preserve deterministic invalidation and query-cache behavior.
5. Keep downstream Stage B/C/D/E contracts stable.

## Non-Goals (v1)

1. Full partial-value representation (`Outcome::Partial`) across composite data.
2. Full query graph migration (Salsa-style) beyond current key+deps snapshot flow.
3. Multi-shot continuation semantics in the evaluator.
4. Type-level declaration synthesis logic (this belongs to type checking/elab).

## Research + Reference Grounding

Primary references for this implementation:

1. Kovacs 2022/2024 (two-level/staged execution framing).
2. Jones/Gomard/Sestoft partial evaluation baseline.
3. Effekt implementation references captured in `docs/v1/implementation_ref.md`.
4. Query invalidation patterns from Salsa/rustc query systems.

Secondary pragmatic references (used to choose concrete engineering tradeoffs):

1. Lean 4 hygienic macro/elaboration model (for phase separation discipline).
2. Template Haskell declaration splices (for type-level CT boundaries).
3. Zig comptime reflection ergonomics (`@typeInfo` style future-facing hooks).

## Placement in Pipeline

```
CoreBuilt
  -> Typecheck
  -> Monomorphize
  -> Evaluate+Classify (CT evaluator + stage classification)
  -> Residualize+Specialize
  -> Normalize
  -> Linearize
  -> C Emit
```

Stage A in v1 remains the only place where value-level CT execution happens.

## Contracts and Invariants

The evaluator must satisfy all of these invariants:

1. `stage_of_expr.len() == program.exprs().len()`.
2. `knownness_of_expr.len() == program.exprs().len()`.
3. Every `ct_cache` entry key is in-bounds for expression arena.
4. Every `branch_decisions` key is in-bounds and consistent with folded boolean.
5. `handler_discharge`/`clause_discharge` cover all handlers with stable arity.
6. Any function id carried in `Reason` remains in-bounds.
7. Cache key, file deps, and program fingerprint determine snapshot reuse exactly.
8. Evaluator behavior is deterministic under identical input + target config.

v1 policy: invariant assertions are enforced in tests and at fused boundaries.

## Data Model (Evaluator-facing)

### Outcomes

Evaluator computes known/stuck outcomes, then projects to existing v1 tables.

```
enum Outcome {
  Known(Literal),
  Stuck(Reason),
}
```

v1 keeps `Literal` as known value carrier. Composite/partial outcomes are v2.

### Working State

```
struct EvalState {
  env: HashMap<VarId, Outcome>,
  call_stack: Vec<FuncId>,
  fuel_remaining: u64,
}
```

v1 uses bounded recursion/fuel to guarantee termination and predictable cost.

## Execution Semantics (v1)

### Expressions

1. `Literal` -> `Known(lit)` (target-normalized for ints).
2. `Var` -> lookup env; missing env binding -> `Stuck(DependsOnVar(var))`.
3. `Unary`/`Binary` -> evaluate operands; if all known apply target-aware fold.
4. `PureCall` -> evaluate args, then attempt callee body evaluation under bounds.
5. Constructors (`MakeStruct`/`MakeEnum`) stay conservative in v1 slice unless all
   fields can be represented in current known-value carrier.

### Statements

1. `Return(expr)` -> evaluate expr.
2. `Let` -> evaluate value expr; bind outcome in env; continue.
3. `Val` -> evaluate child stmt to outcome; bind; continue.
4. `If` -> known boolean picks live branch; otherwise stuck on branch condition.
5. `Match` -> v1 conservative unless scrutinee and arm selection are fully known.
6. Effectful/control statements (`Call`, `Perform`, `Resume`, `Handle`) remain
   conservative in early slices unless explicitly discharged by staged rules.

### Call Evaluation Tiers

1. Any runtime/stuck arg -> stuck; ct-only callee with runtime args -> hard error.
2. All known args + CT-eligible effects -> execute callee body under fuel bounds.
3. Known args + non-CT-eligible effects -> `Stuck(EffectNotDischarged(_))`.

### Effect Eligibility

v1 CT-eligible effects are capability-gated and reuse existing effect property
classification. Runtime-only or opaque effects force stuck outcomes.

### Stage Directives

- `@comptime { ... }` enforces CT context and persistability checks for boundary
  crossings.
- `@runtime { ... }` forces runtime stage in contained region.

## Query Cache and Invalidation

The evaluator keeps existing v1 query-cache shape:

- key: target spec + evaluator policy + compiler version
- deps: normalized path + content hash (`ComptimeReadFiles`)
- fingerprint: program structural fingerprint

Reuse occurs only when all three match.

## Error and Diagnostics Policy

The evaluator must emit diagnostics via existing `DiagnosticBag` channels and
preserve stage reasons that can be rendered by staging/provenance reporting.

Hard errors in v1:

1. ct-only function called with runtime/stuck args.
2. non-persistable CT value crossing a CT->RT boundary.
3. malformed stage directive context where contract is violated.

## Implementation Stages

### CTE-1 (bootstrap)

1. Introduce `src/passes/ct_eval.rs` as Stage A evaluator entrypoint.
2. Reuse existing cache/invalidation plumbing from `ct_propagate`.
3. Add pure-call execution for known-arg pure functions (bounded recursion).
4. Keep unsupported shapes conservative (stuck/unknown) rather than unsound folds.

### CTE-2 (semantic closure for v1 Stage A)

1. Expand statement evaluator coverage (`If`, selective `Match`, stage blocks).
2. Integrate branch-decision derivation from evaluator outcomes.
3. Tighten reason propagation and handler discharge consistency checks.
4. Align fused and split pipeline behavior under shared Stage A entrypoint.

### CTE-3 (hardening)

1. Add parity matrix tests for hit/miss/invalidate and target divergence.
2. Add recursion/fuel guard tests and deterministic replay tests.
3. Add boundary-role diagnostics tests for evaluator-triggered failures.
4. Add metamorphic tests (alpha-renaming/dead-code insertion invariance).

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

