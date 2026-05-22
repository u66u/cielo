# Evaluate+Classify and Residualize+Specialize: Design Document

## Preconditions

Both passes operate on IR **after monomorphization**. All types are concrete.
All effect rows are concrete. The call graph is explicit. Handler installations
are syntactically visible with known clause implementations. Effect properties
are populated in a shared table computed once during effect definition processing.

The type checker has already produced:
- `type_of_expr: ExprId → TypeId`
- `effects_of_expr: ExprId → SortedEffectRow`
- `effect_label_props: EffectLabelId → EffectProperties`
- `persistability_of_type: TypeId → Persistability`

Concrete effects assertion: at pass entry, assert all effect rows have zero
unresolved unification variables. After monomorphization, everything must be
concrete. If this assertion fails, it's a compiler bug, not a user error.

---

## High-Level Architecture

Three passes replace the previous five (CT propagation, BTA, residualize,
handler specialize, normalize):

1. **Evaluate+Classify** — single recursive descent that evaluates CT
   expressions and classifies stages simultaneously.
2. **Residualize+Specialize** — single pass that embeds constants, erases
   discharged handlers, and performs handler specialization.
3. **Normalize** — shrink-inline-shrink sandwich for cleanup.

---

## Three Independent Factors Determine Staging

A computation's stage depends on the conjunction of three checks with
different domains, computed from different sources, producing different
diagnostics.

**Input availability.** Are all values this expression depends on known at
compile time? Dataflow: literals are CT, parameters from RT call sites are
RT, compound expressions join sub-expression stages.

**Effect eligibility.** Can all effects this expression might perform be
resolved at compile time? Depends on capability level of each effect in the
row AND whether a CT handler is in scope for effects that require one. Effect
dispatch eligibility is independent of value staging — a CT handler discharges
dispatch overhead even when values flowing through clauses are RT.

**Cross-stage persistence.** If a value must cross from CT into RT, is its
type suitable? Checked only at stage boundaries, not every expression.

---

## Pass 1: Evaluate + Classify

Single recursive descent over post-monomorphization Core IR. At each node,
produce one of two outcomes:

```
enum Outcome {
    Known(Value),       // fully evaluated, stage = CT
    Stuck(Reason),      // cannot evaluate, stage = RT(reason)
}
```

The walker carries an environment mapping variables to Outcomes.

### Evaluation Rules

**Literals** → `Known(value)`. Always.

**Variables** → look up in environment. Propagate outcome directly.

**Pure operations (Expr nodes)** → all inputs Known → evaluate with
target-aware arithmetic → `Known(result)`. Any input Stuck →
`Stuck(DependsOnVar(first_stuck_var))`. Record which sub-expressions
were Known for later use by the residualizer (allows emitting literals
for known sub-expressions within an overall Stuck expression).

**Struct/enum constructors (MakeStruct/MakeEnum)** → all fields Known →
`Known(Struct(fields))` or `Known(Enum(tag, fields))`. Any field Stuck →
`Stuck`. Known fields are recorded in a side table for the residualizer
to emit as literals within the constructor call.

**Branches (If/Match)** → evaluate condition. If `Known(Bool(true/false))`,
walk only the live branch. Record `BranchDecision::LiveTrue` or
`BranchDecision::LiveFalse`. Dead branch is never walked — its effects
do not exist, its variables are not classified, it generates no residual
code. If condition is Stuck, walk both branches conservatively. Stage =
`Stuck(BranchOnRuntime(cond_id))`.

**Match on Known constructor** → select the matching arm, bind pattern
variables to the constructor's field values as Known, walk the arm body.
Dead arms are not walked.

**Function calls** → three tiers, executed inline during the walk:

Tier 1 — any argument Stuck → result is `Stuck(DependsOnVar(arg))`. If
callee is ct_only → **hard error**, not a staging fallback.

Tier 2 — all arguments Known, function is pure or has only CT-eligible
effects with CT handlers in scope → evaluate the call by entering the
function body with arguments bound in the environment. Fuel counter
decremented per step. If fuel exhausted → `Stuck(FuelExhausted(expr, fuel))`.

Tier 3 — all arguments Known but effects not CT-eligible (no CT handler
in scope, or effect is IO/FFI/SharedState) → `Stuck(EffectNotDischarged(label))`.

**Handler installations (Handle)** → analyze per-clause discharge, push
handler onto capability stack, walk body, pop handler.

Per-clause discharge analysis:

