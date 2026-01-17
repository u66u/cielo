# Implementation References

Maps Cielo features to reference implementations in Effekt's source code and
relevant research. These are not copy targets — they're patterns to study and adapt.

## Core IR

### Expr/Stmt split
- **Files:** `core/Type.scala`, `core/Tree.scala`
- **Key types:** `ValueType`, `BlockType` enums (Type.scala); `Expr`, `Block`, `Stmt` enums (Tree.scala)
- **Pattern:** `Expr.PureApp` is an Expr (pure). `Stmt.App` is a Stmt (effectful). `Stmt.ImpureApp` is for FFI calls that bind a result and continue — already ANF.
- **Typing:** `Type.typecheck(expr): Typing[ValueType]` computes (type, captures, free vars) per node.
- **Cielo mapping:** `ExprKind` ↔ Effekt `Expr` variants. `StmtKind` ↔ Effekt `Stmt` variants. `LinearStmt` ↔ Effekt CPS IR after handler lowering.

### Free variable tracking
- **File:** `core/Type.scala`
- **Key type:** `Free` sealed trait with cases `Empty`, `Join`, `Value`, `Block`, `Without`, `Defer`
- **Pattern:** Lazy composition. `Free.Join(left, right)` merges two subtrees. `Free.Without(params, underlying)` subtracts bound variables. Results cached via `lazy val freeValues`, `lazy val freeBlocks`, `lazy val freeIds`.
- **Also:** `cps/Tree.scala` → `Variables.free(s: Stmt): Variables` — simpler version on CPS IR, distinguishes value/block/cont/meta kinds.

### Declaration lookup
- **File:** `core/DeclarationContext.scala`
- **Key class:** `DeclarationContext(declarations, externs)`
- **Pattern:** Lazy maps `datas`, `interfaces`, `constructors`, `fields`, `properties`. `find*` returns Option, `get*` panics with context message.

### Generic IR traversal
- **Cielo addition:** `IrStmtNode` / `IrProgram` traits in `ir/walk.rs` provide generic traversal across Core and Linear IRs. Replaces duplicated `child_stmts()` / `child_exprs()` methods with trait implementations.
- **MLIR reference:** MLIR operation interfaces provide a similar pattern at much larger scale (dozens of dialects). Our two-IR version is deliberately minimal.


## Evaluate+Classify Pass (fused CT propagation + BTA)

### CT evaluator (tree-walking interpreter)
- **File:** `core/vm/VM.scala`
- **Key class:** `Interpreter`
- **State:** `enum State { Done(Value), Step(Stmt, Env, Stack, Heap) }`
- **Main loop:** `step(s: State): State` — pattern match on Stmt, produce next state. `run` calls `step` in a `@tailrec` loop.
- **Effect handling:**
  - `Reset(BlockLit(_, _, _, prompt :: Nil, body))` → push new segment on stack with fresh prompt address
  - `Shift(prompt, resume, body)` → `unwind(stack)` to find prompt, capture frames as `Resumption(cont)`, continue with remaining stack
  - `Resume(k, body)` → `rewind(cont, stack)` to reinstate captured frames
- **Mutable state:**
  - `Var(ref, init, capture, body)` → push `Frame.Var(addr, value)` on stack
  - `Get(id, tpe, ref, capt, body)` → `findFirst(stack) { case Frame.Var(addr, value) if ... => value }`
  - `Put(ref, capt, value, body)` → `updateOnce(stack) { case Frame.Var(addr, _) if ... => Frame.Var(addr, newValue) }`
- **Value types:** `enum Value { Literal(Any), Data(data, tag, fields), Boxed(Computation), Array(...), ... }`
- **Environment:** `enum Env { Top(functions, builtins, toplevel, decls), Static(id, block, rest), Dynamic(id, block, rest), Let(id, value, rest) }` — linked list with `lookupValue`/`lookupStatic` traversal.
- **Cielo adaptation:** Cielo's evaluator produces `Outcome` (Known/Stuck) instead of only `Value`. When evaluation hits a stuck point (RT variable, non-CT-eligible effect), it produces `Stuck(reason)` instead of aborting. This enables the fused evaluate+classify architecture.

### Instrumentation
- **File:** `core/vm/Instrumentation.scala`
- **Key trait:** `Instrumentation` with hooks: `step(state)`, `allocate(v)`, `staticDispatch(id)`, `dynamicDispatch(id)`, `reset()`, `shift()`, `resume()`, `builtin(name)`.
- **Counter impl:** `Counting` class tallies each event.

