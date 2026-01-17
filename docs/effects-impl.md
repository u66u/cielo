Prob best fit for Cielo is a hybrid:

1. Evidence passing for all handled operations (comptime known capability/evidence records)
2. Direct lowering for tail-resumptive clauses (no continuation capture, no generic dispatcher)
3. Selective CPS only for non-tail resumptive clauses (localized control path)
4. Keep and extend bounded handler specialization so wrapper and recursive hotspots collapse to direct code

Why this wins:

- It aligns with Cielo's current architecture (effect rows, capability levels, single-shot resumptions, residualization, handler specialization).
- It preserves very fast direct-path operations (state/reader/writer/exception style clauses) while still supporting control effects.
- It gives a clean V2 path (fusion, generalized specialization, richer control effects) without forcing whole-program CPS now.

Recommended primary candidate: `H1` (Evidence + Selective CPS + Specialization)  
Recommended fallback candidate: `H2` (Evidence + runtime continuation object for control clauses only)

---

### What is currently missing at low level

- No fully materialized evidence/capability ABI in runtime C path.
- `LinearStmt::Perform` still lowers through generic runtime call plumbing instead of fully specialized per-clause dispatch.
- Runtime handler stack exists, but continuation capture and resume semantics are still minimal/stub-like in `cielo_perform` (`src/backend/cielo_runtime.h`).


Each candidate is rated on:

1. Direct-path performance (hot handled ops where resume is tail)
2. Control-path performance (non-tail resume, continuation materialization)
3. Compiler complexity and risk
4. Runtime complexity and ABI stability
5. Cohesion with Cielo paradigm (explicit effects, CT/RT split, single-shot resumptions)
6. V2 extensibility (fusion, generalized specialization, richer effects)
7. Total migration cost in phased LOC bands

---

## Candidates

## C1. Deep handlers + generic runtime dispatcher

Model:

- Keep `perform`/`handle` mostly explicit to late lowering.
- Resolve operations via runtime stack search + op dispatch.

Pros:

- Lowest compiler rewrite pressure.
- Semantically straightforward.

Cons:

- Slow hot path due to generic dispatch.
- Weak fit with your existing specialization/discharge strategy.
- Hard to optimize away continuation/control overhead.

Best use:

- Prototype/debug runtime semantics only.

## C2. Whole-program CPS for all effectful code

Model:

- Transform all effectful functions to CPS and make continuation explicit everywhere.

Pros:

- Uniform semantics model.
- Easy to reason about advanced control behavior in one framework.

Cons:

- Large code growth and debugging complexity.
- Penalizes direct/tail-resumptive majority cases.
- Poor cohesion with current Cielo pipeline (which already separates direct/control cases).

Best use:

- Research compiler or when multi-shot control is immediate top priority.

## C3. Runtime stack-copying one-shot continuations (OCaml style)

Model:

- Fibers/stack segments represent continuations; effect perform unwinds to nearest handler.

Pros:

- Strong control-effect runtime story.
- Good one-shot continuation performance in mature runtimes.

Cons:

- Runtime-heavy engineering; harder to keep backend-simple C emission path.
- Requires deeper runtime ownership/stack design than current Cielo runtime.

Best use:

- If Cielo moves toward VM/runtime-centric architecture.

## C4. Evidence-passing direct style (Koka/GEP style)

Model:

- Compile effects to explicit capability/evidence arguments.
- Monomorphization and specialization reduce dispatch to direct calls in common paths.

Pros:

- Excellent direct-path performance.
- Strong coherence with explicit effects and your existing monomorphization direction.
- Good codegen properties for C backend.

Cons:

- Control clauses still need a strategy (cannot avoid continuation representation entirely).
- Needs carefully designed evidence ABI to avoid churn.

Best use:

- Core V1/V2 backbone.

## C5. Selective CPS for control clauses only (Links/Effekt style)

Model:

- Keep direct style by default.
- CPS-transform only clauses/call regions that actually require non-tail resumptions.

Pros:

- Keeps direct path fast.
- Localizes CPS complexity.
- Naturally matches your existing `Pure/Direct/Control` split.

Cons:

