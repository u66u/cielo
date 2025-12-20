# Implementation References

Maps Cielo features to reference implementations in Effekt's source code.
These are not copy targets — they're patterns to study and adapt.

## Core IR

### Expr/Stmt/Block split
- **Files:** `core/Type.scala`, `core/Tree.scala`
- **Key types:** `ValueType`, `BlockType` enums (Type.scala); `Expr`, `Block`, `Stmt` enums (Tree.scala)
- **Pattern:** `Expr.PureApp` is an Expr (pure). `Stmt.App` is a Stmt (effectful). `Stmt.ImpureApp` is for FFI calls that bind a result and continue — already ANF.
- **Typing:** `Type.typecheck(expr): Typing[ValueType]` computes (type, captures, free vars) per node.

### Free variable tracking
- **File:** `core/Type.scala`
- **Key type:** `Free` sealed trait with cases `Empty`, `Join`, `Value`, `Block`, `Without`, `Defer`
- **Pattern:** Lazy composition. `Free.Join(left, right)` merges two subtrees. `Free.Without(params, underlying)` subtracts bound variables. Results cached via `lazy val freeValues`, `lazy val freeBlocks`, `lazy val freeIds`.
- **Also:** `cps/Tree.scala` → `Variables.free(s: Stmt): Variables` — simpler version on CPS IR, distinguishes value/block/cont/meta kinds.

### Declaration lookup
- **File:** `core/DeclarationContext.scala`
- **Key class:** `DeclarationContext(declarations, externs)`
- **Pattern:** Lazy maps `datas`, `interfaces`, `constructors`, `fields`, `properties`. `find*` returns Option, `get*` panics with context message.


## Optimization Passes

### Usage / reachability analysis
- **File:** `core/optimizer/Reachable.scala`
- **Key:** `Reachable.apply(entrypoints, module): Map[Id, Usage]`
- **Pattern:** Walk from entry points, track `seen` set and `stack` for recursion detection. `process(id)` increments usage and recursively processes definition if not yet seen. If id is on the stack → `Usage.Recursive`.
- **Usage enum:** `Never | Once | Many | Recursive` with `+`, `*`, `decrement` operations.

### Dead code elimination
- **File:** `core/optimizer/Deadcode.scala`
- **Key:** `Deadcode.remove(entrypoints, module): ModuleDecl`
- **Pattern:** Extends `TrampolinedRewrite`. Checks `used(id)` before emitting definitions. Drops unused match clauses, toplevel defs, externs.

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

### Static argument transformation
- **File:** `core/optimizer/StaticArguments.scala`
- **Key:** `StaticArguments.transform(entrypoint, module)`
- **Analysis:** `Recursive.apply(module)` → `RecursiveFunction(definition, targs, vargs, bargs)` per recursive function. Check which argument positions are invariant across all recursive calls.
- **Transform:** `wrapDefinition(id, blockLit)` — create wrapper with all params, worker with only dynamic params. Worker closes over static params.