### BTA classification (now inline in evaluator)
- **Cielo design:** Classification is inline during evaluation, not a separate pass. Each expression is classified as Known or Stuck as the evaluator visits it. Handler discharge analysis is per-clause and computed when the evaluator encounters a Handle node. No separate BTA walk.
- **Previous Effekt reference:** Effekt does not have a BTA pass — it uses a different compilation strategy. Cielo's BTA is inspired by partial evaluation literature (binding-time analysis from Jones, Gomard, Sestoft "Partial Evaluation and Automatic Program Generation" (1993)).

### Function-level memoization
- **Cielo design:** Function results cached as `(FuncId, Vec<Value>) → Outcome`. Cycle detection via in-progress set. This is the minimal query pattern, extensible to a full Salsa-style query system in v2.
- **Research reference:** Salsa (rust-analyzer's query framework) for the demand-driven + memoized + cycle-detecting pattern.

### Partially-static data (v2)
- **Research reference:** Yallop, von Glehn, Kammar — "Partially-static data as free extension of algebras" (ICFP 2018). Key insight: partial static data is the free extension of a static algebra by dynamic generators. Enables field-level CT evaluation for structs with mixed Known/Stuck fields.

### Staging-by-evaluation theory
- **Research reference:** Kovács — "Staged Compilation with Two-Level Type Theory" (ICFP 2022). Shows that staging can be given by evaluation in a semantic domain. Cielo's fused evaluate+classify pass is structurally similar but operates on effects-aware IR instead of 2LTT terms.
- **Research reference:** Kovács — "Closure-Free Functional Programming in a Two-Level Type Theory" (ICFP 2024). Extends the approach, showing metaprograms can replace general-purpose optimization.


## Optimization Passes

### Usage / reachability analysis
- **File:** `core/optimizer/Reachable.scala`
- **Key:** `Reachable.apply(entrypoints, module): Map[Id, Usage]`
- **Pattern:** Walk from entry points, track `seen` set and `stack` for recursion detection. `process(id)` increments usage and recursively processes definition if not yet seen. If id is on the stack → `Usage.Recursive`.
- **Usage enum:** `Never | Once | Many | Recursive` with `+`, `*`, `decrement` operations.
- **Cielo:** Usage analysis is a standalone reusable module consumed by the evaluator, normalizer, and residualizer.

### Dead code elimination
- **File:** `core/optimizer/Deadcode.scala`
- **Key:** `Deadcode.remove(entrypoints, module): ModuleDecl`
- **Pattern:** Extends `TrampolinedRewrite`. Checks `used(id)` before emitting definitions.
- **Cielo:** Dead code elimination is folded into normalizer shrinking reductions (dead bindings removed during Phase A).

### Normalizer (beta-reduction, val commutation)
- **File:** `core/optimizer/Normalizer.scala`
- **Key functions:**
  - `normalize(s: Stmt)` — main dispatch
  - `active(b: Block)` — chase through aliases, cancel box/unbox, classify as Known/Unknown
  - `shouldInline(b, boundBy, blockArgs)` — heuristic: not recursive, once-used or small body, or higher-order with known arg
  - `normalizeVal(id, binding, body)` — val commutation rules
  - `reduce(b: BlockLit, targs, vargs, bargs)` — beta-reduction with usage update
- **Val commutation rules in `normalizeVal`:**
  - `val x = return e; body` → `let x = e; body`
  - `val x = if(...) ... ; body` → joinpoint + distribute
  - `val x = match(...) ... ; body` → joinpoint + distribute
  - `val x = { val y = s1; s2 }; s3` → `val y = s1; val x = s2; s3`
  - `val x = { def f = ...; s }; body` → `def f = ...; val x = s; body`
- **Cielo adaptation:** Normalizer separated into shrinking reductions (always safe, to fixpoint) and speculative inlining (usage-gated, once). Sandwich structure: shrink → inline → shrink.
- **Research reference:** Appel & Jim — "Shrinking lambda expressions in linear time" (JFP 1997). The shrinking/speculative separation prevents non-elementary blowup.
- **Research reference:** SML.NET compiler uses restricted normalization based on shrinking reductions; original algorithm is quadratic worst-case but practical.

### Static argument transformation
- **File:** `core/optimizer/StaticArguments.scala`
- **Key:** `StaticArguments.transform(entrypoint, module)`
- **Analysis:** `Recursive.apply(module)` → `RecursiveFunction(definition, targs, vargs, bargs)` per recursive function. Check which argument positions are invariant across all recursive calls.
- **Transform:** `wrapDefinition(id, blockLit)` — create wrapper with all params, worker with only dynamic params. Worker closes over static params.
- **Cielo:** Integrated into Residualize+Specialize pass. When handler specialization fires, static argument transformation applies to the specialized copy.

### Tail resumption detection
- **File:** `core/optimizer/RemoveTailResumptions.scala`
- **Key:** `tailResumptive(k: Id, stmt: Stmt): Boolean`
- **Pattern:** Walk stmt. `Resume(k2, body)` where `k2 == k` → true (tail resume). `Return(_)`, `App(...)`, `Reset(...)` → false. `If/Match/Let/Def` → check recursively that k not free in non-tail parts.
- **Removal:** `removeTailResumption(k, tpe, body)` — replace `Resume(k, body)` with just `body`.
- **Caveat:** `Stmt.Var` returns false — mutable state handlers interact with backtracking. Statement cycles conservatively non-tail.
- **Cielo:** Performed during Evaluate+Classify (for clause discharge classification) and verified during linearize (for calling convention assignment). Memoized with `HashMap<StmtId, bool>`.

### Direct style recovery
- **File:** `core/optimizer/DirectStyle.scala`
- **Key:** `canBeDirect(s: Stmt): Boolean`, `toDirectStyle(stmt, label)`
- **Pattern:** `val x = { ... return 42 }; body` → `def l(x) = body; ... l(42)`. The binding becomes a join point.

### Contify (join point recovery)
- **File:** `cps/Contify.scala`
- **Key:** `returnsTo(id: Id, body: Stmt): Set[Cont]` — collects all continuations a function returns to.
- **Decision:** If `returnsTo` gives exactly one continuation, in scope, and function not recursive → convert to local continuation.
- **Scope check:** `Variables.free(rewrittenRest) contains k` — the continuation must be free in the rest of the program.


## Residualize+Specialize Pass

### Handler specialization
- **Effekt files:** `core/optimizer/StaticArguments.scala`, `core/optimizer/Reachable.scala`
- **Cielo design:** Integrated into residualization walk. When encountering a non-discharged handler wrapping a specializable call pattern, creates specialized copy immediately. Reuse static arguments analysis to identify which arguments are invariant across recursive calls.
- **Termination:** `Set<(FuncId, HandlerId)>` tracks what has been specialized. Already-specialized functions not re-specialized.

### Constant table and embedding
- **Cielo design:** Three-tier embedding: InlineLiteral (trivial), StaticConst (serializable, single-use, small), Pooled (serializable, multi-use or large). Structural dedup via hash. Per-entry and per-unit size caps.
- **C emission reference:** Scalar pooling emits `static const CieloValue`. Constructor pooling for nested ADT constants (top-level in v1, recursive in v2).

### Purity-relative-to-handler
- **Cielo design:** Before applying handler reduction, intersect computation's effect row with handler's operation set. If empty → apply return clause only or eliminate if return clause is identity.
- **Research reference:** Eff compiler — "source-to-source transformations... exploit the explicit type and effect information" (Pretnar et al.). Same optimization under a different name.

### Effect summary recomputation
- **Cielo design:** Walk residual function bodies, collect remaining Perform nodes. Union = residual effect row per function. Fixpoint over call edges + handler subtraction.


## Handler Lowering (Linearize)

### Calling convention classification
- **File:** `core/Transformer.scala`
- **Key:** `enum CallingConvention { Pure, Direct, Control }`
- **Cielo mapping:** `classify_clause_convention` in `linearize.rs` implements the same three-way classification. `ClauseConvention::Pure` / `Direct` / `Control` map to `LinearStmt::PureCall` / `DirectCall` / `ControlCall`.
- **Research reference:** Müller, Schuster, Starup, Ostermann, Brachthäuser — "From Capabilities to Regions: Enabling Efficient Compilation of Lexical Effect Handlers" (OOPSLA 2023). Shows capability scopes can be compiled via region scopes, unifying handler evidence allocation with region allocation. Deferred to v2.

### Selective CPS
- **File:** `cps/Transformer.scala`
- **Key:** `enum Continuation { Dynamic(id), Static(hint, k) }`
- **Static:** continuation known at compile time. Body inlined. No closure allocated. Maps to DirectCall.
- **Dynamic:** continuation is a variable. Must be reified as ContLam. Maps to ControlCall.
- **Research reference:** Gaißert, Bolz-Tereick, Brachthäuser — "Tracing Just-in-Time Compilation for Effects and Handlers" (OOPSLA 2025). Shows that the trace/residual structure for effect handlers matches what a CT evaluator + residualizer produces. Relevant for understanding the relationship between staging and handler compilation.

### Pattern matching compilation
- **File:** `core/PatternMatchingCompiler.scala`
- **Key:** `compile(clauses: List[Clause], motif: ValueType): Stmt`
- **Clause:** `conditions: List[Condition], label: BlockVar, targs, args` — conjunction of conditions, jump to label when all satisfied.
- **Branching heuristic:** `branchingHeuristic(patterns, clauses)` — choose scrutinee mentioned by most clauses.
- **Join points:** Each clause body wrapped in a BlockLit. Compiled match jumps to it via `App(label, args)`.


## Type System / Effect Inference

### Capture constraint graph
- **File:** `typer/Constraints.scala`
- **Key class:** `Constraints`
- **Node data:** `CaptureNodeData(lower, upper, lowerNodes, upperNodes)`
- **Propagation:** `propagateLower(bounds, x)` — set lower bound, check against upper, flow to upperNodes. `propagateUpper(bounds, x)` — set upper bound, check against lower, flow to lowerNodes.
- **Solving:** `leave(types, capts)` → `removableNodes()` computes transitive closure → `solve(toRemove)` assigns lower bound as solution.
- **Type constraints:** Union-find via `classes: Map[UnificationVar, Node]`. `learn(x, y)` connects nodes. `getNode(x)` with path compression.

### Concrete effects check
- **File:** `typer/ConcreteEffects.scala`
- **Key:** `assertConcreteEffect(eff)` — checks `unknowns(eff)` is empty.
- **`unknowns`:** Recursively collects `UnificationVar` and `CaptUnificationVar` from types.

### Fresh capabilities per scope
- **File:** `typer/CapabilityScope.scala`
- **Key classes:** `BindAll`, `BindSome`, `GlobalCapabilityScope`
- **`BindAll.capabilityFor(tpe)`:** If capability for this effect type exists in current scope, return it. Otherwise create fresh one and record it.
- **Scope chain:** `parent: CapabilityScope` — linked list of scopes walked during capability resolution.

### Scope escape / wellformedness
- **File:** `typer/Wellformedness.scala`
- **Key:** `wellformed(o: Type)(using ctx: WFContext): Wellformed`
- **Check:** `freeCapture(o) -- ctx.capturesInScope` and `freeTypes(o) -- ctx.typesInScope`. If either non-empty → `Wellformed.No`.
- **Used at:** handler return types, region return types, function return types, block literal return types. Same algorithm, different error messages.

### Structured error context
- **File:** `typer/ErrorContext.scala`
- **Key:** `sealed trait ErrorContext` with cases `Expected`, `PatternMatch`, `MergeTypes`, `FunctionArgument`, `FunctionReturn`, `TypeConstructor`, etc.
- **Rendering:** `explainMismatch(tpe1, tpe2, ctx)` walks the context chain to build a multi-line error message.


## Source → Core Transformation

### Calling convention classification (source level)
- **File:** `core/Transformer.scala`
- **Key:** `enum CallingConvention { Pure, Direct, Control }`
- **`callingConvention(callable)`:** Pure if capture is empty. Direct if capture is IO-only. Control otherwise.
- **Eta-expansion:** `etaExpandPure`, `etaExpandDirect` wrap extern functions to match their calling convention.

### Substitution
- **File:** `core/Tree.scala` (object `substitutions`)
- **Key:** `Substitution(vtypes, captures, values, blocks)` — maps for each kind.
- **Shadowing:** `shadowTypes`, `shadowCaptures`, `shadowValues`, `shadowBlocks` — prevent substituting into bound variables.
- **Inline entry:** `substitute(block: BlockLit, targs, vargs, bargs): Stmt` — creates maps from params→args, applies to body.

### Renaming (alpha conversion)
- **File:** `core/Renamer.scala`
- **Key:** `Renamer.rename(b: BlockLit): (BlockLit, HashMap[Id, Id])` — freshens bound names, returns old→new mapping.
- **Scope management:** `withBindings(ids)(f)` — push fresh names for ids, run f, pop.
- **Cielo:** Needed by normalizer speculative inlining (alpha-rename callee locals to avoid capture when inlining into a new context).


## Pass Infrastructure

### Rewrite (partial function based)
- **File:** `core/Tree.scala`
- **Classes:** `Tree.Rewrite`, `Tree.TrampolinedRewrite`, `Tree.RewriteWithContext[Ctx]`, `Tree.Query[Ctx, Res]`
- **Pattern:** Override `def stmt: PartialFunction[Stmt, Stmt]` for cases you handle. Structural recursion for everything else via `rewriteStructurally`.
- **Cielo:** IrNode trait provides the structural recursion base for generic walkers across both Core and Linear IRs.


## Side Table / Annotation System

### Typed annotations (Effekt)
- **File:** `context/Annotations.scala`
- **Key types:** `TreeAnnotation[K, V]`, `SymbolAnnotation[K, V]`, `SourceAnnotation[K, V]`
- **Storage:** `db: IdentityHashMap[source.Tree, Map[TreeAnnotation[_, _], Any]]` — identity-based lookup.
- **Local annotations:** `class Annotations` — backtrackable, can be snapshot and restored. Used during typer for speculative analysis.
- **Commit:** `updateAndCommit` transfers local annotations to global DB.

### Cielo side tables
- **Cielo uses `DenseMap<K, V>`** — Vec-backed, indexed by newtype IDs. Phase-specific ID types prevent cross-phase access.
- **Phase struct policy:** Each phase struct carries only its own tables + tables downstream passes read. Tables dropped at phase boundaries when no longer needed.

### Compiler context (Effekt)
- **File:** `context/Context.scala`
- **Key:** `abstract class Context extends NamerOps with TyperOps with ModuleDB with TransformerOps with Timers`
- **Scoping:** `def in[T](block: => T): T` — save/restore focus and module across block.
- **State:** `case class State(annotations, cache)` with `backup`/`restore` for rollback.


## Research References (by topic)

### Staging and partial evaluation
- Jones, Gomard, Sestoft — "Partial Evaluation and Automatic Program Generation" (1993). Foundational BTA and partial evaluation. Cielo's staging analysis is binding-time analysis adapted for effects.
- Kovács — "Staged Compilation with Two-Level Type Theory" (ICFP 2022). Staging-by-evaluation in a semantic domain. Validates fused evaluate+classify architecture.
- Kovács — "Closure-Free Functional Programming in a Two-Level Type Theory" (ICFP 2024). Metaprograms replacing general-purpose optimization.

### Normalization
- Appel & Jim — "Shrinking lambda expressions in linear time" (JFP 1997). Shrinking reductions that always decrease term size. Basis for normalizer Phase A.
- Granger — Edinburgh thesis on NbE for computational metalanguage. Shows fast rewrite-based normalizers are defunctionalized NbE. Validates our explicit normalizer approach.

### Effect handler compilation
- Pretnar et al. — Eff compiler. Source-to-source transformations exploiting type and effect information.
- Müller et al. — "From Capabilities to Regions" (OOPSLA 2023). Compiling lexical handlers via region scopes.
- Gaißert et al. — "Tracing JIT for Effects and Handlers" (OOPSLA 2025). Trace structure matches CT evaluator + residualizer output.
- Trifanov et al. — "Staging Effect Handlers for Modular Search" (PEPM 2026). Validates staging + handler specialization as complementary.

### Partially-static data
- Yallop, von Glehn, Kammar — "Partially-static data as free extension of algebras" (ICFP 2018). Enables field-level CT evaluation. Deferred to v2.

### Effectful NbE (theoretical background)
- Ahman & Staton — NbE for fine-grained call-by-value with algebraic effects (MFPS 2013). Theoretical basis; practical implementation for full handler systems remains open.
- Filinski — NbE for Moggi's computational λ-calculus. Residualizing monad handles code generation.


## Plan Notes Sync

### V1
- Keep phase integrity strict per `docs/v1/Decisions.md` (phase-safe IDs + attached analyses).
- Keep each pass independently testable and end-to-end runnable (`--emit-c --run-c`).
- Prefer conservative semantics first, then optimize.
- Calling-convention grounding reference: `core/Transformer.scala` (`Pure`/`Direct`/`Control`).
- Handler-specialization grounding references:
  - `core/optimizer/StaticArguments.scala`
  - `core/optimizer/Reachable.scala`
- Fused pass grounding:
  - Evaluate+Classify replaces separate ct_propagate + bta passes.
  - Residualize+Specialize replaces separate residualize + handler_specialize passes.
  - Normalizer uses shrink-inline-shrink sandwich per Appel & Jim.

### V2
- Generalized specialization remains deferred: parameterize specialized workers by return-clause continuation (selective CPS over specialization sites).
- If mutable-variable stmt forms are introduced in Core IR, tail-resumption checks must mirror the mutable-state caveat used by `RemoveTailResumptions`.
- Full query architecture replacing function-level memoization.
- Partially-static data (Partial variant in Outcome).
- Capability-to-region unification for handler evidence allocation.