- Requires precise clause classification and robust lowering boundaries.
- Mixed-style debugging complexity (direct + CPS regions).

Best use:

- Paired with evidence passing and specialization.

## H1. Evidence + Selective CPS + Handler specialization (chosen for now)

Model:

- C4 + C5 + existing and expanded handler specialization.

Pros:

- Best balance of performance, complexity, and future-proofing.
- Maximum reuse of current passes and design choices.
- Supports staged optimization path: direct first, control second, fusion later.

Cons:

- More moving parts than a single-technique compiler.
- Requires tight invariants between passes.

## H2. Evidence + runtime continuation object for control clauses (fallback)

Model:

- Evidence-passing direct path.
- For control clauses, use runtime continuation objects (not global CPS transform).

Pros:

- Lower compiler complexity than selective CPS.
- Faster to reach semantic completeness.

Cons:

- Runtime becomes heavier.
- Typically lower peak optimization potential than selective CPS.

---

## Comparative scoring

| Candidate                                    | Direct perf | Control perf | Compiler complexity | Runtime complexity | Cohesion w/ Cielo | V2 extensibility | Overall |
| -------------------------------------------- | ----------- | ------------ | ------------------- | ------------------ | ----------------- | ---------------- | ------- |
| C1 deep dispatcher                           | 2           | 2            | 4                   | 3                  | 2                 | 2                | 2.4     |
| C2 whole CPS                                 | 2           | 4            | 1                   | 3                  | 2                 | 4                | 2.7     |
| C3 stack-copying runtime                     | 3           | 4            | 2                   | 1                  | 2                 | 4                | 2.9     |
| C4 evidence passing                          | 5           | 2            | 3                   | 4                  | 5                 | 4                | 3.9     |
| C5 selective CPS only                        | 4           | 4            | 2                   | 3                  | 4                 | 4                | 3.6     |
| H1 evidence + selective CPS + specialization | 5           | 4            | 3                   | 3                  | 5                 | 5                | 4.4     |
| H2 evidence + runtime continuation object    | 5           | 3            | 4                   | 2                  | 4                 | 4                | 3.9     |

---

## Projected implementation complexity (LOC bands)

Bands:

- Small: <300 LOC
- Medium: 300-800 LOC
- Large: 800-1600 LOC
- XL: >1600 LOC

### C1 deep dispatcher

- IR adjustments: Small
- Lowering changes: Small
- Backend/runtime: Medium
- Tests/bench: Medium
- Total: Medium-Large (700-1300 LOC)
- Risk: Medium (performance risk high)

### C2 whole CPS

- IR and typing surface: Medium
- CPS transform and rewrites: XL
- Backend/runtime updates: Large
- Tests/bench: Large
- Total: XL (2600-4200 LOC)
- Risk: High

### C3 stack-copying runtime

- Runtime substrate: XL
- Compiler integration: Large
- Backend integration: Large
- Tests/bench: Large
- Total: XL (3000-4800 LOC)
- Risk: High

### C4 evidence passing

- Effect ABI + IR nodes: Medium
- Lowering integration: Medium-Large
- Backend/runtime dispatch changes: Medium
- Tests/bench: Medium
- Total: Large (1300-2300 LOC)
- Risk: Medium

### C5 selective CPS only

- Clause classification hardening: Small-Medium
- Local CPS transform: Large
- Backend/runtime support: Medium
- Tests/bench: Medium
- Total: Large (1400-2400 LOC)
- Risk: Medium-High

### H1 recommended (C4 + C5 + specialization expansion)

- Evidence ABI and effect op lowering: Medium-Large
- Selective CPS regions: Large
- Specialization improvements: Medium
- Runtime and backend support: Medium
- Tests + benchmark harness: Medium
- Total: 2200-3600 LOC
- Risk: Medium (higher integration complexity, lower performance risk)

### H2 fallback

- Evidence ABI + lowering: Medium-Large
- Runtime continuation object path: Medium-Large
- Backend integration: Medium
- Tests/bench: Medium
- Total: 1700-2900 LOC
- Risk: Medium

---

## Public interface and IR changes for H1

The implementation should introduce explicit low-level interfaces:

1. Evidence/capability runtime ABI

