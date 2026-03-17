## Per-pass specialized IR

General approach: separate passes with IR, each pass only having access to what it needs.
Pipeline: parsing -> HIR -> CoreHIR -> TypedHIR -> Core (fw) -> ... -> Linear, etc.
Only for passes that actually need it. Otherwise reuse IR struct from a previous pass.

## Implementation constraints (v0/v1)

Authoritative policy is in `docs/v1/Decisions.md`.

- Keep style conservative and explicit by default.
- Use advanced syntax patterns only when they materially improve safety/correctness
  or remove repeated bug-prone code.
- Enforce phase integrity:
  - phase-specific ID newtypes at transformation boundaries
  - analyses inseparable from the IR they were computed against
  - pass artifacts exposing phase-typed accessors only

## Modules

Basically rust's system. File = module, `mod`/`pub`/`use` are name resolution, crates are compilation units. For v0/v1 we don't care, just single file compilation.

## Core IR: Expr/Stmt split

The Core IR separates pure expressions from effectful statements as two distinct enums.
This is a structural invariant — purity is enforced by which enum a node lives in, not by
checking effect tables at every pass.

```
enum ExprKind {        // Always pure. Safe to reorder, duplicate, eliminate.
    Var(VarId),
    Literal(Literal),
    Unary { op, expr },
    Binary { op, lhs, rhs },
    PureCall { callee, args },        // only functions with empty effect row
    MakeStruct { ty, fields },        // struct constructor
    MakeEnum { ty, variant, fields }, // enum variant constructor
    Error(ErrorNode),
}

enum StmtKind {        // May have effects. Must be sequenced.
    Return(ExprId),
    Let { binding, value: ExprId, next: StmtId },    // bind pure expr, continue
    Val { binding, value: StmtId, next: StmtId },    // bind effectful computation, continue
    Call { result, callee, args, effects, next },     // effectful function call
    If { cond: ExprId, then_branch, else_branch },    // branch on pure condition
    Match { scrutinee: ExprId, arms, default },       // match on pure scrutinee
    Perform { result, effect, operation, args, next },// effect operation
    Resume { result, resume, arg, next },             // resume continuation
    Handle { handler, body, next },                   // install handler
    Stage { stage, body, next },                      // @comptime / @runtime block
    Hole { ty },                                      // typed hole
    Error(ErrorNode),
}
```

Consequences:
- Every pass operating on Expr gets the purity guarantee for free — no effect lookup needed.
- Evaluate+Classify: Expr with Known inputs → always CT-eligible (no effect check).
- Normalizer: freely CSE/reorder/eliminate Expr nodes.
- Dead code: `Let(id, expr, body)` where `id` unused → drop binding (always safe).
- Handler lowering: only Stmt nodes can contain effect operations.

No Box/Unbox. Closures are first-class values with ARC. A closure capturing mutable state
holds ARC'd references. No ceremony to put closures in data structures.

Capture tracking happens through the effect/capture system, not through box/unbox syntax.
When a closure is created, what it captures is tracked for staging (a closure capturing RT
values is RT-tainted) and for closure conversion (capture list).

[impl-ref: Effekt Expr/Stmt/Block split → Type.scala ValueType/BlockType, Tree.scala Expr/Block/Stmt enums]

## Linear IR: Post-handler-lowering representation

The Linear IR is produced by the linearize pass. It has three distinct call node types
(PureCall, DirectCall, ControlCall) and no Handle/Resume/Perform nodes for handled effects.
Unhandled Perform nodes remain for effects that survived residualization and specialization.

The Linear IR has its own ID types (LinearExprId, LinearStmtId, LinearFuncId) which are
distinct from Core IR IDs. Dense remapping occurs during linearization: only reachable
functions get LinearFuncIds.

## CFG IR: ownership and control-flow boundary

The `cfg_lower` pass consumes Linear IR after handler lowering and builds a separate,
block-form `CfgProgram`. CFG expressions, instructions, blocks, values, and functions
have phase-specific IDs. Block parameters model values returned by `Val`, calls,
effect operations, match arms, and cleanup paths without treating sequential subgraphs
as parallel tree children.

