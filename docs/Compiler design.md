## Per-pass specialized IR

General approach: separate passes with IR, each pass only having access to what it needs.
Pipeline: parsing -> HIR -> CoreHIR -> TypedHIR -> Core (fw) -> ... -> ANF, etc.
Only for passes that actually need it. Otherwise reuse IR struct from a previous pass.

## Implementation constraints (v0/v1)

Authoritative policy is in `docs/Decisions.md`.

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

enum Expr { // Always pure. Safe to reorder, duplicate, eliminate.
Var(id, type)
Literal(value, type)
PureApp(func, targs, vargs) // only functions with empty effect row
Make(datatype, tag, targs, vargs) // constructor application
}

enum Stmt { // May have effects. Must be sequenced.
Return(Expr)
Let(id, Expr, Stmt) // bind pure expr, continue
Val(id, Stmt, Stmt) // bind effectful computation, continue
App(callee, targs, vargs, bargs) // effectful function call
ImpureApp(id, callee, targs, vargs, bargs, Stmt) // FFI/extern call, binds result
Invoke(callee, method, targs, vargs, bargs) // dynamic dispatch
If(Expr, Stmt, Stmt) // branch on pure condition
Match(Expr, clauses, default) // match on pure scrutinee
Def(id, Block, Stmt) // local block definition
Reset(BlockLit) // install prompt/handler delimiter
Shift(prompt, k_param, Stmt) // capture continuation
Resume(k, Stmt) // resume continuation
Region(BlockLit) // region-based allocation scope
Alloc(id, Expr, region, Stmt) // allocate in region
Var(id, Expr, capture, Stmt) // local mutable variable
Get(id, type, ref, Stmt) // read mutable variable
Put(ref, Expr, Stmt) // write mutable variable
Hole(type, span) // typed hole
}

enum Block { // Callable values (first-class, no box/unbox needed)
BlockVar(id, type, captures)
BlockLit(tparams, cparams, vparams, bparams, body: Stmt)
}

text


Consequences:
- Every pass operating on Expr gets the purity guarantee for free — no effect lookup needed.
- BTA: Expr with CT inputs → always CT-eligible (no effect check).
- Normalizer: freely CSE/reorder/eliminate Expr nodes.
- Dead code: `Let(id, expr, body)` where `id` unused → drop binding (always safe).
- ANF conversion: already mostly done — Expr = operands, Stmt = instructions.
- Handler lowering: only Stmt nodes can contain effect operations.

No Box/Unbox. Closures are first-class values with ARC. A closure capturing mutable state
holds ARC'd references. No ceremony to put closures in data structures.

Capture tracking happens through the effect/capture system, not through box/unbox syntax.
When a closure is created, what it captures is tracked for staging (a closure capturing RT
values is RT-tainted) and for closure conversion (capture list).

[impl-ref: Effekt Expr/Stmt/Block split → Type.scala ValueType/BlockType, Tree.scala Expr/Block/Stmt enums]


## Removing effect annotations from IR after BTA/comptime passes

Effect annotations in functions become a burden after monomorphization, CT propagation,
and BTA/provenance, so we remove them after BTA/comptime passes and keep only effect summaries.

### Where effect-annotated arrows are useful

- `type+effect check`: arrows like `A -> <E> B` are most meaningful here.
- `BTA/provenance` + `comptime eval/residualize`: need to know "what effects could happen here?"

### Where effect-annotated arrows become a burden

- `lower handlers/effects`
- `closure convert`
- `ANF/CFG/SSA`
- `effect-qualified opts`
- `ARC insertion`
- `C`

Solution: Effect summaries as metadata per function / region.
Each function symbol has `effects = {LocalState, Alloc}` stored in a side table.


## Usage analysis

Track how each binding is used across the program. Four categories:

enum Usage { Never, Once, Many, Recursive }

text


With arithmetic:
- `Once + Once = Many`
- `Never + x = x`
- `Recursive + anything = Recursive`
- `x * Many = Many` (when inlining into a multiply-used context)
- `decrement` (when substituting a variable counted once in original)

Used for:
- Dead code elimination: `Never` → eliminate
- Inlining decisions: `Once` → always inline; `Many` with small body → inline
- Recursion detection: `Recursive` → don't inline (would diverge), needs fuel for CT
- Usage update after inlining: when inlining a `Many`-use block, multiply usage of its free variables

Entry point: walk from program entry points, track each variable reference. For recursive
functions, detect cycles via a stack of "currently being analyzed" IDs.

[impl-ref: Reachable.apply, Usage enum — Reachable.scala]


## Free variable tracking

Compositional, lazy tracking of what each expression/block depends on. Two purposes:
1. BTA needs "what does this depend on?" to determine staging
2. Closure conversion needs capture lists