- `CieloEvidence` record per handler installation (effect id, clause fn table, captures)
- Capability identity as fresh lexical instance id

2. Lowered effect op forms

- `DirectEffectCall`: tail-resumptive clause, no continuation allocation
- `ControlEffectCall`: non-tail clause, explicit continuation payload

3. Continuation representation (single-shot)

- Continuation object/state for selective CPS regions only
- Runtime guard for single-resume use

4. Handler dispatch contract

- No string-name op dispatch on hot paths
- Dispatch by comptime op index / symbol id in specialized code paths

---

## migration sequence (H1)

Phase A: Evidence ABI and lexical capability freshness

- Add explicit evidence structures in lowered/runtime path.
- Replace generic op-name dispatch in hot paths with indexed/symbol dispatch.
- Thread fresh capability ids per handler scope.

Phase B: Direct effect lowering path

- Lower tail-resumptive clauses to direct calls.
- Preserve current single-shot diagnostics and strengthen invariant checks.

Phase C: Selective CPS for control clauses

- Restrict CPS conversion to `ControlEffectCall` regions.
- Keep unaffected functions in direct style.

Phase D: Specialization expansion

- Extend handler specialization beyond current wrapper-only shapes where safe.
- Keep bounded specialization keys and remapping discipline.

Phase E: Optimization and cleanup

- Add dead-handler elimination after effect-row intersection checks.
- Add optional disjoint-handler fusion in v2 track.

---

## Benchmark suite specification

This is a spec for later implementation, not implemented in this report.

## Microbenchmarks

1. `state_loop_direct`

- tight loop with handled `get/set` tail resumptions
- target: direct path throughput

2. `reader_writer_chain`

- nested direct handlers with lightweight payload passing
- target: dispatch overhead and allocation count

3. `non_tail_resume_tree`

- control-heavy handler with non-tail resume in branching recursion
- target: continuation capture cost and correctness

4. `exception_short_circuit`

- early-exit effect with mixed direct/control sites
- target: branch + resume path overhead

## Macrobenchmarks

1. Specialization-heavy recursive workload under handler
2. Mixed CT-discharged and residual RT effect workloads
3. Nested disjoint handlers (future fusion candidate)

## Metrics

- ns/op
- allocations/op
- continuation captures/op
- code size (generated C LOC and object size)
- comptime (pass-level timings)

## Acceptance thresholds for adoption

Compared to current generic perform path:

1. `state_loop_direct`: >= 3x speedup
2. direct-path allocations: zero heap allocations/op in steady-state
3. `non_tail_resume_tree`: <= 1.5x overhead vs a whole-CPS control baseline
4. code size growth: <= 25% on representative macrobench set
5. comptime growth: <= 20% for non-control-heavy programs

---

## insights and reductions

1. Evidence passing is dictionary passing for effects

- After monomorphization, effect evidence can be represented as compact records.
- This compresses dynamic op lookup into static field/index access in many paths.

2. Tail-resumptive handlers reduce to plain control-flow joins

- Tail resume is isomorphic to direct call + join point.
- In those cases, continuation objects are avoidable and should be erased.

3. Selective CPS is a refinement, not a competing architecture

- Treat CPS as a local lowering for control-only regions.
- This keeps most of the program in direct style and limits code growth.

4. Handler specialization is partial evaluation of interpreters

- Pushing known handlers into callees is equivalent to specializing an effect interpreter against static clauses.
- This is why specialization and evidence passing compound well.

5. Capability freshness is the semantic firewall

- Fresh lexical capability ids avoid same-label interception bugs (internal and user effects colliding).
- This should stay explicit in IR/runtime even if hidden in surface syntax.

6. Local vs shared state split is optimizer-critical

- It is not cosmetic typing: it determines legal rewrites and staging safety.
- Preserve this split through lowering metadata.

---

## Risks and mitigations

Risk: pass invariant drift between specialization and lowering  
Mitigation: add invariant checks and golden tests at phase boundaries.

Risk: selective CPS infecting too much code  
Mitigation: strict region boundaries and diagnostics for why CPS was triggered.

Risk: runtime ABI churn  
Mitigation: lock evidence/continuation structs early and version the ABI surface.