Linear IR ends at this boundary. Ownership planning, verification, and C emission all
consume CFG IR directly. The backend never reconstructs structured Linear control flow:
it emits labels, explicit edges, and parallel block-argument copies from the CFG.

CFG liveness drives a sink-oriented ARC transform. Calls, constructor fields, block
arguments, and returns consume owned references. A last use moves its reference; a
non-last consume retains a copy. Destructuring a dead constructor uses a moved-field
projection and clears the parent slot before releasing the parent, equivalent to Nim's
`=sink`/`wasMoved` lowering.

The current language cannot construct heap cycles: constructors are immutable and can
only point to values that existed before their allocation. ARC is therefore complete,
not a cycle-leaking approximation. A real ORC mode belongs with the first feature that
can create cyclic heap graphs (for example mutable reference fields or heap closures);
dormant Core-level candidate analysis and no-op ORC hooks are deliberately excluded.

## Fused pass architecture

### Evaluate+Classify (replaces separate CT propagation + BTA)

Single-pass tree-walking evaluator that simultaneously computes CT values
and classifies stages. Produces `Outcome` (Known/Stuck) per expression.
Eliminates the intermediate CtPropagationTables → BtaTables handoff and
one full IR traversal.

Rationale: BTA was a read-only consumer of CT cache. The evaluator already
computes everything BTA needs. Making classification inline during
evaluation removes the redundant walk.

The evaluator produces a unified `StagingTables` struct containing
outcomes, handler analysis, branch decisions, usage, and file deps.

### Residualize+Specialize (replaces separate residualize + handler specialize)

Single pass that embeds constants, erases discharged handlers, and
performs handler specialization during the residualization walk. When
encountering a non-discharged handler wrapping a specializable call
pattern, creates the specialized copy immediately.

Rationale: handler specialization needs the same information the
residualizer computes (which handlers are discharged, which functions
are called under which handlers). Fusing eliminates one IR traversal
and the intermediate IR between the two passes.

### Normalizer: shrink-inline-shrink sandwich

Shrinking reductions (always profitable, always terminating) separated
from speculative inlining (may grow code). Shrink runs to fixpoint,
then one round of usage-gated inlining, then shrink again.

Runs after Residualize+Specialize on Core IR.

[impl-ref for normalizer rules: Normalizer.normalize, normalizeVal, active,
shouldInline — core/optimizer/Normalizer.scala; Appel & Jim "Shrinking lambda"
(1997); SML.NET shrinking reductions]

### MLIR patterns: adopted and rejected

Adopted:
- IrNode trait for generic traversal across tree-shaped Core and Linear IRs
- Phase-typed side tables carrying only what downstream passes need

Rejected:
- Full dialect/operation system (the Core, Linear, and compact block CFG forms are sufficient)
- Progressive lowering within a single IR (handler lowering is a
  clean Core → Linear transition)

## IrNode trait for generic traversal

Both Core and Linear IRs implement a shared trait for tree walking:

```
trait IrStmtNode {
    type StmtId: Copy + Eq + Hash;
    type ExprId: Copy + Eq + Hash;
    fn child_stmts(&self) -> SmallVec<[Self::StmtId; 4]>;
    fn child_exprs(&self) -> SmallVec<[Self::ExprId; 4]>;
}

trait IrProgram {
    type StmtId: Copy + Eq + Hash;
    type ExprId: Copy + Eq + Hash;
    type StmtNode: IrStmtNode<StmtId = Self::StmtId, ExprId = Self::ExprId>;
    fn stmt(&self, id: Self::StmtId) -> Option<&Self::StmtNode>;
}
```

Generic analyses (reachability, stmt_mentions_var, fold_stmts) are written
once against IrProgram. Core's `StmtNode` and Linear's `LinearStmtNode`
already have `child_stmts()` / `child_exprs()` methods with the right shape.

## Phase struct policy

Each phase struct carries:
- Its own output tables
- SemanticTables (needed through C emit for type info and effect properties)
- Only other tables that downstream passes actually read

