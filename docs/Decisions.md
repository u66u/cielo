Cielo's philosophy is "everything that can realistically be comptime should be comptime". We use effects to get more granular information about what computations can be compile-time. Effects also provide a nice primitive to create control flow units. Explicit > implicit.

## Compiler style policy (v0/v1, authoritative)

Default implementation style for v0/v1 is conservative and explicit. Clever syntax and
meta-programming are optional tools, not the baseline.

Rules:
- Prioritize readability, phase safety, and easy reasoning over stylistic novelty.
- A non-trivial pattern is acceptable only if it gives one of:
  - clear correctness/safety win
  - elimination of repeated bug-prone boilerplate
  - measurable complexity reduction without obscuring control/data flow
- If understanding control flow requires mentally expanding dense macro/type tricks,
  do not use that pattern in v0/v1.

Pattern policy for v0/v1:
- Adopt now:
  - extension traits for domain semantics
  - scope guards/RAII for push-pop compiler context
  - pattern matching compression when explicit and exhaustive
  - newtype wrappers for invariants and IDs
  - display impls for diagnostics and dumps
  - generic fixpoint helpers for monotone analyses
- Defer (case-by-case only):
  - iterator alchemy for allocation-free traversal
  - const generics fixed-size containers
  - bitset-encoded effect rows/sets
- Avoid for now:
  - blanket `From/Into` IR construction tricks
  - closure-based IR builder DSLs
  - manual function-table dispatch in place of straightforward enum matching

## Phase integrity policy (v0/v1, authoritative)

Analyses and IDs must be phase-safe.

Rules:
- Transformation boundaries use phase-specific newtype IDs (no shared generic ExprId/StmtId
  across unrelated phases).
- Analysis tables are inseparable from the IR they were computed against.
- Pass artifacts must expose only phase-typed accessors, so cross-phase misuse is prevented
  at compile time.
- Generic/raw ID mixing across phase boundaries is prohibited.

## Core v1 features:
Koka's core (System Fw + Effect Rows + evidence passing)
lexical handlers + effect capability hierarchy (refer to order semantics)
single-shot resumptions
monomorphize both generic types and effects
v1 resume surface supports single-shot tail and non-tail use in handler clauses. Current constraints: resume is statement-position only (`let x = resume(v)` / `resume(v)`), resume values are not first-class, and multi-shot/captured resumptions remain deferred.
Ignore the "Equality vs Preorder" math _theory_, but adopt the _mindset_ for your optimizer: "It is safe to replace X with Y if Y has fewer behaviors/effects than X."
Do not add `thunk` to the IR. Implement `is_thunkable(effect_row)` as a helper function in your compiler. Use it to decide if you can move code during Staging.
Distinguish `LocalState` (optimizable) from `SharedState` (volatile/opaque) in your effect definitions.

Handler discharge is independent of value staging. A CT handler erases dispatch 
overhead even when values flowing through its clauses are RT. The handler's control 
flow (which clause to invoke, how to manage continuations, how to thread state) is 
resolved at CT. RT values pass through as residual expressions in the inlined clause 
bodies.

Three calling conventions after handler lowering: PureCall (eliminated), DirectCall 
(tail-resumptive, direct function call), ControlCall (non-TR, CPS with reified 
continuation). Classification is per-clause, not per-handler. Distinct IR node types 
enable pattern-matching in downstream passes.

Handler specialization: when a statically-known handler wraps a function call, 
create a specialized copy with the handler pushed into the function body. Handler 
reduction rules fire inside the copy, eliminating dispatch. Recursive calls are 
tied back to the specialized version. Already-specialized functions are not 
re-specialized (termination guarantee). Runs between residualization and handler 
 lowering. Current v1 scope: direct wrappers (`handle { f(...) }`) and
 wrapper-only bodies (`let` chains + return-forwarding `val` wrappers around
 one direct call) are rewritten to direct calls to specialized copies; broader
 pushdown through complex/block/control-heavy bodies is deferred.

CT-only functions: functions using CT-only effects or TypeInfo arguments cannot 
fall back to RT. RT arguments at call sites are hard errors. Inferrable or 
explicitly declarable.

Three-tier persistability: Trivial (inline), Serializable (constant table), 
NonPersistable (error at boundary). Residualizer uses tier to choose embedding 
strategy.