### Tail resumption detection
- **File:** `core/optimizer/RemoveTailResumptions.scala`
- **Key:** `tailResumptive(k: Id, stmt: Stmt): Boolean`
- **Pattern:** Walk stmt. `Resume(k2, body)` where `k2 == k` → true (tail resume). `Return(_)`, `App(...)`, `Reset(...)` → false (doesn't resume or not in tail position). `If/Match/Let/Def` → check recursively that k not free in non-tail parts.
- **Removal:** `removeTailResumption(k, tpe, body)` — replace `Resume(k, body)` with just `body`.
- **Caveat:** `Stmt.Var` returns false — mutable state handlers interact with backtracking.

### Direct style recovery
- **File:** `core/optimizer/DirectStyle.scala`
- **Key:** `canBeDirect(s: Stmt): Boolean`, `toDirectStyle(stmt, label)`
- **Pattern:** `val x = { ... return 42 }; body` → `def l(x) = body; ... l(42)`. The binding becomes a join point. `canBeDirect` checks no non-tail calls, resets, shifts, or regions.

### ANF / bind subexpressions
- **File:** `core/optimizer/BindSubexpressions.scala`
- **Key:** `BindSubexpressions.transform(module)`
- **Pattern:** Uses `Bind` monad (Tree.scala). Each `bind(expr)` introduces `let tmp = expr` and returns the variable. Monadic composition accumulates bindings. `run` wraps them around continuation.
- **Also removes aliasing:** `let x = y` → substitute x for y, no binding emitted.

### Contify (join point recovery)
- **File:** `cps/Contify.scala`
- **Key:** `returnsTo(id: Id, body: Stmt): Set[Cont]` — collects all continuations a function returns to.
- **Decision:** If `returnsTo` gives exactly one continuation, in scope, and function not recursive → convert to local continuation. Calls become jumps.
- **Scope check:** `Variables.free(rewrittenRest) contains k` — the continuation must be free in the rest of the program.


## CT Evaluator

### Tree-walking interpreter
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

### Instrumentation
- **File:** `core/vm/Instrumentation.scala`
- **Key trait:** `Instrumentation` with hooks: `step(state)`, `allocate(v)`, `staticDispatch(id)`, `dynamicDispatch(id)`, `reset()`, `shift()`, `resume()`, `builtin(name)`.
- **Counter impl:** `Counting` class tallies each event.


## Type System / Effect Inference

### Capture constraint graph
- **File:** `typer/Constraints.scala`
- **Key class:** `Constraints`
- **Node data:** `CaptureNodeData(lower: Option[Set[Capture]], upper: Option[Set[Capture]], lowerNodes: Map[CNode, Filter], upperNodes: Map[CNode, Filter])`
- **Propagation:** `propagateLower(bounds, x)` — set lower bound, check against upper, flow to upperNodes. `propagateUpper(bounds, x)` — set upper bound, check against lower, flow to lowerNodes.
- **Solving:** `leave(types, capts)` — mark variables as pending inactive. `removableNodes()` computes transitive closure of removable nodes. `solve(toRemove)` assigns lower bound as solution.
- **Type constraints:** Union-find via `classes: Map[UnificationVar, Node]`. `learn(x, y)` connects nodes or assigns type. `getNode(x)` with path compression.

### Concrete effects check
- **File:** `typer/ConcreteEffects.scala`
- **Key:** `assertConcreteEffect(eff)` — checks `unknowns(eff)` is empty. If not, aborts with "Effects need to be fully known."
- **`unknowns`:** Recursively collects `UnificationVar` and `CaptUnificationVar` from types. If any remain, the effect is not concrete.

### Fresh capabilities per scope
- **File:** `typer/CapabilityScope.scala`
- **Key classes:** `BindAll`, `BindSome`, `GlobalCapabilityScope`
- **`BindAll.capabilityFor(tpe)`:** If capability for this effect type exists in current scope, return it. Otherwise create fresh one and record it.
- **`BindSome.capabilityFor(tpe)`:** Look in explicit map first. If not found, try lexical resolution (for partially known types). Fall through to parent scope.
- **Scope chain:** `parent: CapabilityScope` — linked list of scopes walked during capability resolution.

### Scope escape / wellformedness
- **File:** `typer/Wellformedness.scala`
- **Key:** `wellformed(o: Type)(using ctx: WFContext): Wellformed`
- **`WFContext`:** `typesInScope: Set[TypeVar], capturesInScope: Set[Capture]`
- **Check:** `freeCapture(o) -- ctx.capturesInScope` and `freeTypes(o) -- ctx.typesInScope`. If either non-empty → `Wellformed.No`.
- **Used at:** handler return types, region return types, function return types, block literal return types. Same algorithm, different error messages.

### Structured error context
- **File:** `typer/ErrorContext.scala`
- **Key:** `sealed trait ErrorContext` with cases `Expected`, `PatternMatch`, `MergeTypes`, `FunctionArgument`, `FunctionReturn`, `TypeConstructor`, etc.
- **Rendering:** `explainMismatch(tpe1, tpe2, ctx)` walks the context chain to build a multi-line error message.


## Source → Core Transformation

### Calling convention classification
- **File:** `core/Transformer.scala`
- **Key:** `enum CallingConvention { Pure, Direct, Control }`
- **`callingConvention(callable)`:** Pure if capture is empty. Direct if capture is IO-only. Control otherwise.
- **Eta-expansion:** `etaExpandPure`, `etaExpandDirect` wrap extern functions to match their calling convention.

### Pattern matching compilation
- **File:** `core/PatternMatchingCompiler.scala`
- **Key:** `compile(clauses: List[Clause], motif: ValueType): Stmt`
- **Clause:** `conditions: List[Condition], label: BlockVar, targs, args` — conjunction of conditions, jump to label when all satisfied.
- **Branching heuristic:** `branchingHeuristic(patterns, clauses)` — choose scrutinee mentioned by most clauses.
- **Join points:** Each clause body wrapped in a BlockLit. Compiled match jumps to it via `App(label, args)`.


## Pass Infrastructure

### Rewrite (partial function based)
- **File:** `core/Tree.scala`
- **Classes:** `Tree.Rewrite`, `Tree.TrampolinedRewrite`, `Tree.RewriteWithContext[Ctx]`, `Tree.Query[Ctx, Res]`
- **Pattern:** Override `def stmt: PartialFunction[Stmt, Stmt]` for cases you handle. Structural recursion for everything else via `rewriteStructurally`.
- **Trampoline:** `TrampolinedRewrite` returns `Trampoline[T]` from each rewrite, preventing stack overflow on deep trees.

### Substitution
- **File:** `core/Tree.scala` (object `substitutions`)
- **Key:** `Substitution(vtypes, captures, values, blocks)` — maps for each kind.
- **Shadowing:** `shadowTypes`, `shadowCaptures`, `shadowValues`, `shadowBlocks` — prevent substituting into bound variables.
- **Inline entry:** `substitute(block: BlockLit, targs, vargs, bargs): Stmt` — creates maps from params→args, applies to body.

### Renaming (alpha conversion)
- **File:** `core/Renamer.scala`
- **Key:** `Renamer.rename(b: BlockLit): (BlockLit, HashMap[Id, Id])` — freshens bound names, returns old→new mapping.
- **Scope management:** `withBindings(ids)(f)` — push fresh names for ids, run f, pop.

### Selective CPS (concept)
- **File:** `cps/Transformer.scala`
- **Key:** `enum Continuation { Dynamic(id), Static(hint, k) }`
- **Static:** continuation is known at compile time. Body is inlined. No closure allocated. Maps to DirectCall.
- **Dynamic:** continuation is a variable. Must be reified as ContLam. Maps to ControlCall.
- **`withJoinpoint(k)(body)`:** If continuation is dynamic, use it directly. If static and body > 5 nodes, create a named join point to avoid duplicating the continuation.


## Side Table / Annotation System

### Typed annotations
- **File:** `context/Annotations.scala`
- **Key types:** `TreeAnnotation[K, V]`, `SymbolAnnotation[K, V]`, `SourceAnnotation[K, V]`
- **Storage:** `db: IdentityHashMap[source.Tree, Map[TreeAnnotation[_, _], Any]]` — identity-based lookup.
- **Local annotations:** `class Annotations` — backtrackable, can be snapshot and restored. Used during typer for speculative analysis.
- **Commit:** `updateAndCommit` transfers local annotations to global DB.

### Compiler context
- **File:** `context/Context.scala`
- **Key:** `abstract class Context extends NamerOps with TyperOps with ModuleDB with TransformerOps with Timers`
- **Scoping:** `def in[T](block: => T): T` — save/restore focus and module across block.
- **State:** `case class State(annotations, cache)` with `backup`/`restore` for rollback.


## Plan Notes Sync

### V1
- Keep phase integrity strict per `docs/Decisions.md` (phase-safe IDs + attached analyses).
- Keep each slice independently testable and end-to-end runnable (`--emit-c --run-c`).
- Prefer conservative semantics first, then optimize.
- Calling-convention grounding reference: `core/Transformer.scala` (`Pure`/`Direct`/`Control`).
- Handler-specialization grounding references:
  - `core/optimizer/StaticArguments.scala`
  - `core/optimizer/Reachable.scala`

### V2
- Generalized specialization remains deferred: parameterize specialized workers by return-clause continuation (selective CPS over specialization sites).
- If mutable-variable stmt forms are introduced in Core IR, tail-resumption checks must mirror the mutable-state caveat used by `RemoveTailResumptions`.