```
enum ClauseDischargeStatus {
    Discharged,       // CT-evaluable: all captures Known, clause body CT-eligible
    TailResumptive,   // not CT-evaluable but tail-resumptive → DirectCall at RT
    NeedsCPS,         // needs full CPS → ControlCall at RT
}
```

If all clauses Discharged → handler is fully dischargeable. The handler's
effect operations are evaluated inline by the CT evaluator. If not all
clauses Discharged → handler is kept. Expression outcome = body outcome
in both cases. Handler discharge recorded in side table for residualizer.

A handler can be fully dischargeable while the overall expression remains
RT (handler erases dispatch overhead, but body produces RT values).

**Effect operations (Perform)** → find nearest handler on capability stack
for matching effect label. If clause is Discharged and all args Known →
evaluate clause body → Known result. If clause is Discharged but args
include Stuck → Stuck, but mark operation for clause-body inlining during
residualization (dispatch overhead is CT even though values are RT). If
clause is not Discharged → `Stuck(EffectNotDischarged(label))`. If no
handler in scope and effect cap_level < IO → CT-eligible without handler.
If no handler and cap_level ≥ IO → `Stuck(EffectNotDischarged(label))`.

**Resume** → within a discharged handler's clause evaluation, resume
reinstates the continuation. The evaluator continues evaluating the
handler body from the point of the Perform. Single-shot: each resume
consumed exactly once.

**@comptime blocks (Stage directive)** → push restricted capability scope.
Only CT-eligible effects allowed: `{Pure, Diverge, Alloc, LocalState,
ComptimeReadFiles}`. Every outer variable reference checked:
`outcomes[x]` must be Known with persistable type. Non-Known or
non-persistable → hard error with explanation. Evaluate body. Check
result is persistable.

**Lambdas/closures** → classify captures individually. All Known +
persistable type → Known(Closure). Any Stuck capture → Stuck(DependsOnVar).

### CT Evaluator State Machine

The evaluator is a state machine over Core IR statements. No host-stack
recursion — deep CT evaluation does not overflow.

```
enum EvalState {
    Done(Outcome),
    Step(StmtId, Env, Stack, Heap),
    FuelExhausted(StmtId, u64),
    SizeExhausted(StmtId, u64),
}
```

Each `step()` takes a Step and returns the next state. Fuel decremented
per step. Size tracked on data allocation.

### Target-Aware Evaluation

Integer arithmetic uses target-width wrapping (e.g. `i64::MIN / -1`
wraps on 64-bit targets). Byte reinterpretation uses target endianness.
Float uses host behavior with tracking counter (`folded_float_host`).
Non-finite float inputs/results (NaN/Inf) are left unresolved. Float `%`
is never folded, because `%` is Int-only and `cv_mod` traps on floats.

CT cache keyed by: target spec + evaluator policy + compiler version.

v1.1 note:
- Integer literals normalized to target word width during CT folding.
- Target query builtins fold immediately from TargetSpec:
  `target_word_size_bits()`, `target_pointer_alignment()`, `target_is_big_endian()`.

### Mutable State in Evaluator

Var/Get/Put operations use stack frames with fresh addresses in the
evaluator, not host-language mutation. Get on a Stuck variable →
`Stuck(DependsOnVar)`. Put of Stuck value makes variable Stuck
permanently for that execution path. Conservative but correct.
Flow-sensitive tracking deferred to v2.

### Handler Effects in Evaluator

Reset pushes a new prompt on the evaluator's prompt stack. Shift captures
frames up to the prompt. Resume reinstates captured frames. This lets the
evaluator run effectful CT code when a CT handler is in scope.

### Recursion Handling

When encountering a recursive function, use fuel-limited evaluation
directly. No optimistic/pessimistic re-analysis cycle. Evaluation either
succeeds within fuel or produces `Stuck(FuelExhausted)`.

Function-level memoization: cache `(FuncId, Vec<Value>) → Outcome`.
Cycle detection via in-progress set. When entering a function already
in-progress → rely on fuel counter to bound evaluation. This is the
minimal query pattern, extensible to a full query system in v2.

### Provenance

Each Stuck carries a single `Reason` enum value, not a chain. Full
provenance chains are reconstructed on demand by following `DependsOnVar`
links through the outcomes table. Per-node storage is O(1). Rendering
walks the dependency structure to produce arbitrary-depth explanations.

### CT-Allowed Effects

