## F1) Evidence ABI and lexical capability freshness

### Formal core

Objects:

- Effect labels: `ℓ`
- Capability identities: `κ`
- Handler stack: `H = [(ℓ, κ, clauses, captures), ...]`
- Evidence value `ev` corresponding to topmost matching handler frame

Operational lookup:

- `lookup(H, ℓ) = first frame from top with effect label ℓ`

Freshness invariant:

- If two handler installations are distinct lexical scopes, they receive distinct `κ`.

### Key theorem targets

1. Deterministic nearest-handler resolution: for any `perform ℓ`, lookup returns unique topmost frame.
2. Non-interference by label collision: a callback cannot accidentally target an unrelated internal handler if `κ` is fresh per installation.

### Proof strategy checklist

1. Prove stack-discipline lemma: push/pop preserves relative order.
2. Prove lookup determinism by top-down first-hit rule.
3. Prove freshness uniqueness by construction (`fresh()` not reused in active stack).
4. Compose with evaluation contexts to show nearest-handler semantics is preserved after lowering.

### Oracle construction

Differential oracle:

1. Evaluate in reference Core interpreter with lexical handler stack.
2. Evaluate lowered runtime path with evidence ABI.
3. Compare final value + trace of handled operations `(ℓ, κ, op)`.

Metamorphic oracle:

- Alpha-rename handler-local capability IDs; behavior must be unchanged.

Subtle bug tests:

1. Same effect label used in internal handler and user callback (wrong-handler trap).
2. Nested same-label handlers with different captures.
3. Reentrant callbacks that perform the same effect.

Primary references:

- Plotkin/Pretnar 2013: <https://lmcs.episciences.org/705>
- EHGC (typed CPS/lowering view): <https://bentnib.org/handlers-cps-journal.pdf>

---

<a id="f2-resumption-linearity-qualifiers-without-global-linear-types"></a>
## F2) Resumption linearity qualifiers without global linear types

### Formal core

Qualifier lattice (per clause):

- `Abortive <= Affine <= Multi`
- `Abortive <= Linear <= Multi`
- `Affine` and `Linear` incomparable unless exactly-once property is known.

Judgment:

- `Γ ⊢ clause c : q` where `q ∈ {Abortive, Affine, Linear, Multi}`

Interpretation:

- `Abortive`: resume never used
- `Affine`: resume used at most once
- `Linear`: resume used exactly once on all continuing paths
- `Multi`: resume may be used more than once

### Key theorem targets

1. Single-shot safety theorem: if `q ∈ {Abortive, Affine, Linear}`, one-shot runtime cannot observe double-resume.
2. Fast-path eligibility theorem: direct lowering is sound for tail-resumptive `Linear` clauses.

### Proof strategy checklist

1. Define path-sensitive resume-use counting over CFG/AST.
2. Prove sound upper bound: analysis never underestimates resume uses.
3. Show lowering preconditions imply runtime one-shot token consumed at most once.
4. Show rejected `Multi` class avoids unsound one-shot execution.

### Oracle construction

Differential oracle:

- Compare reference evaluator with explicit one-shot continuation token model vs lowered runtime.

Property-based oracle:

- Generate random clause bodies under syntax constraints; assert:
  - predicted qualifier is conservative
  - any generated double-resume path is never classified as non-`Multi`

Subtle bug tests:

1. Branch-exclusive single resumes (should remain non-`Multi`).
2. Same-path double resumes (must classify `Multi`).
3. Resume hidden through let/val wrappers and aliases.

References:

- EHGC: <https://bentnib.org/handlers-cps-journal.pdf>
- OCaml one-shot effect docs (runtime discipline): <https://ocaml.org/manual/5.3/effects.html>

---

<a id="f3-directcontrol-lowering-correctness"></a>
## F3) Direct/Control lowering correctness

### Formal core

Translation relation:

- `⟦s⟧ = l` from handled Core statement `s` to Linear statement `l`

Boundary contract:

1. handled tail-resumptive ops -> `DirectCall`
2. handled non-tail ops -> `ControlCall`
3. no handled `Resume` remains after lowering boundary

### Key theorem targets

