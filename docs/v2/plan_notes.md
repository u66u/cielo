# V2 Plan Notes

Carry-over items from v1 plus v2-specific planning. This document is a living
scratchpad, not an authoritative specification.

---

## Carry-Over from V1

### Deferred features

- **Generalized handler specialization:** When the return clause varies across recursive
  call sites (the With-Do case), v1 aborts specialization. v2 parameterizes the
  specialized function by the return clause (selective CPS over specialization sites).
  Implementation: add a continuation parameter to the specialized function signature.
  Recursive calls pass the appropriate return-clause closure.

- **Mutable-variable stmt forms:** If `Stmt::Var` / `Stmt::Get` / `Stmt::Put` are added
  to Core IR (currently mutable state is modeled through effects), tail-resumption
  checking must mirror the mutable-state caveat from Effekt's `RemoveTailResumptions`:
  mutable variable definitions are NOT considered tail-resumptive even when they look
  like it.

- **Recursive aggregate pooling:** v1 pools top-level constructor literals in the constant
  table. v2 extends to recursively nested ADT constants (e.g., `Cons(1, Cons(2, Nil))`
  becomes a single pooled constant, not three separate allocations).

- **Non-finite float pooling:** v1 leaves NaN/Inf unresolved during CT evaluation. If v2
  adopts a policy for non-finite floats (e.g., target-specific NaN bit patterns), the
  constant table must handle them. Decision: defer until strict FP emulation lands.

### Infrastructure ready for v2

- **IrNode trait:** Adopted in v1. v2 extends with `bound_vars`, `used_vars`, `is_pure`,
  `effect_class` methods.

- **Phase struct pattern:** v1 established the consume-and-transition pattern. v2 adds
  new phases without changing the pattern.

- **Function-level memoization:** v1's evaluator cache is the seed for v2's query
  architecture. The cache key structure `(FuncId, Vec<Value>)` and cycle detection
  via in-progress set carry over directly.

- **SortedEffectRow:** v1's `union`, `subtract`, `intersect` operations are sufficient
  for v2's handler fusion (disjointness check = `intersect` is empty).

---

## V2 Ordering and Dependencies

### Phase 1: Partially-static data (independent)

**Depends on:** Nothing beyond v1.

**Changes:**
- Add `Partial(PartialValue)` to Outcome enum
- Update evaluator for MakeStruct/MakeEnum with mixed outcomes
- Update evaluator for field access on Partial
- Update residualizer for mixed constructors
- Update C emitter for mixed struct literals

**Risk:** Low. Additive change. v1 code paths (Known/Stuck) are unchanged.

**Testing:** Same test infrastructure as v1. Add tests for mixed-field structs,
field access on partial, match on partial enum.

### Phase 2: Flow-sensitive mutable state (depends on Phase 1)

**Depends on:** Partially-static data (a mutable variable can hold a Partial value).

**Changes:**
- Replace per-variable single Outcome with per-program-point FlowState
- Add join at if/match convergence points
- Add rollback support for speculative clause evaluation

**Risk:** Medium. Changes the evaluator's state model. Must be careful with handler
interaction (non-tail-resumptive clauses invalidate flow state).

**Testing:** Programs with mutable accumulators. Programs with mutable state inside
handler clauses. Programs with mutable state across branches.

### Phase 3: Query architecture (independent)

**Depends on:** Nothing beyond v1. Can proceed in parallel with Phases 1-2.

**Changes:**
- Extract evaluator memoization into QueryDb
- Add dependency tracking
- Add cross-compilation persistence
- Add incremental invalidation

**Risk:** Medium-high. Replacing the memoization layer affects the evaluator's
performance characteristics. Must ensure query overhead doesn't exceed the savings
from incremental recomputation.

**Testing:** Correctness: same results as v1 for same inputs. Performance: measure
compilation time with and without query cache. Incrementality: change a file, verify
only affected queries re-execute.

### Phase 4: Handler fusion (depends on row polymorphism)

**Depends on:** Row polymorphism (to express fused handler types).

**Changes:**
- Disjointness analysis for nested handler pairs
- Fusion transform: merge clause sets, merge state threading
- Integration into Residualize+Specialize pass