Risk: code size blow-up from specialization  
Mitigation: keep bounded specialization keys, cap clone depth, and report specialization counts.

---

## Mayhaps: linearity qualifiers and modal/qualified effects

###  control-flow linearity qualifiers

Implement:

1. Per-clause and per-op qualifier lattice:
- `Abortive` (resume never called)
- `Affine` (resume called at most once)
- `Linear` (resume called exactly once on all paths that continue)
- `Multi` (resume may be called more than once)
2. Reuse existing path-sensitive resume-use analysis as the backbone (`src/passes/linearize.rs` already computes bounds).
3. Use qualifiers to gate lowering and optimization:
- `Linear` or tail-resumptive -> direct fast path
- `Affine` -> single-shot continuation object allowed
- `Multi` -> reject in v1 or route to explicit multi-shot runtime path in v2
4. Emit diagnostics that explain why an op fell off direct path.

Estimated cost and payoff:

- Cost: Medium (roughly 400-900 LOC across analysis/lowering/diagnostics)
- Payoff: High (better fast-path hit rate, lower control-path surprises, clearer safety boundaries)

### modal/qualified effect typing

Full form is hard. Minimal form is manageable and useful.
Implement (minimal, high-ROI subset):

1. Ambient stage/mode context checks (`@comptime` as restricted effect world).
2. Persistability boundary checks (you already have tiers; keep extending this instead of full modal calculus).
3. Capability non-escape checks in handler result types and cross-stage boundaries.
4. Optional lightweight qualifiers on effects for optimization legality (`discardable`, `commutative`, `opaque_for_staging`, `shared_state`).

Do not implement initially:

1. Full modal type calculus with new modality kinds and proof-heavy typing machinery.
2. Higher-rank modal quantification and generalized modality inference.
3. A full replacement of row typing with modal typing.

Estimated cost and payoff:

- Minimal subset cost: Medium-Large (700-1500 LOC)

Practical recommendation:

1. Ship the minimal subset only after P0 and core P1 are in.
2. Keep modal machinery as a checker-side layer; do not let it destabilize low-level lowering/ABI.

---


##  Clear winners by version

### v1 clear winner

`W1`: Evidence ABI + lexical capability freshness + per-clause direct/control lowering
within fused `Evaluate+Classify` -> `Residualize+Specialize` -> `linearize`.

Why:

- Highest runtime perf-per-cost in v1.
- Directly matches v1 authoritative docs.
- Preserves an easy v2 upgrade path.

### v2 clear winner

`W2`: Capability-to-region unification + row-polymorphism-enabled handler fusion +
generalized specialization (return-clause parameterization).

Why:

- Capability-to-region gives immediate memory/runtime wins.
- Fusion and generalized specialization remove remaining handler overhead in complex nests.
- Row polymorphism unlocks these without API blow-up.

---

## Dependencies required to begin

### Global prerequisites

1. Concrete effects assertion at pre-staging boundary (no unresolved row/effect vars).
2. Stable phase-typed artifacts at fused boundaries (`Evaluate+Classify`, `Residualize+Specialize`).
3. Existing benchmark harness skeleton and staging diagnostics plumbing.
4. Runtime ABI versioning policy for handler evidence structs.

### v1-specific prerequisites

1. Capability scope stack and handler-id/effect-id metadata reachable in lowering.
2. `ClauseDischargeStatus` available from `Evaluate+Classify`.
3. Baseline correctness suite for single-shot resume behavior.

### v2-specific prerequisites

1. v1 winner stages complete and stable.
2. Row-polymorphism feature flag and monomorphization support for row variables.
3. Escape-analysis hooks available in linear/ARC pipeline (for capability-to-region).

---

## v1 implementation plan

Priority order for v1:

1. Evidence ABI + capability freshness
2. Resumption linearity qualifiers (lightweight, not full linear types)
3. Direct/control lowering hardening + selective CPS boundaries
4. Residualize+Specialize specialization hardening
5. Perf gates and regression harness

### V1-Stage 1: Evidence ABI + lexical capability freshness

Depends on:
- v1 prerequisites #1 and #2

