# BTA/Provenance and CT Eval/Residualize: Design Document (Updated)

## Preconditions

Both passes operate on IR **after monomorphization**. All types are concrete. All effect rows are concrete. The call graph is explicit. Handler installations are syntactically visible with known clause implementations. Effect properties are populated in a shared table computed once during effect definition processing.

The type checker has already produced:
- `type_of: ExprId → TypeId`
- `effects_of: ExprId → EffectRowId`
- `effect_label_props: EffectLabel → EffectProperties`
- `persistability_of: TypeId → Persistability`

---

## High-Level Logic and Considerations

### Three independent factors determine staging

A computation's stage depends on the conjunction of three checks with different domains, computed from different sources, producing different diagnostics.

**Input availability.** Are all values this expression depends on known at compile time? Dataflow: literals are CT, parameters from RT call sites are RT, compound expressions join sub-expression stages.

**Effect eligibility.** Can all effects this expression might perform be resolved at compile time? Depends on capability level of each effect in the row AND whether a CT handler is in scope for effects that require one. Effect dispatch eligibility is independent of value staging — a CT handler discharges dispatch overhead even when values flowing through clauses are RT.

**Cross-stage persistence.** If a value must cross from CT into RT, is its type suitable? Checked only at stage boundaries, not every expression.

### Handler discharge is separable from value staging

When BTA encounters a handler installation where the handler is CT, the handler construct is erased in the residual regardless of whether the body's values are CT or RT. The handler's control flow — dispatch, continuation management, state threading — is resolved at CT. RT values pass through inlined clause bodies as residual expressions.