Tables not read after a phase boundary are dropped:
- MonomorphizationSummary: consumed by Evaluate+Classify, dropped after Staged
- StagingTables: consumed by Residualize+Specialize, dropped after Residualized

Phase chain:
```
Parsed → CoreBuilt → Typed → Monomorphized → Staged → Residualized → Linearized
```

Each transition consumes the previous phase struct. No borrowing across boundaries.

## Removing effect annotations from IR after comptime passes

Effect annotations in functions become a burden after monomorphization and staging,
so we remove them after Residualize+Specialize and keep only effect summaries.

### Where effect-annotated arrows are useful

- `type+effect check`: arrows like `A -> <E> B` are most meaningful here.
- `Evaluate+Classify`: need to know "what effects could happen here?"

### Where effect-annotated arrows become a burden

- `linearize` (handler lowering)
- `closure convert`
- `ANF/CFG/SSA`
- `effect-qualified opts`
- `ARC insertion`
- `C`

Solution: Effect summaries as metadata per function / region.
Each function symbol has `effects = {LocalState, Alloc}` stored in a side table.

## Usage analysis

Track how each binding is used across the program. Four categories:

```
enum Usage { Never, Once, Many, Recursive }
```

With arithmetic:
- `Once + Once = Many`
- `Never + x = x`
- `Recursive + anything = Recursive`
- `x * Many = Many` (when inlining into a multiply-used context)
- For branches: `max(Once, Once) = Once` (only one branch executes)
- For sequential: `Once + Once = Many` (both execute)

Used for:
- Dead code elimination: `Never` → eliminate
- Inlining decisions: `Once` → always inline; `Many` with small body → inline
- Recursion detection: `Recursive` → don't inline (would diverge), needs fuel for CT
- Usage update after inlining: when inlining a `Many`-use block, multiply usage of its free variables
- Normalizer: shrinking reductions gated on usage, speculative inline gated on usage + size

Entry point: walk from program entry points, track each variable reference. For recursive
functions, detect cycles via in-progress set.

[impl-ref: Reachable.apply, Usage enum — Reachable.scala]

## Free variable tracking

Compositional, lazy tracking of what each expression/block depends on. Two purposes:
1. Evaluate+Classify needs "what does this depend on?" to determine staging
2. Closure conversion needs capture lists

```
struct FreeVars {
    values: Map<VarId, (Type, Stage)>,
    blocks: Map<VarId, (BlockType, Captures, Stage)>,
}
```

Composition rules:
- FreeVars of `(a + b)` = join(FreeVars(a), FreeVars(b))
- FreeVars of `let x = e in body` = FreeVars(e) ∪ (FreeVars(body) \ {x})
- FreeVars of `BlockLit(params, body)` = FreeVars(body) \ params

Results are cached per node (lazy val). Used by tail resumption detection, normalizer
inlining decisions, contify analysis, and staging.

[impl-ref: Free ADT (Join, Without, Value, Block) — Type.scala; Variables.free — cps/Tree.scala]

## Side tables with typed keys

All per-node metadata lives in typed side tables, not on IR nodes.

```
struct DenseMap<K, V> {
    data: Vec<Option<V>>,
}
```

Identity-based keys for tree nodes (two nodes with same structure are distinct).
Typed key newtypes prevent mixups across phases.

Rollback mechanism: used by the type checker for speculative unification
(try a type assignment, undo if it fails). NOT used by staging analysis —
staging is sequential (Evaluate+Classify → Residualize+Specialize) with no
speculation.

[impl-ref: Annotation system — context/Annotations.scala; backupUnification/restoreUnification — typer/Unification.scala]

## Capture constraint propagation

Capture/effect variables form a constraint graph. Each variable is a node with:
- Lower bound: what we know is definitely in the set
- Upper bound: what's allowed in the set
- Connections to other nodes via subset relationships

When you learn a new lower bound, it propagates forward. New upper bounds propagate backward.
Contradictions detected immediately.

Example: handler installs capability for effect E. Body uses IO. Constraint graph propagates
{IO} through the body's capture variable. If handler doesn't handle IO, upper bound violated → error.

Used by: type+effect checker to infer effect rows precisely. Precise effect rows → better
staging decisions (evaluator knows exactly what effects a computation has).

