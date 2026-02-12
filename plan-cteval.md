# CT Evaluator Execution Tracker

Scope:
- v1 value-level CT evaluator implementation and hardening
- v2 items tracked but deferred

## Locked Decisions

- [x] Keep value-level CT evaluator separate from type-level macro/declaration synthesis.
- [x] Reuse v1 query-cache + invalidation shape (`CtCacheKey + deps + fingerprint`).
- [x] Keep v1 known-value carrier conservative (`Literal` only).
- [x] Use bounded recursion guard for call evaluation (`MAX_CALL_EVAL_DEPTH`).
- [x] Keep unsupported/control-heavy shapes conservative (stuck/no fold) in CTE-1.
- [x] Integrate incrementally: expose evaluator API first, switch canonical Stage A only after parity gates are green.

## CTE-0 Docs + Tracker

- [x] Add detailed evaluator implementation doc (`docs/cteval-impl.md`).
- [x] Add this execution tracker with tasks/decisions/caveats.

## CTE-1 Bootstrap (active)

- [x] Introduce `src/passes/ct_eval.rs` pass entrypoint.
- [x] Seed evaluator from existing `ct_propagate` cache/invalidation plumbing.
- [x] Implement pure known-arg `PureCall` evaluation for pure callees.
- [x] Evaluate `Return` / `Let` / `Val` / known-boolean `If` in callee bodies.
- [x] Guard recursion/cycles via bounded depth.
- [x] Expose compiler API for evaluator experiments (`Compiler::run_v1_ct_eval`).
- [x] Add evaluator tests (`tests/ct_eval.rs`): pure-call fold, let-chain fold, recursion-cycle no-fold.
- [x] Keep branch decision table aligned with newly folded call conditions.

## CTE-2 Stage-A Parity Hardening

- [x] Add branch-decision recompute/merge for evaluator-added cache entries.
- [x] Add tests for branch-decision parity on folded call conditions.
- [x] Add tests for conservative behavior on unsupported statements (`Match`, `Handle`, `Perform`).
- [x] Add tests for effectful callee no-fold guarantees.
- [x] Add tests for stage-block interaction (`@comptime` accepted, `@runtime` rejected in evaluator path).
- [x] Add tests for call-depth budget determinism.

## CTE-3 Integration Gates

- [x] Add explicit parity suite: `ct_eval + bta` vs `ct_propagate + bta` for Stage tables.
- [x] Resolve backend-sensitive behavior deltas (reachability/codegen expectations).
- [x] Switch fused Stage A (`comptime::evaluate_classify`) to `ct_eval`.
- [x] Switch default v0 core pipeline Stage A to `ct_eval`.
- [ ] Remove redundant/legacy Stage-A plumbing once parity is stable.

## CTE-v2 Deferred

- [ ] `Outcome::Partial` representation and partial residualization policy.
- [ ] Salsa-style query graph migration.
- [ ] Flow-sensitive mutable-state staging.

## Caveats / Risks

- Folding more calls can change residual reachability and codegen shape; backend tests will detect this.
- v1 literal-only known values limits constructor folding; avoid unsound partial evaluation hacks.
- Evaluator must not bypass CT/RT boundary diagnostics currently enforced in BTA.
- Query-cache determinism must remain stable across target changes and file-dep updates.

## Verification Checklist (run per CTE task)

- [x] `cargo test -q --test ct_eval`
- [x] `cargo test -q --test backend`
- [x] `cargo test -q`