Deliverable:
- Explicit evidence/capability structs in lowering/runtime path.
- Fresh capability id per handler installation.
- Hot-path dispatch by op index/symbol id, no string dispatch in optimized handled path.

DoD:
- All handled direct clauses compile to direct evidence-backed calls.
- Capability identity is unique per lexical handler scope.

Test targets (subtle/ambiguous):
- Wrong-handler interception regression from `docs/v1/caveats.md` callback scenario.
- Same effect label installed twice; nearest lexical handler always wins.
- Nested handlers with identical op names but different captures.

Oracle references:
- *Effect Handlers in Scope* (scope/capture discipline): https://arxiv.org/abs/2106.00389
- *Handling Algebraic Effects* (core handler semantics): https://lmcs.episciences.org/705

### V1-Stage 2: Lightweight resumption linearity qualifiers

Depends on:
- V1-Stage 1

Deliverable:
- Per-clause qualifier classification (`Abortive`/`Affine`/`Linear`/`Multi`) derived from resume-use analysis.
- Lowering gates keyed off qualifiers.

DoD:
- `Linear` and tail-resumptive clauses go direct path.
- `Multi` clauses are rejected in v1 (or explicitly routed to unsupported diagnostic).
- Diagnostics explain classification outcome at clause span.

Test targets:
- Branch-exclusive single resumes accepted.
- Same-path double resume rejected.
- Resume captured or passed as value rejected under v1 surface rules.

Oracle references:
- Effekt tail-resumption logic reference (implementation pattern): `docs/v1/implementation_ref.md`
- *Compiling without continuations* (direct/control split grounding): https://bentnib.org/compiling-without-continuations.pdf

### V1-Stage 3: Direct/control lowering hardening and selective CPS boundary lock

Depends on:
- V1-Stage 2

Deliverable:
- Linear IR boundary guarantees:
  - handled direct ops -> `DirectCall`
  - handled control ops -> `ControlCall`
  - no residual handled `Resume` nodes past lowering boundary

DoD:
- Selective CPS only for `ControlCall` regions.
- No CPS spill into pure/direct-only functions.

Test targets:
- Mixed direct/control clauses in the same handler.
- Non-tail resumptive clause with mutable state interactions.
- Dead handler elimination when handled-effect intersection is empty.

Oracle references:
- *Retrofitting Effect Handlers onto OCaml* (one-shot runtime/control behavior): https://doi.org/10.1145/3453483.3454039
- OCaml 5 effect semantics docs (one-shot operational expectations): https://ocaml.org/manual/5.3/effects.html

### V1-Stage 4: Residualize+Specialize hardening for bounded specialization

Depends on:
- V1-Stage 3

Deliverable:
- Reliable wrapper and recursive specialization in fused residual pass.
- Strict specialization bound keys and remapping integrity.

DoD:
- Each `(function, handler-shape)` specialized at most once.
- Recursive calls in specialized copies retarget correctly.
- Non-specializable forms remain semantics-preserving.

Test targets:
- Recursive handler-wrapped loops (tie-back correctness).
- Varying return-clause case must abort specialization in v1 (expected behavior).
- Reachability pruning and ID remap consistency across side tables.

Oracle references:
- Effekt static-argument/specialization references: `docs/v1/implementation_ref.md`
- `docs/v1/comptime_passes.md` specialization invariants

### V1-Stage 5: Perf gates + differential regression suite

Depends on:
- V1-Stage 1 through V1-Stage 4

Deliverable:
- Bench suite + CI thresholds for direct/control handler workloads.
- Differential test harness: evaluator semantics vs lowered runtime behavior.

DoD:
- Meets v1 acceptance thresholds from Section 8.
- No correctness regressions on seeded randomized handler programs.

Test targets:
- Deep handler stacks with disjoint and overlapping effects.
- LocalState vs SharedState optimization legality tests.
- CT-discharge with RT value pass-through.

Oracle references (how to build oracle):
- Redex for executable semantics/model oracle: https://docs.racket-lang.org/redex/
- Differential testing method (reference interpreter vs compiled output):
  use `Evaluate+Classify` interpreter path as the executable oracle for pure/direct fragments.