The evaluator permits evaluation of expressions performing effects from
a configurable set: `{Pure, Diverge, Alloc, LocalState, ComptimeReadFiles}`.

When `ComptimeReadFiles` is exercised, dependencies are recorded as
normalized path + BLAKE3 content hash. If the hash changes, staging
results are invalidated.

Users can extend the allowed set for custom CT-only effects that are
deterministic given their inputs.

Effects outside this set cause the evaluator to produce Stuck at that
expression. The surrounding context inherits this.

### Post-Walk Suggestions

Single pass over root-cause Stuck variables (those whose Reason is
`Parameter` or `UserForcedRuntime`). For each root cause, BFS over
dependents via `DependsOnVar` links counting tainted expressions. Sort
by taint count descending. Report top N.

### Output

```
struct StagingTables {
    outcomes: DenseMap<ExprId, Outcome>,
    var_outcomes: DenseMap<VarId, Outcome>,
    handler_analysis: DenseMap<HandlerId, HandlerAnalysis>,
    branch_decisions: DenseMap<ExprId, BranchDecision>,
    usage: DenseMap<VarId, Usage>,
    file_deps: Vec<CtFileDep>,
    cache_key: CtCacheKey,
    eval_stats: CtEvalStats,
}

struct HandlerAnalysis {
    clause_statuses: Vec<ClauseDischargeStatus>,
    fully_dischargeable: bool,
}
```

---

## Pass 2: Residualize + Specialize

Input: Core IR + StagingTables. Output: residual Core IR with CT parts
embedded as constants, discharged handlers erased, and remaining handlers
pushed into function bodies where possible.

### Entry Point

Residualize only functions with at least one RT call site or reachable
from entrypoints. Functions called only from CT contexts and fully
evaluated produce no residual code.

### Expression Residualization

**Known nodes** → embed as constant using three-tier strategy:
- Trivial (Int, Bool, Char, Float, String, small enums), any use count
  → inline literal.
- Serializable (structs/arrays/ADTs of persistable fields), single use,
  small → inline structured literal.
- Serializable, multi-use or large → constant table reference.

Use-count analysis before emission: pre-pass or rely on usage from
StagingTables.

**Stuck nodes** → emit code. Sub-expressions that are Known become
literals within the emitted code.

**Dead branches** → `BranchDecision::LiveTrue/LiveFalse` → emit only
the live branch body. Erase if/match construct, condition, and dead
branch.

**Match with Known constructor scrutinee** → select matching arm, erase
match node, materialize binder values as `let` bindings before the
selected arm body.

### Handler Residualization (integrated with specialization)

**Fully discharged handler** → erase the Handle node. Effect operations
targeting this handler: if all args were Known → embed evaluated result.
If clause was discharged but args Stuck → inline clause body with Stuck
slots as residual expressions. For tail-resumptive discharged clauses,
resume becomes a no-op (continuation is the surrounding code). For
non-TR discharged clauses, the entire operation was CT-evaluated.

**Non-discharged handler wrapping a function call** → handler
specialization fires inline:

1. Check purity-relative-to-handler: intersect computation's effect row
   with handler's operation set. If empty → apply return clause only, or
   eliminate entirely if return clause is identity.
2. If specializable wrapper (direct call, or let-chain + return-forwarding
   val wrappers around one direct call) → create specialized copy with
   handler pushed into body, apply handler reduction rules within body,
   tie recursive knots (replace `handle f(v') with h` inside specialized
   copy with `f_specialized(v')`).
3. Don't re-specialize already-specialized functions (termination
   guarantee: tracked via `Set<(FuncId, HandlerId)>`).
4. Fall through: keep handler in residual for linearize.

**Non-discharged, not specializable** → kept in residual for handler
lowering in linearize pass.

### Effect Summary Recomputation

Walk residual function bodies, collect remaining Perform/effect operation
nodes. Their union is the residual effect row per function. Stored in
`ResidualTables.function_effect_summary`. Replaces original effect
annotations for all downstream passes.

### Constant Table

```
struct ConstantTable {
    entries: Vec<ConstantEntry>,
    dedup: HashMap<u64, Vec<ConstId>>,  // structural hash → entry indices
    entry_cap: u64,                      // per-entry size limit (bytes)
    unit_cap: u64,                       // per-compilation-unit size limit
    total_size: u64,
}

struct ConstantEntry {
    id: ConstId,
    value: Value,
    typ: TypeId,
    size_bytes: u64,
    strategy: EmbedStrategy,
}

enum EmbedStrategy {
    InlineLiteral,     // emit as C literal
    StaticConst,       // emit as `static const` declaration
    Pooled(PoolId),    // reference shared constant in .rodata
}
```