Knownness split in staging analysis: `KnownLocal` (CT evaluator produced a value) 
vs `KnownPersistable` (value can safely cross CT→RT). This keeps CSP diagnostics 
explicit without requiring full MetaOCaml online/offline machinery in v1.

Target-aware CT evaluation: evaluator uses target word size, endianness, and 
alignment for all arithmetic and memory layout operations. CT cache keyed by 
target spec. Cross-compilation produces correct CT results.

Constant-table embedding policy: pooled constants are structurally deduplicated, 
subject to per-constant and per-compilation-unit size caps. Runtime resources, 
capabilities, raw pointers, and closure values are not embeddable.

ComptimeReadFiles allowed in demand evaluation with file dependency tracking. 
BTA results invalidated when dependency files change. Users can extend demand 
eval allowlist for custom deterministic CT-only effects.

ASCAPE with transparency: automatic staging with proactive diagnostics. The 
compiler reports what was CT-evaluated, why things are RT (provenance chains), 
and suggests what to make CT for maximum impact. @runtime is the escape hatch 
for unwanted CT evaluation. Staging diffs between compilations surface changes.

Purity-relative-to-handler: before applying handler reduction, check if 
computation's effects intersect handler's operations. If not, skip handler 
entirely (apply return clause only, or eliminate if identity).

No multi-stage needed: two stages (CT/RT) confirmed sufficient by MetaOCaml 
experience. Third stage historically used only to guarantee inlining, which 
monomorphization handles.

Rust-style error handling with Result, Option, ?

---

## Core v2 features:
Row polymorphism, for v1 just explicit effect parameters
Handler fusion: compose nested handlers with disjoint effect sets into single 
fused handler. Eliminates intermediate dispatch for handler chains.

Generalized handler specialization with return-clause parameterization: selective 
CPS for cases where return clause varies across recursive call sites.

Incremental staging diagnostics: when recompiling, report expressions whose stage 
changed (CT→RT or RT→CT) with explanation of what caused the change. IDE/LSP feature.

---

## Maybe for v1, maybe for v2:
`@comptime` blocks can be isolated and computed in the background at build time -> save it to disk or smth. While that's happening, they are handled by the runtime. Then replace with computed data once it finishes. This allows the user to keep iterating on the program with `@comptime` directives without having to wait. This should be opt-in to avoid confusion.
Effect composition (unsure, this might be hard)
A set of effect properties / qualifiers(what you already want for effect-qualified opts)  
    Examples:
- `is_opaque_for_staging` (true for IO/FFI/System)
- `is_local_state` vs `is_shared_state` (your LocalState/SharedState split)
- `discardable`, `commutative`, etc.
Nim's optimized arc gc. Overall our memory management model for now is identical to Nim.
---

## Maybe in the future (don't plan for this at all):
Modal effect types (separate effects and functions with modalities): maybe for v2 or v3, out of scope right now because of implementation complexity
Rank-2 effect polymorphism can hide internal handler effects while keeping external APIs “pure,” which matches your staging goals.
Errors as effects, `?` operator, chaining Result<Ok, Err> and regular values with `.`, gauging if errors as effects are a harmonius solution here. For now representing them as types seems optimal.

---

## Other:
pipeline:
lex/parse -> desugar -> type+effect check -> monomorphize (types+effects) -> CT propagation -> BTA/provenance -> residualize -> Handler Specialization -> lower handlers/effects -> closure convert -> ANF/CFG/SSA -> effect-qualified opts -> ARC insertion -> C.

effect annotation lifecycle:
keep effect annotations through type/effect check, monomorphization, CT propagation, and BTA.
erase them after BTA (store only effect summaries/metadata for downstream passes).

Handler lowering sub-pipeline: Evidence passing → Evidence specialization → TR optimization (per-clause) → Selective CPS (ControlCall only) → Standard optimizations

Handler specialization is bounded: each function specialized at most once per 
handler. Static arguments analysis (cf. Effekt's Recursive.scala) identifies 
which arguments are invariant across recursive calls to enable specialization

Notion of "ASCAPE" (escape) = As comptime as possible
For compiler design we use princpiles 1,2,3,12,14,6,10,15,20,23,11,13,18,30 [[compiler-practices]]