[impl-ref: Constraints class, propagateLower/propagateUpper — typer/Constraints.scala]

## Concrete effects assertion

Before Evaluate+Classify runs, assert that all effect rows in the input IR have zero
unresolved type variables. If any effect mentions an unsolved unification variable, that's
a compiler bug.

This prevents the evaluator from making wrong staging decisions on incomplete information.
After monomorphization, everything should be concrete.

[impl-ref: assertConcreteEffect, ConcreteEffects — typer/ConcreteEffects.scala]

## Fresh capabilities per handler scope

Each `with handler` creates a fresh capability ID. Effect operations resolve to the nearest
capability in the lexical scope chain. Two handlers for the same effect type get different
capabilities — they cannot be confused.

Implementation: capability scope is a stack. Each entry has handler ID, effect label,
capability ID, and per-clause discharge analysis (from Evaluate+Classify). Lookup walks
the stack top-to-bottom.

This prevents the "wrong handler catches the effect" bug described in Caveats.md.

[impl-ref: CapabilityScope (BindAll, BindSome, GlobalCapabilityScope) — typer/CapabilityScope.scala]

## CT evaluator design

The CT evaluator is a tree-walking interpreter over Core IR, integrated into the
Evaluate+Classify pass. It handles algebraic effects internally via a prompt stack.
Single-shot resumption. Target-aware arithmetic.

State machine pattern (no host-language stack overflow on deep evaluation):

```
enum EvalState {
    Done(Outcome),
    Step(StmtId, Env, Stack, Heap),
    FuelExhausted(StmtId, u64),
    SizeExhausted(StmtId, u64),
}
```

Each `step()` takes a `Step` and returns the next state. Fuel counter decremented per step.
Size tracked on data allocation.

When the evaluator encounters a stuck point (RT variable, non-CT-eligible effect), instead
of aborting, it produces `Outcome::Stuck(reason)` and continues classifying the surrounding
context. This is the key difference from a pure interpreter: it's a partial evaluator that
gracefully handles unknown inputs.

Handler support: Reset pushes a new prompt on the stack. Shift captures frames up to the
prompt. Resume reinstates captured frames. This lets the evaluator run effectful CT code
with CT handlers.

Mutable state: `Var` allocates a frame on the stack with an address. `Get` looks up the
address in the stack. `Put` updates it. Stack-based, not host-mutation-based.

Function-level memoization: `(FuncId, Vec<Value>) → Outcome` cache with in-progress set
for cycle detection. Extensible to full query architecture in v2.

[impl-ref: Interpreter.step, State enum, eval(Block), eval(Expr) — core/vm/VM.scala]

## Instrumentation for CT diagnostics

Lightweight observer on the CT evaluator. Tracks fuel, size, file dependencies
without modifying interpreter logic.

```
trait CTInstrumentation {
    fn step(&mut self, fuel: &mut u64) -> bool;
    fn allocate(&mut self, size: u64, budget: &mut u64) -> bool;
    fn file_read(&mut self, path: &Path, hash: &Hash);
    fn effect_performed(&mut self, effect: EffectLabelId);
}
```

[impl-ref: Instrumentation trait, Counting class — core/vm/Instrumentation.scala]

## Declaration context

Centralized lookup tables for type/effect/constructor declarations. Built once, passed
as context to type checker, evaluator, residualizer, handler lowering.

```
struct DeclarationContext {
    datas: Map<Id, DataDecl>,
    interfaces: Map<Id, InterfaceDecl>,
    constructors: Map<Id, ConstructorRef>,
    fields: Map<Id, FieldRef>,
    properties: Map<Id, PropertyRef>,
    effect_labels: Map<EffectLabel, EffectProperties>,
    extern_defs: Map<Id, ExternDef>,
}
```

Lazy maps keyed by ID. `find*` returns Option, `get*` panics with context.

[impl-ref: DeclarationContext class — core/DeclarationContext.scala]

## Built-in effects (suggested minimal set)