struct FreeVars {
values: Map<VarId, (Type, Stage)>,
blocks: Map<VarId, (BlockType, Captures, Stage)>,
}

text


Composition rules:
- FreeVars of `(a + b)` = join(FreeVars(a), FreeVars(b))
- FreeVars of `let x = e in body` = FreeVars(e) ∪ (FreeVars(body) \ {x})
- FreeVars of `BlockLit(params, body)` = FreeVars(body) \ params

Results are cached per node (lazy val). Used by tail resumption detection, normalizer
inlining decisions, contify analysis, and BTA.

[impl-ref: Free ADT (Join, Without, Value, Block) — Type.scala; Variables.free — cps/Tree.scala]


## Side tables with typed keys and rollback

All per-node metadata lives in typed side tables, not on IR nodes.

struct SideTable<K, V> {
name: &'static str, // for debugging
data: HashMap<NodeId, V>,
}

text


Key design decisions:
- Identity-based keys for tree nodes (two nodes with same structure are distinct)
- Typed annotation keys prevent mixups
- Backup/restore for speculative analysis (BTA demand evaluation)

Rollback mechanism: before speculative evaluation, snapshot the side tables. If evaluation
fails (hits RT value, exceeds fuel), restore the snapshot. With arena allocation, snapshot
is just saving the arena's `used` pointer.

Rollback mechanism: used by the type checker for speculative unification 
(try a type assignment, undo if it fails). NOT used by BTA — staging 
analysis is sequential (CT propagation → BTA classification) with no 
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
staging decisions (BTA knows exactly what effects a computation has).

[impl-ref: Constraints class, propagateLower/propagateUpper — typer/Constraints.scala]


## Concrete effects assertion

Before BTA runs, assert that all effect rows in the input IR have zero unresolved type
variables. If any effect mentions an unsolved unification variable, that's a compiler bug.

This prevents the BTA from making wrong staging decisions on incomplete information.
After monomorphization, everything should be concrete.

[impl-ref: assertConcreteEffect, ConcreteEffects — typer/ConcreteEffects.scala]


## Fresh capabilities per handler scope

Each `with handler` creates a fresh capability ID. Effect operations resolve to the nearest
capability in the lexical scope chain. Two handlers for the same effect type get different
capabilities — they cannot be confused.

Implementation: capability scope is a stack. Each entry has handler ID, effect label,
capability ID. Lookup walks the stack top-to-bottom.

typedef struct {
uint32_t effect_label;
uint32_t capability_id; // fresh per handler installation
uint32_t handler_id;
} CapabilityEntry;

CapabilityEntry cap_stack[64];
uint32_t cap_depth;

text


This prevents the "wrong handler catches the effect" bug described in Caveats.md.

[impl-ref: CapabilityScope (BindAll, BindSome, GlobalCapabilityScope) — typer/CapabilityScope.scala]


## CT evaluator design

The CT evaluator is a tree-walking interpreter over Core IR. It handles algebraic effects
internally via a prompt stack. Single-shot resumption. Target-aware arithmetic.

State machine pattern (no host-language stack overflow on deep evaluation):

enum EvalState {
Done(Value),
Step(Stmt, Env, Stack, Heap),
FuelExhausted(Stmt, u64),
SizeExhausted(Stmt, u64),
}

text


Each `step()` takes a `Step` and returns the next state. Fuel counter decremented per step.
Size tracked on data allocation.

Handler support: Reset pushes a new prompt on the stack. Shift captures frames up to the
prompt. Resume reinstates captured frames. This lets the evaluator run effectful CT code
with CT handlers (e.g., Config effect handled by build-time config file).

Mutable state: `Var` allocates a frame on the stack with an address. `Get` looks up the
address in the stack. `Put` updates it. Stack-based, not host-mutation-based.

[impl-ref: Interpreter.step, State enum, eval(Block), eval(Expr) — core/vm/VM.scala]


## Instrumentation for CT diagnostics

Lightweight observer trait on the CT evaluator. Tracks fuel, size, file dependencies
without modifying interpreter logic.

trait CTInstrumentation {
fn step(&mut self, fuel: &mut u64) -> bool;
fn allocate(&mut self, size: u64, budget: &mut u64) -> bool;
fn file_read(&mut self, path: &Path, hash: &Hash);
fn effect_performed(&mut self, effect: EffectLabel);
}

text


[impl-ref: Instrumentation trait, Counting class — core/vm/Instrumentation.scala]


## Declaration context

Centralized lookup tables for type/effect/constructor declarations. Built once, passed
as context to type checker, BTA, residualizer, handler lowering.

struct DeclarationContext {
datas: Map<Id, DataDecl>,
interfaces: Map<Id, InterfaceDecl>,
constructors: Map<Id, ConstructorRef>,
fields: Map<Id, FieldRef>,
properties: Map<Id, PropertyRef>, // interface operation signatures
effect_labels: Map<EffectLabel, EffectProperties>,
extern_defs: Map<Id, ExternDef>,
}