---

## v2 implementation plan 

Priority order for v2 (perf for buck):

1. Capability-to-region unification
2. Linear-IR normalization + expanded effect-qualified opts
3. Row polymorphism foundation
4. Handler fusion
5. Generalized specialization
6. Comptime scalability track (partial values, flow-sensitive state, query architecture, totality)

### V2-Stage 1: Capability-to-region unification

Depends on:
- v2 prerequisites #1 and #3

Deliverable:
- Handler evidence allocation mapped onto region scopes.
- Escape-aware stack allocation of non-escaping evidence.

DoD:
- Evidence lifetime tied to region lifetime by construction.
- ARC handles region-backed evidence without leaks or premature frees.

Test targets:
- Evidence captured in closures that outlive handler scope (must force escape path).
- Control/yield paths crossing handler boundaries.
- Nested handlers with region nesting and early exits.

Oracle references:
- *From Capabilities to Regions* (typed translation and semantics-preserving guidance): https://doi.org/10.1145/3622831

### V2-Stage 2: Normalize Linear + effect-property-driven optimizations

Depends on:
- V2-Stage 1

Deliverable:
- `normalize_linear` pass in pipeline.
- Commutative/idempotent/discardable effect-property rewrites.

DoD:
- Rewrites gated strictly by effect properties and Local vs Shared state distinction.
- No reordering across non-commutative or opaque effects.

Test targets:
- Reordering tests with aliasing traps (should reject invalid reorderings).
- Idempotent elimination with equal vs non-equal values.
- SharedState volatile behavior must block LocalState-only rewrites.

Oracle references:
- `docs/v1/features.md` rewrite legality rules.
- Property-based optimizer validation via equivalence checking against unoptimized linear interpreter.

### V2-Stage 3: Row polymorphism foundation

Depends on:
- v2 prerequisites #2

Deliverable:
- Row variables in function signatures and row unification.
- Monomorphization produces concrete rows pre-`Evaluate+Classify`.

DoD:
- No unresolved row vars at staging boundary.
- API-level polymorphic effect combinators typecheck.

Test targets:
- `map_with_effects` style propagation.
- Hidden internal effects with rank-2-like encapsulation patterns (where supported).
- Ambiguous row inference diagnostics.

Oracle references:
- *Type Directed Compilation of Row-Typed Algebraic Effects*: https://www.microsoft.com/en-us/research/publication/type-directed-compilation-of-row-typed-algebraic-effects/
- Koka row-polymorphism behavior reference: https://koka-lang.github.io/koka/doc/book.html

### V2-Stage 4: Handler fusion (disjoint effects only)

Depends on:
- V2-Stage 3

Deliverable:
- Fusion transform for statically known disjoint nested handlers.
- Integration point after `Residualize+Specialize` (or in-pass extension).

DoD:
- Fusion only when disjointness and safety constraints pass.
- Fused and unfused versions are observationally equivalent on validated test corpus.

Test targets:
- Disjoint nested handlers (must fuse).
- Overlapping effect sets (must not fuse).
- Mixed CT-dischargeability between fusion candidates.

Oracle references:
- *Staging Effect Handlers for Modular Search* (fusion speedups/semantics target): https://doi.org/10.1145/3704908
- Algebraic handler semantics oracle from *Handling Algebraic Effects*: https://lmcs.episciences.org/705

### V2-Stage 5: Generalized specialization (return-clause parameterization)

Depends on:
- V2-Stage 4

Deliverable:
- Specializer handles varying recursive return clauses by continuation parameter.

DoD:
- Former v1 abort cases now specialize where rules permit.
- Bounded specialization and recursion tie-back still hold.

Test targets:
- With-Do style varying return clauses in recursion.
- CPS parameter threading through recursive calls.
- Code-size guardrails under specialization.

Oracle references:
- Effekt specialization and selective CPS references: `docs/v1/implementation_ref.md`
- *Compiling without continuations*: https://bentnib.org/compiling-without-continuations.pdf

### V2-Stage 6: Comptime scalability track (parallel to stages 1-5)

Depends on:
- v2 prerequisites #1

Deliverable:
- Partial values, flow-sensitive mutable state, query architecture, optional totality checker.

