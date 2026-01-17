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
  - IrNode trait for generic traversal across IR types
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
- Each phase struct carries only its own output tables plus tables that downstream passes
  actually read. Tables not read after a phase boundary are dropped.

Specifically:
- MonomorphizationSummary: dropped after Staged (only needed by evaluator).
- StagingTables: dropped after Residualized (outcomes consumed by residualizer).
- CtPropagationTables and BtaTables no longer exist as separate artifacts (fused into
  StagingTables).

## Fused comptime passes (v1, authoritative)

CT propagation and BTA are fused into a single Evaluate+Classify pass.
Residualization and handler specialization are fused into a single
Residualize+Specialize pass. This reduces five comptime passes to three
(Evaluate+Classify → Residualize+Specialize → Normalize).

Rationale: BTA was a pure read-only consumer of CT propagation results.
The evaluator already visits every node and computes all needed
information. Separate passes existed for spec clarity; implementation
fuses them for efficiency and simpler phase chain.

The normalizer uses a shrink-inline-shrink sandwich. Shrinking
reductions are unconditionally safe (dead code, val commutation, constant
branch elimination, beta-reduction of once-used functions). Speculative
inlining is usage-gated and runs once. This separation provides a
termination guarantee for shrinking and explicit control over code growth.

## MLIR patterns (v1, authoritative)

Do not adopt MLIR's dialect/operation system. Two concrete IRs
(Core + Linear) are sufficient for v1 targeting C.

Adopt:
- IrNode trait for generic traversal across Core and Linear IR types.
  Both IRs implement `child_stmts()` / `child_exprs()` through a shared
  trait. Generic walkers (reachability, free variable tracking, usage
  analysis) are written once against the trait.
- Phase structs carry only tables needed downstream (not all ancestor tables).

Reject:
- Full dialect/operation registry (two IRs are sufficient)
- Progressive lowering within a single IR (handler lowering is a clean
  Core → Linear transition)
- Operation/type interface system (enum matching is clearer for two IRs)

## Normalizer policy (v1, authoritative)

Shrinking reductions: always apply, always safe, run to fixpoint.
Speculative inlining: usage-gated, run once, sandwiched between shrinks.
No egg/e-graph/nanopass machinery in v1.

Shrinking reductions are:
1. Dead binding elimination (Never usage)
2. Val-return commutation
3. Val-val flattening
4. Constant branch elimination
5. Known match elimination
6. Beta-reduction of Once-used functions

All decrease or preserve term size. Compose freely. Fixpoint in O(depth)
iterations.

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
re-specialized (termination guarantee). Integrated into the Residualize+Specialize
pass. Current v1 scope: direct wrappers (`handle { f(...) }`) and wrapper-only
bodies (`let` chains + return-forwarding `val` wrappers around one direct call)
are rewritten to direct calls to specialized copies; broader pushdown through
complex/block/control-heavy bodies is deferred.

CT-only functions: functions using CT-only effects or TypeInfo arguments cannot
fall back to RT. RT arguments at call sites are hard errors. Inferrable or
explicitly declarable.

Three-tier persistability: Trivial (inline), Serializable (constant table),
NonPersistable (error at boundary). Residualizer uses tier to choose embedding
strategy.

Knownness split in staging analysis: `Known` (CT evaluator produced a value)
vs `Stuck(reason)` (cannot evaluate, carries immediate reason). Provenance
chains reconstructed on demand by following DependsOnVar links.

Target-aware CT evaluation: evaluator uses target word size, endianness, and
alignment for all arithmetic and memory layout operations. CT cache keyed by
target spec. Cross-compilation produces correct CT results.

Constant-table embedding policy: pooled constants are structurally deduplicated,
subject to per-constant and per-compilation-unit size caps. Runtime resources,
capabilities, raw pointers, and closure values are not embeddable.

ComptimeReadFiles allowed during CT evaluation with file dependency tracking.
Staging results invalidated when dependency files change. Users can extend
CT-allowed effect set for custom deterministic CT-only effects.

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

Partially-static data: struct constructors where some fields are CT and some RT
are represented as partial values, enabling field-level CT evaluation even when
the struct as a whole is partially unknown. Deferred to v2 because it adds a
third Outcome variant and complicates residualizer logic.

Full query architecture for incremental compilation. v1 uses function-level
memoization with cycle detection in the evaluator, extensible to full query
system in v2.

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
Rank-2 effect polymorphism can hide internal handler effects while keeping external APIs "pure," which matches your staging goals.
Errors as effects, `?` operator, chaining Result<Ok, Err> and regular values with `.`, gauging if errors as effects are a harmonius solution here. For now representing them as types seems optimal.

---

## Other:
pipeline:
lex/parse -> desugar -> type+effect check -> monomorphize (types+effects) -> Evaluate+Classify -> Residualize+Specialize -> Normalize -> linearize (handler lowering) -> effect-qualified opts -> ARC insertion -> C.

effect annotation lifecycle:
keep effect annotations through type/effect check, monomorphization, and Evaluate+Classify.
erase them after Residualize+Specialize (store only effect summaries/metadata for downstream passes).

Handler lowering sub-pipeline (within linearize): Evidence passing → Evidence specialization → TR optimization (per-clause) → Selective CPS (ControlCall only) → Standard optimizations

Handler specialization is bounded: each function specialized at most once per
handler. Static arguments analysis (cf. Effekt's Recursive.scala) identifies
which arguments are invariant across recursive calls to enable specialization.

Notion of "ASCAPE" (escape) = As comptime as possible
For compiler design we use principles 1,2,3,12,14,6,10,15,20,23,11,13,18,30 [[compiler-practices]]

TBD:
- How to actually implement performant effects
- Arc gc pass and all optimizations for it. Identify which ones are easier/more difficult under our model/with the data we collect
- Thought-through C emission. Do we just emit SSA, or do we want proper C to let compilers optimize heuristically?
- Syntax sugar that's conducive to an intuitive mental model of effects
- Can we easily extract some benefit from PGO? e.g. if a variable x is allocated in a function, and Usage Analysis shows it never escapes to a Global or Unknown scope:
  - Do not emit New/Retain/Release.
  - Emit struct x_storage; on the C Stack.
  - Nim tries to do this with "Cursor Inference," but often fails on complex control flow. But since we have a Residualizer (Partial Evaluator), we can "unroll" the control flow.
  - If the loop is unrolled at Comptime, the complex lifetime becomes a simple linear lifetime. So we can stack-allocate objects that Nim would heap-allocate, because we can "see" the future (via staging).
  - But can we ONLY stack-allocate objects in Pure and Direct functions? If a function is Control (it yields), its execution suspends.
    - If v0 is on the C Stack: The stack frame is destroyed/popped when we yield (return to the scheduler). The data is lost.
    - If v0 is in the Frame Struct (Arena): The data persists across the yield. - So how do we handle it? hmm