text


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
3. **Stage** (Comptime vs Runtime + provenance reason)
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
- `effects_of_expr: ExprId -> EffectRowId`
- `persistable_of_type: TypeId -> Persistability`
- `effect_label_props: EffectLabel -> EffectProperties`
- `capture_constraints: CaptureNodeId -> CaptureNodeData` (constraint graph)

**Assertion:** all effect rows concrete after this pass (no unresolved unification vars).

### 2) Monomorphize (types + effects)

**IR:** generates specialized functions/rows
**Side tables:**
- `func_effect_summary: FuncId -> EffectRowId`

This pass runs before any comptime pass so CT propagation/BTA only see concrete types
and concrete effect rows.

### 3) CT Propagation

**Consumes:** type_of, effects_of, effect_label_props, persistability_of
**Produces:**
- `ct_cache: ExprId -> Value` (all successfully evaluated CT expressions)
- `branch_decisions: ExprId -> BranchDecision` (live branch for CT conditions)
- `knownness_of_expr: ExprId -> {Unknown, KnownLocal, KnownPersistable}`
- `file_deps: Vec<DepKey { normalized_path, content_hash }>` (build dependencies)
- `ct_cache_key: {target spec, evaluator policy, compiler version}`
- `ct_diagnostics: Vec<CtDiagnostic>` (fuel/size warnings)

Tree-walking evaluation of all transitively-CT expressions. Respects fuel 
and size budgets. Handles CT-allowed effects via internal handler stack. 
Iterates until no new cache entries (typically 1 pass).

### 4) BTA / Staging Classification

**Consumes:** type_of, effects_of, ct_cache, branch_decisions (all read-only)
**Produces:**
- `stage_of_expr: ExprId -> Stage { CT | RT(reason) }`
- `stage_of_var: VarId -> Stage`
- `knownness_of_expr: ExprId -> Knownness`
- `handler_discharge: HandlerId -> HandlerDischarge`
- `usage_of: VarId -> Usage` (from reachability analysis, can also run earlier)

Pure classification pass. No evaluation, no side effects, no rollback. 
Deterministic given inputs.

### 5) Residualize

**Consumes:** ct_cache, stage_of, handler_discharge, usage_of
**Produces:**
- residual IR with CT parts replaced by constants
- `func_effect_summary: FuncId -> EffectRow` (recomputed for residual)

Constant embedding policy:
- Trivial values inline directly.
- Serializable values use a deduplicated const pool with per-entry and per-unit caps.
- Non-persistable values fail boundary checks (never embedded).

### 6) Handler specialization

Between residualization and handler lowering.
Uses: usage analysis (recursive detection), free variable tracking.
Produces: specialized function copies with handlers pushed into bodies.

### 7) Lower handlers/effects — big boundary

**Goal:** erase high-level effects into explicit runtime artifacts.
**IR changes:**
- remove Handle/Perform/Resume nodes
- introduce capability/evidence structs + calls
- Three distinct node types: PureCall, DirectCall, ControlCall
**Side tables:**
- keep `func_effect_summary` (for later opt gating)

### 8) Closure convert

**Side tables:**
- capture lists from free variable tracking: `closure_captures: ClosureId -> [VarId]`
- capture-stage summary: `closure_stage: ClosureId -> Stage`

### 9) ANF / CFG / SSA

**IR:** flat instructions, blocks, block args
**Side tables per instruction:**
- `inst_effect_class: InstId -> {Pure, ReadLocal, WriteLocal, Alloc, ReadShared, WriteShared, IO, FFI}`
- `inst_movable: bool` derived from class

### 10) Effect-qualified opts

**Consumes:** `inst_effect_class` + `func_effect_summary`
Gates CSE/LICM/DSE etc.

### 11) ARC insertion

**Consumes:** escape/capture info, SSA graph
Effects matter only for motion; encoded in instruction classes.

### 12) C emission

No effect system needed; just emit.


## Normalizer reduction rules

After residualization, apply these reduction rules to clean up the IR:

**Beta-reduction:** When a known function is called, inline its body (subject to size/usage
heuristics: always inline `Once`-used, inline `Many`-used if body small, never inline `Recursive`).

**Val commutation:** Flatten nested bindings and push branching outward:

val x = if (cond) thn else els; body
→ def k(x) = body; if (cond) { val x1 = thn; k(x1) } else { val x2 = els; k(x2) }

val x = { val y = s1; s2 }; s3
→ val y = s1; val x = s2; s3

val x = return e; body
→ let x = e; body

text


**Constant folding on branches:** `if (true) thn else els → thn`

**Match reduction:** `match Make(tag, args) { case tag(params) => body } → body[params/args]`

These rules fire after handler specialization removes dispatch overhead, exposing
further simplification opportunities.