**Risk:** High. Handler fusion is semantically subtle (handler ordering matters,
state threading must be correct). Extensive testing required.

**Testing:** Benchmarks from search transformer literature. Correctness tests for
all combinations of handler properties (local/shared, discardable/non-discardable,
commutative/non-commutative).

### Phase 5: Generalized handler specialization (depends on Phase 4)

**Depends on:** Handler fusion (understanding of handler composition). Also needs
the return-clause parameterization mechanism.

**Changes:**
- Detect varying return clauses across recursive call sites
- Generate return-clause continuation parameter
- Selective CPS for the continuation parameter

**Risk:** Medium. The transformation is well-understood from Effekt's literature.
Implementation is mostly mechanical given the handler specialization infrastructure
from v1.

### Phase 6: Capability-to-region unification (independent)

**Depends on:** Nothing beyond v1. Can proceed in parallel.

**Changes:**
- Handler lowering emits region operations for evidence allocation
- Escape analysis covers handler evidence
- C emitter uses stack allocation for non-escaping evidence

**Risk:** Medium. Changes the handler lowering output. Must ensure all handler
patterns still compile correctly.

### Phase 7: Totality checking (independent)

**Depends on:** Nothing beyond v1. Can proceed in parallel.

**Changes:**
- Structural recursion detection on function bodies
- Sized type inference (optional, for more precise checking)
- Integration with evaluator: total functions skip fuel check

**Risk:** Low. Additive feature. Functions that aren't proven total continue to
use fuel-limited evaluation.

---

## V2 Pipeline (projected)

lex/parse → desugar → type+effect check (+ row polymorphism) → monomorphize (types + effects + rows) → Evaluate+Classify (+ partial values, + flow-sensitive state, + query cache) → Residualize+Specialize (+ handler fusion, + generalized specialization) → Normalize (shrink-inline-shrink) → linearize (handler lowering, + capability-to-region) → Normalize_linear (new) → effect-qualified opts (expanded) → ARC insertion → C emission (structured control flow, arena-aware)

text


Changes from v1 pipeline marked with `+`. New passes marked with `(new)`.

---

## Open Questions for V2

### How much does partial static data actually buy?

Hypothesis: most CT value is in scalar fields, not mixed structs. Partial static
data helps most for "config struct" patterns where some fields are build-time and
others are runtime. Need benchmarks on representative codebases.

### Is the query architecture worth the complexity?

For small programs (< 10k LOC), incremental recomputation saves less time than the
query overhead costs. For large programs (> 100k LOC), incremental is essential.
Need to determine Cielo's target program size range.

Mitigation: make the query layer opt-in. Small programs use direct evaluation (v1
style). Large programs enable query caching. The evaluator's core logic is the same
either way.

### How does handler fusion interact with staging?

If two handlers are fused, and one is CT-dischargeable but the other isn't, can the
fused handler be partially discharged? This is the handler-fusion equivalent of
partially-static data.

Tentative answer: yes, but only if the dischargeable handler's clauses don't interact
with the non-dischargeable handler's clauses. Disjointness check covers this.

### What's the incremental compilation unit?

v1: whole program (single file compilation). v2: needs to support multi-file
compilation. The natural incremental unit is the function: a function's staging
result depends on its body + its callees' staging results. Changing a function
invalidates its staging result and all callers'.

This matches the query architecture: `FuncCallQuery` results are the incremental
unit. File-level invalidation is coarser but simpler.

---

## Performance Targets (aspirational)

- CT evaluation: < 100ms for programs < 10k LOC
- Incremental re-staging after single file change: < 10ms for programs < 100k LOC
- Handler specialization: bounded by O(functions × handlers), each bounded by function size
- Normalizer: < 50ms total (both shrink and speculative phases)
- Full pipeline (parse to C emission): < 500ms for programs < 10k LOC
- C compilation of emitted code: dominated by C compiler, not Cielo

These are targets, not guarantees. Measure early, optimize the hot path (evaluator
step function, shrinking reduction fixpoint).

---

## Research to Track

- **Effect handlers in practice:** Ongoing work from the Effekt group 