BTA classifies two things independently for each handler:
1. Is the handler dischargeable? (All captures CT? Clause bodies expressible without RT-only effects?)
2. What is the stage of the overall handled expression? (Depends on body's value stages after handler discharge.)

A handler can be dischargeable (erased from residual) while the overall expression remains RT.

### Branch elimination uses pre-computed CT cache

When a branch condition is CT, BTA needs its value to determine which branch
is live. This value comes from the CT propagation pass, which runs BEFORE BTA.

CT propagation evaluates all expressions that are transitively CT (pure 
functions of CT inputs, CT-allowed effects with CT handlers in scope). It 
caches results keyed by ExprId. When it encounters a branch with a CT-known 
condition, it evaluates only the live branch and records the decision.

BTA then reads these cached values. It never evaluates anything — it only 
classifies. When it encounters a branch, it checks the cache for a branch 
decision. If present, it analyzes only the live branch. If absent (condition 
wasn't evaluable during CT propagation), it conservatively analyzes both 
branches.

This separation means BTA is a pure classification pass with no side effects, 
no rollback, and deterministic behavior regardless of evaluation costs.

### CT propagation allowed effects

The CT propagation pass permits evaluation of expressions performing effects 
from a configurable set: `{ Pure, Diverge, Alloc, LocalState, ComptimeReadFiles }`.

When `ComptimeReadFiles` is exercised, dependencies are recorded as
normalized path identity + content hash. If the hash changes, CT propagation
re-runs.

Users can extend the allowed set for custom CT-only effects that are 
deterministic given their inputs. An effect is allowed during CT propagation 
if it is (a) CT-only or eliminable, AND (b) deterministic given its inputs, 
AND (c) a CT handler is in scope.

Effects outside this set cause CT propagation to stop at that expression — 
it remains unevaluated and BTA will classify it based on structural analysis 
(effect row, input stages).

### CT evaluator is a tree-walking interpreter

The evaluator is a state machine that steps through Core IR statements. Each step takes
a `(Stmt, Env, Stack, Heap)` and produces the next state. No recursion on the host stack —
deep CT evaluation doesn't overflow.

Effect handling: Reset pushes a prompt on the stack. Shift captures frames up to the prompt.
Resume reinstates captured frames. This lets the evaluator run effectful CT code when a
CT handler is in scope.

Mutable state: Var/Get/Put operations use stack frames with fresh addresses, not host-language
mutation. This makes rollback clean — restore the stack and heap, and state changes are undone.

### Concrete effects assertion

At BTA entry, assert that all effect rows in the input IR have zero unresolved unification
variables. After monomorphization, everything must be concrete. If this assertion fails,
it's a compiler bug, not a user error. This prevents the BTA from silently making wrong
staging decisions based on incomplete effect information.

### Provenance tracks the immediate reason

Each RT classification carries a single reason enum value, not a chain. When diagnostics need full chains, the renderer walks IR dependency structure collecting reasons from each node's stage entry. Per-node storage stays O(1). Arbitrary-depth explanations are produced on demand.

### Three-tier persistability

Values crossing stage boundaries are classified by type:

- **Trivial**: Int, Bool, Char, Float, String, small enums. Inline literals. Cheap to duplicate.
- **Serializable**: Structs/arrays/ADTs of persistable fields. Embeddable via constant table for large values. Duplication has cost.
- **Non-persistable**: Closures capturing RT values, file handles, OS resources, effect capabilities, raw pointers. Cannot cross.

BTA uses this for cross-stage persistence checks. The residualizer uses tier distinction for embedding strategy.

Knownness is tracked separately from staging:
- **KnownLocal**: CT evaluator computed a value for this expression in the current build.
- **KnownPersistable**: known local value + persistable type, so it is eligible for CT→RT embedding.

This distinction improves diagnostics without requiring full online/offline CSP machinery in v1.

### CT-only functions

Functions using CT-only effects (`ComptimeReadFiles`), taking `TypeInfo` arguments, or explicitly annotated CT-only cannot fall back to RT. If BTA determines a CT-only function is called with RT arguments, this is a hard error.

### Target-aware evaluation

The evaluator simulates target semantics, not host. Integer arithmetic uses target-width types (wrapping/sign-extension to target word size). Byte reinterpretation uses target endianness. The evaluator takes a `TargetSpec` and all memory layout operations consult it. CT cache entries are keyed by target spec + evaluator policy + compiler version.

Current caveat: floating point still uses host behavior when exact target emulation is unavailable.

v1.1 note:
- Integer literals are normalized to target word width during CT folding (not only arithmetic results).
- `CtEvalStats.folded_float_host` tracks float folds that currently rely on host FP behavior.

### Type-level CT is a separate, earlier phase

Type-level CT computation (struct-from-fields, trait derivation, TypeInfo manipulation) happens during type checking, before monomorphization. BTA/residualize passes deal only with value-level CT computation.

---

## Data Structures (Code is approximate, for demonstration, subject to change)

### Shared Definitions

```
enum Persistability {
    Trivial,
    Serializable,
    NonPersistable,
}

enum CapLevel {
    Pure, Diverge, Alloc, LocalState, SharedState, IO, FFI,
}

struct EffectProperties {
    cap_level: CapLevel,
    is_local: bool,
    is_discardable: bool,
    is_commutative: bool,
    cardinality: Cardinality,
    ct_only: bool,
}

enum Cardinality {
    Preserving,   // State, Reader, Writer, Exception: one-in one-out or zero
    Changing,      // Nondeterminism, coroutines, amb: one-in many-out
}

struct TargetSpec {
    word_size: u8,
    endianness: Endian,
}
```

### BTA Output

```
struct BtaResults {
    expr_stage: Map<ExprId, Stage>,
    var_stage: Map<VarId, Stage>,
    handler_discharge: Map<HandlerId, HandlerDischarge>,
    branch_decisions: Map<ExprId, BranchDecision>,
    ct_cache: Map<ExprId, Value>,
    func_ct_only: Set<FuncId>,
    file_deps: Vec<(PathBuf, FileHash)>,
    diagnostics: Vec<BtaDiagnostic>,
}

enum Stage {
    CT,
    RT(Reason),
}

enum Reason {
    Parameter(FuncId, ParamIndex),
    DependsOn(VarId),
    EffectNotDischarged(EffectLabel),
    HandlerIsRT(HandlerId, VarId),
    BranchOnRT(ExprId),
    NotPersistable(TypeId),
    FuelExhausted(ExprId, u64),
    SizeExhausted(ExprId, u64),
    UserAnnotatedRT,
    CTOnlyWithRTArgs(FuncId, Vec<VarId>),
}

struct HandlerDischarge {
    dischargeable: bool,
    discharge_reason: Option<Reason>,
}

enum BranchDecision {
    LiveTrue,
    LiveFalse,
    DemandEvalFailed,
}
```

### BTA Context

```
struct BtaContext {
    // Inputs (read-only, from CT propagation)
    ct_cache: Map<ExprId, Value>,
    branch_decisions: Map<ExprId, BranchDecision>,
    
    // State (built during BTA walk)
    results: BtaResults,
    handler_stack: Vec<HandlerEntry>,
    in_progress: Set<FuncId>,           // recursion detection
    analyzed: Map<FuncId, FuncStagingSummary>,
}

struct HandlerEntry {
    effect_label: EffectLabel,
    handler_id: HandlerId,
    dischargeable: bool,
}

struct FuncStagingSummary {
    all_ct_result: Stage,
    ct_only: bool,
}
```

### Evaluator

```
struct Evaluator {
    target: TargetSpec,
    ct_cache: Map<ExprId, Value>,
}

enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Char(char),
    Str(InternedString),
    Struct(Vec<Value>),
    Enum(Tag, Vec<Value>),
    Array(Vec<Value>),
    Closure { params: Vec<ParamId>, body: ExprId, env: Map<VarId, Value> },
    Builtin(BuiltinFn),
    Unit,
}
```

### Residualizer Output

```
struct ResidualProgram {
    functions: Map<FuncId, ResidualFunction>,
    constant_table: Vec<ConstantEntry>,
    func_effect_summary: Map<FuncId, EffectRow>,
    diagnostics: Vec<ResidualDiagnostic>,
}

struct ConstantEntry {
    id: ConstId,
    value: Value,
    typ: TypeId,
    size_bytes: u64,
    file_deps: Vec<(PathBuf, FileHash)>,
}
```

---

## BTA Algorithm

### Entry Point

Analyze from program entry points. For `main`, all parameters are RT.

### Expression Analysis

For each expression, check annotations first (`@runtime` forces RT, `@comptime` forces CT with error if impossible), then dispatch by expression kind.

**Literals**: always CT. Cache value immediately.

**Variables**: look up in `var_stage`.

**Binary/unary operations**: join of operand stages.

**Let-bindings**: analyze RHS, record variable stage, analyze body.

**Branches (if/match)**: Look up condition in `branch_decisions` (from CT 
propagation). If `LiveTrue` or `LiveFalse`, analyze only the live branch. 
Dead branch is never analyzed — its effects don't contribute to staging. 
If no cached decision (condition wasn't CT-evaluable), analyze both branches 
conservatively. Stage = join of analyzed branches. If condition is RT, 
reason = BranchOnRT.

**Function calls (three-tier)**:

Tier 1 — any RT argument → result is RT. Most common, cheapest check.

Tier 2 — all args CT, check function's effect row. If empty (pure) → 
check ct_cache for result. If cached → CT. If not cached (fuel exhausted, 
too complex) → still CT-eligible but value unknown until residualization.
If effects present but all CT-eligible with CT handlers in scope → same 
check.

Tier 3 — all args CT, effects present, no CT handler → RT. Or: function 
body has paths with non-CT-eligible effects that weren't eliminated by 
branch decisions → RT.

CT-only functions: if any arg is RT, hard error.

**Handler installations**: Compute handler discharge independently from body staging. Push handler onto stack. Analyze body. Pop handler. If handler dischargeable, expression stage = body stage. If not dischargeable, expression stage = body stage (handler kept for lowering). Handler discharge recorded in side table for residualizer.

Handler discharge check: all captures CT, all clause bodies use only CT-eligible effects.

**Effect operations**: Find nearest handler on stack. If handler dischargeable and all args CT → CT (fully evaluated). If handler dischargeable but args RT → RT, but dispatch is CT (residualizer inlines clause body with RT slots). If handler not dischargeable → RT. If no handler and cap_level < IO → CT-eligible without handler. If no handler and cap_level ≥ IO → RT.

**Lambdas**: Stage = join of capture stages, gated by persistability. All captures CT and type persistable → CT. Any capture RT → RT. Type non-persistable → RT (unless all captures are CT and of persistable types).

**@comptime blocks**: Every outer variable reference must be CT and persistable. Effects restricted to CT-eligible set.

### Recursion Handling

Optimistic assumption: CT. After analysis, check if assumption held. If wrong, re-analyze with RT. Converges in ≤ 2 iterations per SCC.

### Post-Walk Suggestions

Single pass over root-cause RT variables. For each root cause, count tainted downstream expressions via reachability. Sort by taint count descending. Report top N. One graph traversal per root cause.

---

## CT Eval/Residualize Algorithm

### Entry Point

Residualize only functions with at least one RT call site. Functions called only from CT contexts produce no residual code.

### Expression Residualization

**CT expressions**: Evaluate (or retrieve from cache) and embed as constant using three-tier strategy:
- Trivial, used once → inline literal
- Trivial, used multiple times → inline (cheap to duplicate)
- Serializable, used once, small → inline structured literal
- Serializable, used multiple times or large → let-binding referencing constant table

Use-count analysis before emission (pre-pass over function body or emit let-bindings eagerly, rely on later copy-propagation for trivial cases).

**RT expressions**: Emit code with CT sub-expressions recursively residualized (may become literals).

**Branches with CT condition**: Emit only live branch body. Dead branch, condition, and branch construct all eliminated.

**Matches with CT-known constructor scrutinee**: Select the matching arm (or default),
erase the match node, and materialize binder values as `let` bindings before the selected
arm body. This keeps binder semantics correct without carrying match control flow to
runtime.

**Handler blocks with discharged handler**: Handler construct erased. Effect operations targeting this handler are inlined with clause bodies. For tail-resumptive clauses, resume becomes a no-op — the continuation is the surrounding code. For non-TR clauses in a discharged handler, the entire operation was CT-evaluated; embed as constant.

**Handler blocks with non-discharged handler**: Kept in residual for handler lowering.

**Purity-relative-to-handler**: Before processing a handler's body, check if the body's effect set intersects the handler's handled operations. If empty intersection, apply only the return clause (or skip the handler entirely if return clause is identity). This is the With-Pure optimization.

### Effect Summary Recomputation

After residualization, walk residual function bodies and collect remaining EffectOp nodes. Their union is the residual effect row. This updated summary replaces original annotations for downstream passes.

### Full Evaluator

Tree-walking interpreter using target semantics. Handles algebraic effects via internal handler stack. Single-shot resumption (continuation = return point in recursive eval call stack). Tracks fuel and size. Records file dependencies for `ComptimeReadFiles` operations.

### Interface Between CT Propagation and BTA

CT propagation runs first and produces:
- `ct_cache: Map<ExprId, Value>` — evaluated CT values
- `branch_decisions: Map<ExprId, BranchDecision>` — which branches are live
- `file_deps: Vec<(PathBuf, FileHash)>` — build dependencies
- `diagnostics: Vec<CtDiagnostic>` — fuel/size warnings

BTA consumes these as read-only inputs. It never modifies them. BTA produces:
- `expr_stage: Map<ExprId, Stage>` — CT or RT(reason) per expression
- `var_stage: Map<VarId, Stage>` — per variable
- `handler_discharge: Map<HandlerId, HandlerDischarge>` — per handler

The residualizer then consumes both: CT cache for embedding constants, BTA 
results for knowing what to residualize vs. what to emit as code.

Data flow: CT Propagation → (ct_cache, branch_decisions) → BTA → (stages) → Residualizer

---

## Handler Specialization (Post-Residualize)

After residualization, some handlers remain in the IR (non-discharged, or discharged but wrapping function calls where the function body contains effect operations). Handler specialization creates specialized copies of functions with handlers pushed into their bodies.

For `handle f(v) with h` where h is statically known:
1. Check purity-relative-to-handler: if f's effects don't intersect h's operations, apply return clause only
2. Otherwise: create specialized `f'` with h pushed into body, apply handler reduction rules within body, tie recursive knots (replace `handle f(v') with h` inside f' with `f'(v')`)
3. Don't re-specialize already-specialized functions (termination guarantee)

Current v1 implementation rewrites both direct wrappers and wrapper-only bodies that structurally forward into one direct call (e.g. `let` chains + return-forwarding `val` wrappers). Block/control-heavy wrapper bodies remain deferred.

When the return clause varies across recursive call sites (With-Do fires), v1 aborts specialization for that case. V2 adds generalized specialization with return-clause parameter (selective CPS).

Position in pipeline: between residualization and handler lowering.

---

## Calling Convention Classification (Handler Lowering Input)

After handler specialization, remaining effect operations are classified into three calling conventions:

- **Pure**: operation was fully discharged (no residual) → no IR node emitted
- **Direct**: handler clause is tail-resumptive → function call, no continuation capture. Corresponds to Effekt's `ImpureApp` / our evidence-passing direct call
- **Control**: handler clause is non-tail-resumptive → CPS transform, continuation reified as closure. Corresponds to Effekt's `App` with reset/shift

These three conventions should be distinct IR node types after handler lowering, enabling downstream passes to pattern-match on calling convention without re-deriving it.

---

# Features.md Additions

## V1 Additions

### Three Calling Conventions After Handler Lowering

After handler lowering, effect operations become one of three IR node types:

- **PureCall**: operation fully eliminated (discharged at CT or dead code). No IR node remains.
- **DirectCall**: tail-resumptive handler clause. Compiled as a regular function call with evidence/capability argument. No continuation capture. Covers ~90% of effect operations in practice (State get/put, Reader ask, Writer tell, Exception raise).
- **ControlCall**: non-tail-resumptive handler clause. Continuation reified as a closure. CPS transform applied only to the function containing this call, not globally.

This classification is per-clause, not per-handler. A single handler can have both DirectCall and ControlCall clauses. The per-clause classification replaces the previous per-handler binary (TR vs non-TR).

### Handler Specialization

When a handler wraps a recursive function call, the compiler creates a specialized copy of the function with the handler pushed into its body. This enables handler reduction rules to fire on each operation inside the function, eliminating dispatch overhead.

```
// Before specialization:
with state_handler handle loop(1000)

// After specialization:
fn state_handler_loop(m: Int, s: Int) -> Int {
    if m == 0 { s }
    else { state_handler_loop(m - 1, s + 1) }
}
state_handler_loop(1000, 0)
```

Termination: already-specialized functions are not re-specialized. Generalized specialization (parameterizing by return clause) is v2.

### Purity-Relative-to-Handler

When a computation's effect set doesn't intersect a handler's handled operations, the handler's effect clauses are irrelevant. Only the return clause applies. If the return clause is identity, the handler is eliminated entirely.

Check: intersect computation's effect row with handler's operation set. If empty, skip all handler machinery.

### ComptimeReadFiles in Demand Evaluation

`ComptimeReadFiles` is allowed during demand evaluation (BTA branch condition evaluation). When exercised, normalized paths and content hashes are recorded as dependencies on BTA results. Content-hash changes trigger BTA re-run. Users can extend the demand evaluation allowlist for custom deterministic CT-only effects.

### Three-Tier Persistability

Values crossing CT→RT boundary are classified:

- **Trivial** (Int, Bool, Char, Float, String, small enums): inline as literal, cheap to duplicate
- **Serializable** (structs/arrays/ADTs of persistable fields): constant table for large or multiply-used values (dedup + size caps)
- **Non-persistable** (closures over RT values, handles, capabilities): cannot cross, error at boundary

### CT-Only Functions

Functions using CT-only effects or taking TypeInfo arguments are CT-only. Calling with RT arguments is a hard error, not a staging suggestion. Inferrable from effect usage or explicitly declarable.

### V1 Plan Notes (synced from `plan.md`)

- Keep slices independently testable and end-to-end runnable (`--emit-c --run-c`).
- Prefer conservative semantics first; optimize after invariants are explicit.
- Handler pipeline closure notes:
  - 2026-02-17: resume single-shot check is path-sensitive (branch-exclusive single resumes are accepted; same-path double resumes are diagnosed).
  - 2026-02-17: tail-resumption checking is memoized and treats stmt cycles as non-tail conservatively.
  - 2026-03-16: boundary diagnostics for non-persistable crossings anchor to runtime boundary stmt spans and include stmt ids.
- Handler specialization notes:
  - 2026-02-17: reachable pruning + `FuncId` remapping landed; tests assert cross-table id integrity.
  - Scope remains bounded in v1 (wrapper/control-light pushdown only).
- Persistability boundary notes:
  - 2026-03-16: residualizer gates CT embedding on BTA stage (`Ct`) + knownness (`KnownPersistable`), preventing runtime-forced/boundary-rejected embeddings.
  - 2026-03-18: C emitter pools repeated runtime scalar literals (`Int`/`Bool`/`Char`) as `static const CieloValue`.
  - 2026-03-27: scalar/ctor pooling now includes finite `Float` literals with deterministic bit-pattern keys.
  - 2026-04-04: non-persistable boundary diagnostics now report exact boundary roles (for example `call-arg#0`, `return-value`) in addition to statement ids/spans.
- Residual effect-summary notes:
  - 2026-04-05: residualizer now recomputes `FuncId -> EffectRow` from rewritten residual IR (fixpoint over call edges + handler subtraction), then rewrites call-site effect rows from that result.
- Incremental staging/invalidation notes:
  - 2026-03-28: staging snapshot IDs now use span+kind+structural expression fingerprint (index-free), reducing false churn under expr-index renumbering.
  - 2026-03-28: `ComptimeReadFiles` invalidation reasons are persisted in a sidecar snapshot (`.ctdeps.tsv`) and surfaced as explicit key/dependency deltas (added/removed/changed + cache-key field changes).
  - 2026-04-03: CT propagation now supports a persistent query-cache sidecar (`.ctquery.tsv`) keyed by `{CtCacheKey + normalized file deps + program fingerprint}` and restores cached CT results when keys match.
- CT evaluator notes:
  - 2026-04-05: CT fold coverage now includes float arithmetic/comparison/equality and non-numeric equality (`Bool`/`Char`/`String`/`Unit`); `folded_float_host` counts all host-float folds, not only unary negation.
  - 2026-04-06: host-float folds are finite-only. Non-finite inputs/results (`NaN`/`Inf`) are left unresolved to keep cross-target behavior conservative until strict FP emulation lands.

## V2 Additions

### Handler Fusion

When multiple handlers are syntactically nested and handle disjoint effects with statically known implementations, compose them into a single fused handler that threads all state simultaneously. Eliminates intermediate dispatch. Empirically yields 4-24x speedups on handler-heavy code (search transformers, constraint programming).

### Generalized Handler Specialization

When the return clause varies across recursive call sites within a specialized function, parameterize the specialized function by the return clause (pass as continuation argument). This is selective CPS applied to the specialization, covering cases like list-building with handlers where each recursive call wraps the result differently.

### V2 Carry-Over Notes (from `plan.md`)

- Generalized handler specialization by return-clause parameterization is deferred to v2 selective CPS.
- If mutable-variable stmt forms are added to Core IR, tail-resumption gating must mirror the mutable-state caveat from `RemoveTailResumptions`.
- Persistability/codegen expansion still pending beyond v1 scalar/string pooling:
  - structural constant pooling for ADT/aggregate serializable values
  - non-finite float pooling policy (NaN/Inf handling if adopted)