Embedding decision: Trivial → InlineLiteral. Serializable, single use,
small → InlineLiteral or StaticConst. Serializable, multi-use or large
→ Pooled with structural dedup. Scalar/ctor pooling includes finite
Float literals with deterministic bit-pattern keys.

Non-persistable values are caught at classification time and never reach
the constant table.

### Output

```
struct ResidualTables {
    function_effect_summary: HashMap<FuncId, SortedEffectRow>,
    constant_table: ConstantTable,
}
```

---

## Pass 3: Normalize (shrink-inline-shrink sandwich)

Runs after Residualize+Specialize. May run again after linearize if
needed.

### Phase A: Shrinking Reductions (to fixpoint)

Always safe, always profitable, always terminates:

1. **Dead binding**: `Let/Val` where binding has `Usage::Never` → drop,
   keep continuation.
2. **Val-return**: `Val { x, Return(e), body }` → `Let { x, e, body }`.
3. **Val-val flattening**: `Val { x, Val { y, s1, s2 }, s3 }` →
   `Val { y, s1, Val { x, s2, s3 } }`.
4. **Constant branch**: `If(Literal(Bool(true)), A, B)` → `A`.
5. **Known match**: `Match(MakeEnum(tag, args), arms)` → matching arm
   with `Let` bindings for pattern variables.
6. **Beta-reduce Once-used**: `Val/Call` targeting a Once-used function →
   inline body with args substituted.

Properties:
- Every rule decreases or preserves term size.
- Rules compose: applying one may enable another.
- Fixpoint reached in O(depth) iterations.
- No risk of code blowup.

Implementation: in-place replacement in the stmt arena. Freed nodes
become unreferenced (no compaction needed for v1).

### Phase B: Speculative Inline (once)

Inline `Many`-used functions whose body size ≤ threshold. Never inline
`Recursive`. Runs exactly once. May increase code size.

Inlining requires alpha-renaming of callee's locals to avoid capture.
Append fresh VarIds to the arena.

### Phase C: Shrink Again

Same as Phase A. Cleans up after inlining. Usage recomputed between
phases (inlining changes use counts).

### Sandwich

```
usage = compute_usage(program)
shrink(program, usage)          // fixpoint
inline_speculative(program, usage, threshold)
usage = compute_usage(program)  // recompute
shrink(program, usage)          // fixpoint
```

[impl-ref: Normalizer.normalize — core/optimizer/Normalizer.scala;
Appel & Jim "Shrinking Reductions"; SML.NET shrinking lambda calculus]

---

## Handler Discharge vs Value Staging (separability)

Handler discharge is independent of value staging. When BTA/evaluator
encounters a handler where the handler is CT (all captures Known, clause
bodies CT-eligible), the handler construct is erased in the residual
regardless of whether the body's values are CT or RT.

The handler's control flow — dispatch, continuation management, state
threading — is resolved at CT. RT values pass through inlined clause
bodies as residual expressions.

The evaluator classifies two things independently for each handler:
1. Is the handler dischargeable? (All captures Known? Clause bodies
   expressible without RT-only effects?)