**Core behavior**
- `Pure` (no effects)
- `Diverge` (may not terminate) — matters for CT fuel and some opts
- `Alloc` (heap alloc) — CT-ok if evaluator supports it
- `LocalState[T]` — optimizable mutable state
- `SharedState[T]` (or `Atomic[T]`) — volatile/opaque for optimization
- `IO` — runtime-only
- `FFI` — runtime-only, opaque

**Optional**
- `System` (clock, randomness, env vars) — runtime-only unless explicitly "frozen"
- `Concurrency` — runtime-only

**Compile-time-only**
- `ComptimeReadFiles` / `BuildFS` — allowed only in comptime mode

## What we're tracking during compilation

### Primary facts

1. **Type** of each expression/value
2. **Effect row** of each expression (typed IR only; later becomes metadata/tags)
3. **Outcome** (Known(Value) or Stuck(Reason)) — replaces separate Stage + CT cache
4. **Value persistence classification** (can this value cross CT boundary?)
5. **Effect properties** per effect label (Local/Shared, Volatile, Discardable, etc.)
6. **Usage** per binding (Never/Once/Many/Recursive)
7. **Free variables** per node (compositional, lazy)
8. After lowering: instruction effect classes

Everything else derived from these.

### Where each piece lives per pass

### 0) Parse / Desugar

**IR:** AST/HIR with spans, names
**Side tables:** interned strings, scopes/symbol IDs
**No types/effects/stage yet**

### 1) Type + effect check (TypedHIR / Core)

**IR:** Core with Expr/Stmt split. Types in side tables.
**Side tables:**
- `type_of_expr: ExprId -> TypeId`
- `effects_of_expr: ExprId -> SortedEffectRow`
- `persistability_of_type: TypeId -> Persistability`
- `effect_label_props: EffectLabelId -> EffectProperties`
- `capture_constraints: CaptureNodeId -> CaptureNodeData` (constraint graph)

**Assertion:** all effect rows concrete after this pass (no unresolved unification vars).

### 2) Monomorphize (types + effects)

**IR:** generates specialized functions/rows
**Side tables:**
- `func_effect_summary: FuncId -> SortedEffectRow`

This pass runs before any comptime pass so the evaluator only sees concrete types
and concrete effect rows.

### 3) Evaluate+Classify (fused CT propagation + BTA)

**Consumes:** type_of, effects_of, effect_label_props, persistability_of
**Produces (StagingTables):**
- `outcomes: DenseMap<ExprId, Outcome>` (Known values and Stuck reasons)
- `var_outcomes: DenseMap<VarId, Outcome>` (per-variable staging)
- `handler_analysis: DenseMap<HandlerId, HandlerAnalysis>` (per-clause discharge)
- `branch_decisions: DenseMap<ExprId, BranchDecision>` (live branch for CT conditions)
- `usage: DenseMap<VarId, Usage>` (from reachability analysis)
- `file_deps: Vec<CtFileDep>` (build dependencies)
- `cache_key: CtCacheKey` (target spec, evaluator policy, compiler version)
- `eval_stats: CtEvalStats` (fuel/size/fold counters)

Single-pass tree-walking evaluation + classification. No separate BTA pass.
No rollback. Deterministic given inputs.

### 4) Residualize+Specialize (fused residualize + handler specialize)

**Consumes:** outcomes, handler_analysis, branch_decisions, usage
**Produces:**
- residual Core IR with CT parts replaced by constants
- `function_effect_summary: HashMap<FuncId, SortedEffectRow>` (recomputed for residual)
- `constant_table: ConstantTable` (deduplicated constant pool)

Constant embedding policy:
- Trivial values inline directly.
- Serializable values use a deduplicated const pool with per-entry and per-unit caps.
- Non-persistable values fail boundary checks (never embedded).

Handler specialization integrated: specialized function copies created during
residualization walk for non-discharged handlers wrapping specializable patterns.

### 5) Normalize (shrink-inline-shrink)

**Consumes:** residual Core IR, usage (recomputed)
**Produces:** cleaned-up Core IR

Shrinking reductions to fixpoint, one round of speculative inlining, shrink again.

### 6) Linearize (handler lowering) — big boundary