DoD:
- No staging regressions against v1 on unchanged programs.
- Incremental recompilation invalidates only dependent queries.
- Totality-proven functions bypass fuel path.

Test targets:
- Partial struct/enum field evaluation and residualization.
- Mutable state reset from `Stuck` to `Known` across control-flow joins.
- Query invalidation under `ComptimeReadFiles` hash changes.
- Cycle handling correctness in query execution.

Oracle references:
- Partial static data theory: https://doi.org/10.1145/3236769
- Salsa query-system pattern (oracle for dependency invalidation semantics): https://github.com/salsa-rs/salsa
- Sized types / totality grounding:
  - https://doi.org/10.1145/263116
  - https://www.cse.chalmers.se/~abela/    (sized-types resources)

---

## Oracle construction cookbook

For effect/runtime stages with formal grounding, build oracles with this stack:

1. Small-step reference evaluator for a Core subset (handlers, perform, resume, state).
2. Differential runner:
- run program via reference evaluator
- run lowered linear/runtime path
- compare normalized traces and final values
3. Metamorphic checks:
- alpha-renaming invariance
- handler alpha-equivalence invariance
- dead-code insertion with discardable effects preserves behavior
4. For optimization passes, enforce:
- semantic equivalence under allowed rewrites
- strict non-application when preconditions are violated

Recommended tooling references:

1. Redex for executable semantics and reduction testing: https://docs.racket-lang.org/redex/
2. Rust property-based testing for randomized program generation: https://docs.rs/proptest/latest/proptest/

---

## References

1. Plotkin, Pretnar. *Handling Algebraic Effects*. LMCS, 2013.  
   https://lmcs.episciences.org/705
2. Pretnar. *An Introduction to Algebraic Effects and Handlers*. 2015.  
   https://www.eff-lang.org/handlers-tutorial.pdf
3. Leijen, Ye, Hillerstrom. *Generalized Evidence Passing for Effect Handlers*. ICFP 2021.  
   https://www.microsoft.com/en-us/research/publication/generalized-evidence-passing-for-effect-handlers/
4. Hillerstrom et al. *Retrofitting Effect Handlers onto OCaml*. PLDI 2021.  
   https://dl.acm.org/doi/10.1145/3453483.3454039
5. Kiselyov, Sivaramakrishnan. *Eff Directly in OCaml*. JFP 2021.  
   https://arxiv.org/abs/1812.11664
6. Bauer et al. *Effect Handlers in Scope*. Journal of Functional Programming, 2020.  
   https://www.cambridge.org/core/journals/journal-of-functional-programming/article/effect-handlers-in-scope/9B0F6299E08F9C2E4B99ADB6E363F10E
7. Leijen. *Type Directed Compilation of Row-Typed Algebraic Effects*. POPL 2017.  
   https://www.microsoft.com/en-us/research/publication/type-directed-compilation-of-row-typed-algebraic-effects/
8. Hillerstrom, Lindley. *Compiling without continuations*. ICFP 2018.  
   https://bentnib.org/compiling-without-continuations.pdf
9. Schuster et al. *Rows and Capabilities as Modal Effects*. OOPSLA 2025.  
   https://people.mpi-sws.org/~skilpatr/publications/oopsla2025racoome.pdf
10. Ahman, Schuster, et al. *Qualified Effect Types*. POPL 2024.  
   https://www.microsoft.com/en-us/research/publication/qualified-effect-types/
11. Yang et al. *Asymptotic speedup via effect handlers*. ICFP 2022.  
   https://www.cs.ox.ac.uk/people/nicolas.wu/papers/icfp22.pdf
12. OCaml 5.3 Reference Manual (effect handlers chapter and one-shot continuation model).  
   https://caml.inria.fr/pub/distrib/ocaml-5.3/ocaml-5.3-refman.html
13. Koka repository and docs (direct C backend, evidence-passing implementation context).  
   https://github.com/koka-lang/koka
14. Koka std core handler internals.  
   https://koka-lang.github.io/koka/doc/std_core_hnd-source.html
15. Koka latest release so far
   https://github.com/koka-lang/koka/releases