2. What is the stage of the overall handled expression? (Depends on
   body's value stages after handler discharge.)

---

## Three-Tier Persistability

Values crossing stage boundaries are classified by type:

- **Trivial**: Int, Bool, Char, Float, String, small enums. Inline
  literals. Cheap to duplicate.
- **Serializable**: Structs/arrays/ADTs of persistable fields. Embeddable
  via constant table for large values. Duplication has cost.
- **Non-persistable**: Closures capturing RT values, file handles, OS
  resources, effect capabilities, raw pointers. Cannot cross.

The evaluator uses this for cross-stage persistence checks. The
residualizer uses tier distinction for embedding strategy.

---

## CT-Only Functions

Functions using CT-only effects (`ComptimeReadFiles`), taking `TypeInfo`
arguments, or explicitly annotated CT-only cannot fall back to RT. If
the evaluator determines a CT-only function is called with Stuck
arguments, this is a hard error, not a staging suggestion.

---

## Type-Level CT vs Value-Level CT

Type-level CT computation (struct-from-fields, trait derivation, TypeInfo
manipulation) happens during type checking, before monomorphization.
The Evaluate+Classify pass deals only with value-level CT computation.
No interaction between these. Strict phase separation.

---

## Incremental Staging

ComptimeReadFiles records normalized path + BLAKE3 content hash.
Staging snapshot persisted as `.ctdeps.tsv` + `.ctquery.tsv`.

Invalidation check before Pass 1:
- File dependency content hash changed → re-run
- Target spec changed → re-run
- Evaluator policy changed → re-run
- Program fingerprint changed → re-run

If nothing changed, reuse previous StagingTables.

Staging snapshot IDs use span+kind+structural expression fingerprint
(index-free), reducing false churn under expr-index renumbering.

---

## Calling Convention Classification (Handler Lowering Input)

After Residualize+Specialize, remaining effect operations are classified
into three calling conventions (determined per-clause during
Evaluate+Classify's handler analysis):

- **Pure**: operation was fully discharged → no IR node emitted.
- **Direct**: handler clause is tail-resumptive → function call, no
  continuation capture.
- **Control**: handler clause is non-tail-resumptive → CPS transform,
  continuation reified as closure.

These three are distinct IR node types after linearize (handler lowering).

---

## Diagnostics

After Pass 1, automatically report:
- Expression count evaluated at CT vs total
- Handlers fully discharged / partially / kept
- Constant table size after Pass 2

With `--staging-report`:
- Top RT root causes with taint counts
- Suggestions for CT improvement

Error messages include provenance chains with source spans.
CT-only function called with RT args is a hard error with explanation.
@comptime block referencing RT variable is an error with type + reason.

---

## Data Flow Summary

```
Core IR (post-mono) + SemanticTables
        │
        ▼
Evaluate+Classify ──→ StagingTables
        │                  (outcomes, handler_analysis,
        │                   branch_decisions, usage, file_deps)
        ▼
Residualize+Specialize ──→ ResidualTables
        │                      (function_effect_summary, constant_table)
        ▼
Normalize (shrink-inline-shrink)
        │
        ▼
Linearize (handler lowering, Core → Linear IR)
```

---

## V1 Plan Notes

- Keep each pass independently testable and end-to-end runnable
  (`--emit-c --run-c`).
- Prefer conservative semantics first; optimize after invariants explicit.
- Resume single-shot check is path-sensitive (branch-exclusive single
  resumes accepted; same-path double resumes diagnosed).
- Tail-resumption checking is memoized and treats stmt cycles as
  non-tail conservatively.
- Boundary diagnostics for non-persistable crossings anchor to runtime
  boundary stmt spans and include stmt ids.
- Reachable pruning + FuncId remapping landed; tests assert cross-table
  id integrity.
- Handler specialization scope remains bounded in v1 (wrapper/control-light
  pushdown only).
- Residualizer gates CT embedding on outcome (`Known`) + persistability,
  preventing runtime-forced/boundary-rejected embeddings.
- C emitter pools repeated runtime scalar literals as `static const`.
- Scalar/ctor pooling includes finite Float literals with deterministic
  bit-pattern keys.
- Non-persistable boundary diagnostics report exact boundary roles
  (`call-arg#0`, `return-value`) in addition to statement ids/spans.
- Boundary-use indexing is stage-context-aware: expression uses inside
  `@comptime` blocks are not treated as CT→RT boundaries unless the
  value actually flows to a runtime context.
- Constructor-literal constant pooling follows documented policy:
  pool when structurally repeated or when serializable and large,
  keep small single-use literals inline.
- Residualizer recomputes `FuncId → EffectRow` from rewritten residual
  IR (fixpoint over call edges + handler subtraction).
- Staging snapshot IDs use span+kind+structural expression fingerprint.
- ComptimeReadFiles invalidation persisted in `.ctdeps.tsv` sidecar.
- CT propagation supports persistent query-cache sidecar (`.ctquery.tsv`)
  keyed by `{CtCacheKey + normalized file deps + program fingerprint}`.
- ComptimeReadFiles dependency content hashes use BLAKE3 digests.
- CT fold coverage includes float `+ - * /`, comparison, equality, and
  non-numeric equality (Bool/Char/String/Unit). Float `%` is not folded:
  `%` is Int-only and `cv_mod` traps on floats.
- Host-float folds are finite-only (NaN/Inf left unresolved).
- Integer Div/Mod overflow edges decline to fold and residualize onto the
  runtime trap; `MIN % -1` folds to `0`, matching `cv_mod`.
```