1. Simulation theorem (forward): each Core step corresponds to 0+ Linear steps with same observable behavior.
2. Reflection theorem (backward up to stuttering): each Linear observable transition maps to a Core transition sequence.

### Proof strategy checklist

1. Define observables (returned value, performed external effects, handler trace).
2. Induct on Core evaluation derivation.
3. Case split on perform/resume/handle.
4. For control cases, use continuation reification lemma.
5. Close with bisimulation or lockstep simulation as practical.

### Oracle construction

Differential oracle:

- Core interpreter vs lowered Linear executor (or emitted C runner) on same program corpus.

Metamorphic oracle:

- Reassociate pure let/val wrappers around handled calls; behavior invariant.

Subtle bug tests:

1. Mixed direct and control clauses in one handler.
2. Dead handler elimination when effect-row intersection is empty.
3. Resume outside active clause context diagnostics.

References:

- EHGC: <https://bentnib.org/handlers-cps-journal.pdf>
- GEP: <https://www.microsoft.com/en-us/research/publication/generalized-evidence-passing-for-effect-handlers/>

---

<a id="f4-bounded-handler-specialization-correctness-and-termination"></a>
## F4) Bounded handler specialization correctness and termination

### Formal core

Specialization key:

- `K = (callee, handler_shape)`

Bound:

- each key `K` specialized at most once in a compilation run.

Rewrite:

- `handle h (f args)` -> `f_h args` when specializable.

### Key theorem targets

1. Termination of specialization pass: finite key space + at-most-once rule.
2. Semantics preservation: specialized call graph is observationally equivalent to unspecialized with explicit handler.

### Proof strategy checklist

1. Define measure: remaining unspecialized eligible callsites + unseen keys.
2. Show each specialization strictly decreases measure.
3. Prove rewrite lemma for one specialized callsite.
4. Lift to whole-program via induction over rewritten call graph.

### Oracle construction

Differential oracle:

- Run before/after specialization on same input, compare outputs and effect traces.

Structural oracle:

- Assert no key specialized more than once.
- Assert recursive retargeting reaches specialized function ID.

Subtle bug tests:

1. Recursive handler-wrapped loops.
2. Wrapper chains with alias-forwarding tails.
3. Non-specializable bodies must remain unmodified semantically.

References:

- GEP (specialization background): <https://www.microsoft.com/en-us/research/publication/generalized-evidence-passing-for-effect-handlers/>
- Effekt implementation references: `docs/Implementation ref.md`

---

<a id="g1-capability-to-region-unification"></a>
## G1) Capability-to-region unification

### Formal core

Typed translation idea:

- Handler scope induces region `ρ`.
- Evidence allocated in `ρ`.
- Exiting handler scope deallocates `ρ`.

Judgment form:

- `Γ; ρ ⊢ e : τ` with region-indexed capability storage.

### Key theorem targets

1. Type-preserving translation from capability-scoped program to region-scoped program.
2. Memory safety/lifetime theorem: non-escaping evidence never outlives region.

### Proof strategy checklist

1. Define capability-store to region-store mapping.
2. Prove substitution/weakening lemmas under region contexts.
3. Prove progress/preservation after translation.
4. Add escape lemma for captured evidence.

### Oracle construction

Differential oracle:

- Compare capability-runtime path and region-runtime path on same programs.

Allocation oracle:

- Validate stack vs region vs heap placement based on escape analysis decisions.

Subtle bug tests:

1. Evidence captured by closures escaping handler.
2. Early returns/exceptions across region boundaries.
3. Nested handlers and nested regions with same effect label.

Reference:

- From Capabilities to Regions: <https://doi.org/10.1145/3622831>

---

<a id="g2-row-polymorphism-and-concretization-before-staging"></a>
## G2) Row polymorphism and concretization before staging

### Formal core

Typing shape:

- `Γ ⊢ f : A -> B ! ε`
- with row variables `ε` and row operations (extension/unification).

Compilation precondition:

- All row variables solved/instantiated before `Evaluate+Classify`.

### Key theorem targets

1. Principal typing (or stable inferred typing) for row-polymorphic functions.
2. Concretization theorem: monomorphization yields row-concrete residual IR accepted by staging passes.

### Proof strategy checklist