[impl-ref: Normalizer.normalize, normalizeVal, active, shouldInline — core/optimizer/Normalizer.scala]


## Static argument transformation → handler specialization

When a recursive function passes some arguments unchanged through every recursive call,
split into wrapper (takes all args) and worker (takes only changing args, closes over static ones).

// Before:
def loop(handler, n) = if n == 0 then handler.get() else loop(handler, n-1)

// After (handler is static):
def loop(handler, n_fresh) =
def loop_worker(n) = if n == 0 then handler.get() else loop_worker(n-1)
loop_worker(n_fresh)

text


The handler capability becomes part of the worker's closure. All handler dispatches inside
become direct calls to known operations.

Analysis: collect all recursive functions and the arguments at their call sites. For each
argument position, check if it's always passed unchanged (same variable). If so, mark it static.

[impl-ref: StaticArguments.transform, IsStatic, wrapDefinition — core/optimizer/StaticArguments.scala;
RecursiveFunction, Recursive.process — core/Recursive.scala]


## Tail resumption detection

A handler clause is tail-resumptive if the continuation `k` appears only in tail position
as `resume(k, value)` — never captured, never passed to other functions, never used under
Reset/Region.

Syntactic check: walk the clause body. At each node, check if `k` appears free in non-tail
positions. If `k` only appears as `Resume(k, body)` at the end, the clause is tail-resumptive.

When true, `resume(k, body)` is replaced with just `body` — the shift/reset overhead disappears.

Important: mutable variable definitions (`Var`) are NOT considered tail-resumptive even when
they look like it. Mutable state handlers interact with backtracking in subtle ways.

[impl-ref: tailResumptive, removeTailResumption — core/optimizer/RemoveTailResumptions.scala]


## Calling convention classification (per-clause)

After tail resumption detection:
- `tailResumptive(k, clauseBody) == true` → **DirectCall** (function call, no continuation capture)
- otherwise → **ControlCall** (CPS transform, continuation reified as closure)
- handler fully discharged at CT → **PureCall** (eliminated, no IR node)

These three should be distinct IR node types after handler lowering.

Selective CPS: only ControlCall clauses trigger CPS transformation. DirectCall clauses
compile as regular function calls with evidence/capability argument. This avoids the
overhead of a full CPS transformation across the entire program.

[impl-ref: Continuation.Static vs Continuation.Dynamic — cps/Transformer.scala;
CallingConvention enum — core/Transformer.scala]


## Contify analysis (join point recovery)

After handler lowering, some functions always return to the same place. These can become
local continuations (join points) — calls become jumps, no closure allocation needed.

Analysis: for each function `f`, collect the set of continuations it returns to across all
call sites. If the set has exactly one element `k`, and `k` is in scope at `f`'s definition,
and `f` is not recursive, then `f` can become a join point.

// Before:
def f(x) = ... return result to k ...
call f(arg) with k

// After:
let_cont f(x) = ... jump f_cont(result) ...
jump f(arg)

text


Scoping check: `k` must be free in the rest of the program after `f`'s definition. If `k`
was defined in a different scope, the transformation is invalid.

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
effect check, or staging decision pushes a frame. On error, the chain produces messages like:

Cannot use handle inside @comptime block.
handle has type FileHandle which is not persistable
because FileHandle contains an OS resource
opened at src/main.cielo:12:5

text


For staging provenance specifically, each RT classification carries a single reason enum.
Full chains are rendered on demand by walking the IR dependency structure.

[impl-ref: ErrorContext sealed trait hierarchy — typer/ErrorContext.scala]


## Pattern matching compilation

Uses the Jules Jacobs algorithm. Clauses as conjunction of conditions (patterns, predicates,
bindings). Branching heuristic: split on the scrutinee mentioned by the most clauses.

Each clause body is wrapped in a join point function. The compiled match calls into it.
This enables sharing when multiple paths lead to the same clause.

For staging: if the scrutinee is CT-known, the match compiler can evaluate which branch
is taken at compile time (dead-branch elimination).

[impl-ref: PatternMatchingCompiler.compile, Clause, Condition, Pattern — core/PatternMatchingCompiler.scala]


## Rewrite/query pass infrastructure

Passes built on partial-function-based rewrite pattern:

class Rewrite {
def stmt: PartialFunction[Stmt, Stmt] = PartialFunction.empty
def rewrite(s: Stmt): Stmt = rewriteStructurally(s, stmt)
}

text


Override only the cases you care about; everything else is structural recursion. For large
programs, trampolined variant prevents stack overflow. Context-carrying variant for passes
needing environment info.

Analysis passes use Query trait: accumulate results instead of transforming.

[impl-ref: Tree.Rewrite, Tree.TrampolinedRewrite, Tree.RewriteWithContext, Tree.Query — core/Tree.scala]