**Goal:** erase high-level effects into explicit runtime artifacts.
**IR changes:**
- Core IR → Linear IR (different ID types, dense remapping)
- remove Handle/Perform/Resume nodes for handled effects
- introduce PureCall/DirectCall/ControlCall as distinct node types
**Side tables:**
- keep `func_effect_summary` (for later opt gating)

### 7) CFG lowering

**Consumes:** Linear IR
**Produces:** block-form CFG with SSA-like values and explicit continuations

`Val`, call, `Perform`, match-arm, handler-exit, and stage-exit sequencing is represented
by block parameters and edges. Backward liveness and last-use analysis runs on this graph.

### 8) Effect-qualified opts

**Consumes:** `inst_effect_class` + `func_effect_summary`
Gates CSE/LICM/DSE etc.

### 9) CFG ARC insertion and verification

**Consumes:** CFG liveness, semantic ownership classes
**Produces:** ARC operations on block entries, instructions, and terminators; match
projection modes (`Borrow`, `Copy`, `Move`)

Raw mode materializes retain/release ownership transfers. Optimized mode folds a
last-use pair into a sink move and uses moved-field projection when a match consumes its
parent. The verifier rejects invalid ARC value references, duplicate releases at one
site, and moves from live parents.

### 10) Direct CFG C emission

The emitter consumes the annotated CFG. It emits explicit labels and gotos, preserves
parallel edge-copy semantics with temporaries, and passes lexical handler capabilities
to scoped effects. No legacy Linear emitter or Linear ARC annotations remain.

## Normalizer reduction rules

After residualization, apply these reduction rules to clean up the IR.
Split into shrinking (always safe) and speculative (may grow code).

### Shrinking reductions (Phase A: to fixpoint)

**Dead binding elimination:** `Let(id, expr, body)` / `Val(id, stmt, body)` where
`usage[id] == Never` → body only. Always safe because binding is pure (Let) or
the effectful computation's result is unused.

**Val-return commutation:** `val x = return e; body` → `let x = e; body`.
Downgrades effectful binding to pure binding.

**Val-val flattening:**
```
val x = { val y = s1; s2 }; s3
→ val y = s1; val x = s2; s3
```
Requires alpha-renaming if y shadows something in s3.

**Constant branch elimination:** `if (Literal(true)) thn else els → thn`

**Match reduction:** `match MakeEnum(tag, args) { case tag(params) => body }` →
`let param0 = arg0; ... let paramN = argN; body`

**Beta-reduce Once-used:** When a function is used exactly once, inline its body
with arguments substituted. Always decreases total program size.

### Speculative inlining (Phase B: once, gated)

**Inline Many-used small functions:** Body size ≤ threshold → inline at all call
sites. Never inline Recursive. After inlining, the original function may become
Never-used and be eliminated by subsequent shrinking.

### Val commutation for join points

```
val x = if (cond) thn else els; body
→ def k(x) = body; if (cond) { val x1 = thn; k(x1) } else { val x2 = els; k(x2) }
```

This introduces join points at branch/match boundaries. Applied during shrinking
when the val-if pattern is detected.

[impl-ref: Normalizer.normalize, normalizeVal, active, shouldInline — core/optimizer/Normalizer.scala]

## Static argument transformation → handler specialization

When a recursive function passes some arguments unchanged through every recursive call,
split into wrapper (takes all args) and worker (takes only changing args, closes over static ones).

The handler capability becomes part of the worker's closure. All handler dispatches inside
become direct calls to known operations.

Analysis: collect all recursive functions and the arguments at their call sites. For each
argument position, check if it's always passed unchanged (same variable). If so, mark it static.

Now integrated into the Residualize+Specialize pass: when handler specialization fires,
static argument transformation is applied to the specialized copy.

[impl-ref: StaticArguments.transform, IsStatic, wrapDefinition — core/optimizer/StaticArguments.scala;
RecursiveFunction, Recursive.process — core/Recursive.scala]

## Tail resumption detection

A handler clause is tail-resumptive if the continuation `k` appears only in tail position
as `resume(k, value)` — never captured, never passed to other functions, never used under
Reset/Region.