1. Define row unification rules and occurs checks.
2. Prove unification soundness.
3. Prove monomorphization substitutes all free row vars in staged IR.
4. Prove staging precondition invariant at pass boundary.

### Oracle construction

Type oracle:

- Golden tests for inferred row signatures of standard combinators.

Boundary oracle:

- Assert no unresolved row vars at staging entry (hard compiler bug if present).

Subtle bug tests:

1. Higher-order map/fold with effect-polymorphic callbacks.
2. Ambiguous row inference diagnostics.
3. Hidden internal effects under polymorphic wrappers.

References:

- Leijen 2017 row-typed compilation: <https://www.microsoft.com/en-us/research/publication/type-directed-compilation-of-row-typed-algebraic-effects/>
- Koka docs/book (practical row behavior): <https://koka-lang.github.io/koka/doc/book.html>

---

<a id="g3-disjoint-handler-fusion-correctness"></a>
## G3) Disjoint handler fusion correctness

### Formal core

Fusion precondition:

- Nested handlers `h1`, `h2` with disjoint handled effect sets:
  `effects(h1) ∩ effects(h2) = ∅`

Rewrite:

- `handle h1 (handle h2 e)` -> `handle fuse(h1,h2) e`

### Key theorem targets

1. Observational equivalence under disjointness and required effect-property constraints.
2. Dispatch reduction theorem: fused execution does no more handler dispatches than unfused execution.

### Proof strategy checklist

1. Define fusion operator over clauses and state threading.
2. Prove commuting diagram for independent operations.
3. Prove return-clause composition law.
4. Discharge counterexample classes where effects overlap or properties fail.

### Oracle construction

Differential oracle:

- Execute fused and unfused programs; compare values and external effects.

Counterexample oracle:

- Auto-generate overlapping-effect cases; assert fusion is rejected.

Subtle bug tests:

1. LocalState + SharedState mixed handlers (must obey volatility constraints).
2. CT-dischargeable + non-dischargeable mixed nesting.
3. Return-clause interactions in deep nesting.

References:

- Plotkin/Pretnar laws baseline: <https://lmcs.episciences.org/705>
- Staging Effect Handlers for Modular Search (PEPM 2026): <https://doi.org/10.1145/3779209.3779536>

---

<a id="g4-generalized-specialization-via-return-continuation-parameterization"></a>
## G4) Generalized specialization via return-continuation parameterization

### Formal core

When recursive callsites use different return wrappers, specialize to:

- `f_spec(args, k)` where `k` represents return-clause continuation.

This is selective CPS at specialization sites, not global CPS.

### Key theorem targets

1. Equivalence to unspecialized recursion under continuation parameterization.
2. Bounded growth theorem with specialization keying and code-size guardrails.

### Proof strategy checklist

1. Define source and transformed recursive equations.
2. Prove by induction on recursion depth that `k`-parameterized worker returns same result as source wrapper composition.
3. Prove key-bound constraints still prevent unbounded specialization.

### Oracle construction

Differential oracle:

- Compare original and generalized-specialized forms on recursive workloads.

Metamorphic oracle:

- Equivalent return-clause refactorings should produce same runtime output and stage behavior.

Subtle bug tests:

1. Varying return wrappers per branch in recursion.
2. Mixed direct/control continuations inside same recursive worker.
3. Large recursive inputs for code-size and perf guardrails.

References:

- EHGC CPS foundation: <https://bentnib.org/handlers-cps-journal.pdf>
- GEP compilation path: <https://www.microsoft.com/en-us/research/publication/generalized-evidence-passing-for-effect-handlers/>

---

## Oracle implementation cookbook (practical)

1. Define a small executable Core semantics (handlers, perform, resume, state).
2. Build a differential runner:
- interpreter result/trace
- lowered runtime result/trace
- deterministic normalization of traces
3. Add metamorphic generators:
- alpha renaming
- harmless let/val wrapping
- dead code insertion with discardable effects
4. Add precondition-failure tests:
- ensure transformations do not fire when hypotheses fail.
5. Run randomized property tests nightly on seeded generator corpus.

Tooling references:

- Redex: <https://docs.racket-lang.org/redex/>
- proptest: <https://docs.rs/proptest/latest/proptest/>