Syntactic check: walk the clause body. At each node, check if `k` appears free in non-tail
positions. If `k` only appears as `Resume(k, body)` at the end, the clause is tail-resumptive.

When true, `resume(k, body)` is replaced with just `body` — the shift/reset overhead disappears.

Important: mutable variable definitions are NOT considered tail-resumptive even when they
look like it. Statement cycles are conservatively treated as non-tail.

Performed during Evaluate+Classify (for clause discharge classification) and verified during
linearize (for calling convention assignment).

[impl-ref: tailResumptive, removeTailResumption — core/optimizer/RemoveTailResumptions.scala]

## Calling convention classification (per-clause)

After tail resumption detection:
- `tailResumptive(k, clauseBody) == true` → **DirectCall** (function call, no continuation capture)
- otherwise → **ControlCall** (CPS transform, continuation reified as closure)
- handler fully discharged at CT → **PureCall** (eliminated, no IR node)

These three are distinct IR node types after linearize (handler lowering).

Selective CPS: only ControlCall clauses trigger CPS transformation. DirectCall clauses
compile as regular function calls with evidence/capability argument.

[impl-ref: Continuation.Static vs Continuation.Dynamic — cps/Transformer.scala;
CallingConvention enum — core/Transformer.scala]

## Contify analysis (join point recovery)

After handler lowering, some functions always return to the same place. These can become
local continuations (join points) — calls become jumps, no closure allocation needed.

Analysis: for each function `f`, collect the set of continuations it returns to across all
call sites. If the set has exactly one element `k`, and `k` is in scope at `f`'s definition,
and `f` is not recursive, then `f` can become a join point.

Scoping check: `k` must be free in the rest of the program after `f`'s definition.

[impl-ref: Contify.returnsTo, Contify.contify — cps/Contify.scala]

## Scope escape check → persistability

When a value's type mentions a capture variable or type variable that's only available
inside a certain scope, and the value crosses that scope boundary, that's an error.

Same algorithm used for:
1. Handler return types: result must not mention handler's capabilities
2. Region return types: result must not mention region's capture
3. Cross-stage persistence: CT value crossing to RT must not mention CT-only resources

Implementation: compute free type/capture variables of the result type. Check if any are
not in the current scope. If so, error with explanation of what escaped.

[impl-ref: Wellformedness.wellformed, freeCapture, freeTypes — typer/Wellformedness.scala]

## Structured error context for diagnostics

Errors carry a chain of "why was this check happening" context frames. Each type comparison,
effect check, or staging decision pushes a frame. On error, the chain produces messages.

For staging provenance specifically, each Stuck outcome carries a single Reason enum.
Full chains are rendered on demand by walking DependsOnVar links through the outcomes table.

[impl-ref: ErrorContext sealed trait hierarchy — typer/ErrorContext.scala]

## Pattern matching compilation

Uses the Jules Jacobs algorithm. Clauses as conjunction of conditions (patterns, predicates,
bindings). Branching heuristic: split on the scrutinee mentioned by the most clauses.

Each clause body is wrapped in a join point function. The compiled match calls into it.
This enables sharing when multiple paths lead to the same clause.

For staging: if the scrutinee is Known, the match compiler can select the matching arm
at compile time (dead-arm elimination within Evaluate+Classify).

[impl-ref: PatternMatchingCompiler.compile, Clause, Condition, Pattern — core/PatternMatchingCompiler.scala]

## Rewrite/query pass infrastructure

Passes built on partial-function-based rewrite pattern:

```
class Rewrite {
    fn stmt: PartialFunction<Stmt, Stmt> = empty
    fn rewrite(s: Stmt) -> Stmt = rewriteStructurally(s, stmt)
}
```

Override only the cases you care about; everything else is structural recursion.
Context-carrying variant for passes needing environment info.
Analysis passes use Query trait: accumulate results instead of transforming.

For v1, the IrNode trait provides the structural recursion base for both Core and
Linear IRs. Generic walkers use IrProgram to traverse either IR.

[impl-ref: Tree.Rewrite, Tree.TrampolinedRewrite, Tree.RewriteWithContext, Tree.Query — core/Tree.scala]
```
