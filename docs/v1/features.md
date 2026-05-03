# V1 Features

## Capability-Based Effect Organization

Rather than a flat set of effects, organize effects as a hierarchy of capabilities:

```text
// Capability hierarchy
capability Pure                      // always comptime-eligible
capability Diverge   extends Pure    // might not terminate (affects fuel)
capability Alloc     extends Pure    // heap allocation (comptime if evaluator supports it)
capability State[T]  extends Alloc   // mutable state
capability IO        extends State   // actual IO (always runtime)
capability FFI       extends IO      // foreign calls (always runtime, opaque)

Why this matters for staging: The hierarchy gives the compiler coarser-grained but faster
staging decisions. Anything below IO in the hierarchy is a comptime candidate. Anything
at IO or above is runtime. The fine-grained analysis (looking at handler values) only
needs to happen for the middle layers.

This also gives users an intuitive mental model: "IO and above = runtime, everything else
= compiler figures it out."
```

## Rules for Automatic Staging

The compiler needs decidable rules. Here's the candidate set:

Level 0 — Always comptime:

    Literal expressions
    Pure functions applied to comptime-known arguments
    Type-level computation

Level 1 — Comptime if handler is comptime:

    Effectful computation where all effects are handled by handlers with comptime-known
    arguments. After handler application, if no effects remain and no runtime values are
    needed → comptime.

Level 2 — Runtime:

    Anything with unhandled effects (IO, FFI, system calls)
    Anything depending on values only known at runtime
    Anything the user explicitly marks as runtime (for config-dependent behavior, etc.)

The critical rule: A computation's stage is determined by the residual effects after all
enclosing handlers are applied, plus the binding times of its free variables.

Diagnostics: measure comptime cost of each fn or other unit of computation and report
to user, allow them to set fuel (budget) for memory, time, iterations, etc.


## Using Effects to Derive Comptime vs Runtime

Eliminable effects — can always be discharged at compile time when the handler is comptime:

    State<T> — it's just threading a value through; a comptime interpreter handles this trivially
    Exception<E> / Raise — just control flow, fully evaluable
    Reader<T> — looking up a value from an environment
    Writer<T> — accumulating output
    Nondeterminism / Amb — enumerate possibilities
    Coroutine / Yield — interleaved execution, but fully deterministic given inputs

Opaque effects — inherently require runtime, no handler can change this:

    IO (filesystem, network, stdout)
    FFI (calling external code)
    System (clock, true randomness, process info)
    Concurrency (threads, channels — real parallelism is runtime-only)

Parametric effects — user-defined, staging depends on what the handler does:

```
effect Database {
    query(sql: String): Rows
}

// Comptime handler (mock/static data):
handler db_mock with Database {
    | query(sql) => resume(static_test_data)   // comptime!
}

// Runtime handler (actual DB connection). Not implemented yet: capturing
// `conn` in the clause bodies needs closures (CIELO-25).
handler db_real(conn: Connection) with Database {
    | query(sql) => resume(conn.execute(sql))   // runtime (conn is runtime, execute has IO)
}

The rule: After all handlers are applied, look at the residual effects. If none remain
(or only eliminable ones with comptime handlers remain), and all free variables are
comptime, the computation is comptime.
```


## Shared State vs. Local State Rewrites

The thesis mentions rewrites that are valid for Local state but invalid for Shared state.

**Rewrite 1: Read-after-Write (Forwarding)**

```
// Code
x := 1;
y := !x;

// Optimization
x := 1;
y := 1; // Valid for Local. INVALID for Shared.
```

_Why Invalid for Shared?_ Another thread could write to `x` between line 1 and 2.

**Rewrite 2: Write-after-Write (Dead Store)**

```
// Code
x := 1;
x := 2;

// Optimization
x := 2; // Valid for Local. INVALID for Shared.
```

_Why Invalid for Shared?_ Another thread might be observing the transition state where `x` was 1.

**Rewrite 3: Read-after-Read (CSE)**

```
// Code
y := !x;
z := !x;

// Optimization
y := !x;
z := y; // Valid for Local. INVALID for Shared.
```

_Why Invalid for Shared?_ `x` might change between the reads.

**Implementation Strategy:**

1. Define two effects: `State<T>` (Local) and `Atomic<T>` (Shared).
2. In your optimizer:

```
if effect == LocalState {
    enable_dead_store_elimination();
    enable_constant_propagation();
}
if effect == AtomicState {
    disable_optimizations(); // Treat as IO/Volatile
}
```

## Equality (`==`) vs. Preorder (⊑) Semantics

**Equality Semantics (A=B):**
"I can replace code A with code B **and vice versa**, and the program does _exactly_
the same thing."

- _Example:_ `2 + 2` can be replaced with `4`. `4` can be replaced with `2 + 2`.

**Preorder Semantics (A⊑B):**
"I can replace code A with code B because B is **better** (or more defined) than A."

- _Example:_ `flip_coin()` (nondeterministic) ⊑ `true` (deterministic).

**Practical takeaway:** You don't need to code "preorders" into your compiler. You just
need to mentally verify that every optimization pass moves "down" the preorder (making
the code more specific/efficient) and never "up" (making it more vague/effectful).


## Effect Capability Ordering

Give effects a coarse ordering:

```
Pure ≤ Diverge ≤ Alloc ≤ LocalState ≤ IO ≤ FFI
LocalState ≤ SharedState?  (usually NO; SharedState is "more dangerous" than LocalState)
```

Then define:

- `capability(effect)` → one of those levels
- `capability(effect_row)` → the **max** level in the row (or "top" if unknown)

Now you can formalize lots of compiler decisions:

- **staging**: comptime allowed iff `capability(effect_row) < IO` (plus other checks)
- **code motion**: allowed iff `is_thunkable(effect_row)` (derived from level + flags)
- **optimizations**: "this rewrite is allowed if the new code's capability is ≤ old code's capability"

This doesn't require fancy types. It's just metadata + a compare function.


## Effect-Qualified Optimizations

Tag your effect rows with properties (e.g., "Commutative", "Idempotent", "Discardable").

- **The Win:** Dead Code Elimination is valid if the effects in the dead code satisfy
  the "Discardable" property (like reading state, but not writing it).

Ambient effect contexts are a natural fit for "this region is comptime" vs "runtime" contexts.

## Using Modalities for `@comptime`

### Mental Model

**Entering `@comptime { ... }` is like switching to a restricted world** where only
certain effects are allowed and only certain outside values are allowed in.

### Simple Rule-Set

Think of `@comptime` as entering a new "mode":

- Allowed effects inside: `{Pure, Alloc, LocalState, ComptimeReadFiles, ...}`
- Disallowed effects inside: `{IO, FFI, SharedState, Concurrency, ...}`

When code inside `@comptime` references an outer variable `x`, require:

**Rule: Cross-stage persistence (Cielo version)**

`x` is allowed inside `@comptime` iff:

1. `outcome(x) == Known(v)` (it's actually evaluated at CT)
2. `is_persistable(type(x)) == true` (it's safe to carry into comptime)

`is_persistable` is the "Abs-like" predicate:

- persistable: ints/bools/strings/arrays/structs of persistables
- not persistable: file handles, pointers to runtime memory, OS resources, thread handles
- functions/closures: only persistable if all captures are persistable

Example:

```cielo
let n = 5                  // comptime (Known)
let fh = open("x.txt")    // runtime handle (IO) (Stuck)

@comptime {
  n + 1        // OK: n is Known, Int is persistable
  use(fh)      // error: fh is Stuck(EffectNotDischarged(IO)) and not persistable
}
```

This replaces fuzzy heuristics with a crisp rule users can learn. It improves diagnostics:
"this is runtime because it depends on `fh` (runtime handle)."


## Fused Staging Pass (Evaluate+Classify)

The compiler evaluates and classifies expressions in a single walk over the post-
monomorphization Core IR. Pure expressions with Known inputs are evaluated immediately.
Effectful expressions check handler discharge per-clause. Branch conditions that evaluate
to Known values trigger dead-branch elimination inline.

The evaluator produces `Outcome` per expression: either `Known(Value)` (fully evaluated
at CT) or `Stuck(Reason)` (cannot evaluate, carries the immediate cause).

Users see the result via `--staging-report`:
- How many expressions were CT vs RT
- Root causes of RT classification with taint counts
- Suggestions for what to make CT

Example output:
```
[staging] 847/1203 expressions evaluated at compile time (70.4%)
[staging] 12 handlers fully discharged, 3 partially discharged, 2 kept for runtime
[staging] constant table: 14 entries, 2.3 KB total

[staging] Top RT root causes:
  1. `request: Request` (parameter of main) → taints 342 expressions
  2. `conn: Connection` (parameter of handle_request) → taints 187 expressions

[staging] Suggestions:
  - Making `config` comptime would make 89 additional expressions CT
```


## Fused Residualization + Handler Specialization

After Evaluate+Classify, the Residualize+Specialize pass embeds constants, erases
discharged handlers, and pushes remaining handlers into function bodies — all in one
walk.

Handler specialization fires inline during residualization: when a non-discharged handler
wraps a specializable call pattern (direct wrapper or let-chain forwarding), a specialized
copy is created with the handler pushed in. Recursive calls are tied to the specialized
version. Each function specialized at most once per handler (termination guarantee).


## Normalizer

Post-residualization cleanup using a shrink-inline-shrink sandwich.

Shrinking reductions (dead code, constant branches, val commutation, beta-reduction of
once-used functions) run to fixpoint — always safe, always terminates, never grows code.

One round of usage-gated speculative inlining (Many-used functions below size threshold).

Shrink again to clean up after inlining.

No manual tuning needed. No e-graph/egg machinery for v1.

[impl-ref: Appel & Jim "Shrinking lambda" (1997); Normalizer.normalize —
core/optimizer/Normalizer.scala]


## Three Calling Conventions After Handler Lowering

After handler lowering (linearize pass), effect operations become one of three IR node types:

- **PureCall**: operation fully eliminated (discharged at CT or dead code). No IR node remains.
- **DirectCall**: tail-resumptive handler clause. Compiled as a regular function call with
  evidence/capability argument. No continuation capture. Covers ~90% of effect operations
  in practice (State get/put, Reader ask, Writer tell, Exception raise).
- **ControlCall**: non-tail-resumptive handler clause. Continuation reified as a closure.
  CPS transform applied only to the function containing this call, not globally.

This classification is per-clause, not per-handler. A single handler can have both
DirectCall and ControlCall clauses.


## Handler Specialization

When a handler wraps a recursive function call, the compiler creates a specialized copy
of the function with the handler pushed into its body. This enables handler reduction
rules to fire on each operation inside the function, eliminating dispatch overhead.

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

Termination: already-specialized functions are not re-specialized. Integrated into
the Residualize+Specialize pass. Generalized specialization (parameterizing by return
clause) is v2.


## Purity-Relative-to-Handler

When a computation's effect set doesn't intersect a handler's handled operations, the
handler's effect clauses are irrelevant. Only the return clause applies. If the return
clause is identity, the handler is eliminated entirely.

Check: intersect computation's effect row with handler's operation set. If empty, skip
all handler machinery.


## ComptimeReadFiles

`ComptimeReadFiles` is allowed during CT evaluation. When exercised, normalized paths
and content hashes are recorded as dependencies on staging results. Content-hash changes
trigger staging re-run. Users can extend the CT-allowed effect set for custom deterministic
CT-only effects.


## Three-Tier Persistability

Values crossing CT→RT boundary are classified:

- **Trivial** (Int, Bool, Char, Float, String, small enums): inline as literal, cheap to duplicate
- **Serializable** (structs/arrays/ADTs of persistable fields): constant table for large or
  multiply-used values (dedup + size caps)
- **Non-persistable** (closures over RT values, handles, capabilities): cannot cross, error at boundary


## CT-Only Functions

Functions using CT-only effects or taking TypeInfo arguments are CT-only. Calling with RT
arguments is a hard error, not a staging suggestion. Inferrable from effect usage or
explicitly declarable.


## Usage Analysis Infrastructure

Every binding is classified as Never (dead), Once (always inline), Many (inline if small),
or Recursive (never inline, needs fuel for CT). Computed via reachability from program
entry points.

Feeds into:
- Dead code elimination (drop Never bindings)
- Inlining heuristics (inline Once always, Many if body ≤ threshold)
- CT evaluation (Recursive functions get fuel-limited evaluation)
- Handler specialization (detect static arguments in recursive calls)
- Normalizer (all shrinking/speculative decisions gated on usage)


## Normalizer Reduction Rules

Post-residualization cleanup. Separated into shrinking (always safe) and speculative
(may grow code).

### Shrinking Reductions (Phase A)

Beta-reduction of once-used functions. Dead binding elimination. Val-return commutation.
Val-val flattening. Constant branch elimination. Known match elimination.

### Speculative Inlining (Phase B)

Inline Many-used functions below size threshold. Usage-gated. Runs once.

### Val Commutation for Join Points

```
val x = if (cond) thn else els; body
→ def k(x) = body; if (cond) { val x1 = thn; k(x1) } else { val x2 = els; k(x2) }
```

These fire after handler specialization removes dispatch overhead, exposing further
simplification opportunities.


## Scope Escape as Unified Check

The same "free variables of a type" algorithm serves three purposes:
1. Handler result types must not mention handler capabilities
2. Region results must not mention region captures
3. Cross-stage values must not mention CT-only resources (persistability check)

Single implementation, three call sites with different error messages.


## ASCAPE: As Comptime As Possible

The compiler automatically determines what can be compile-time and what must be runtime.
This is not annotation-driven: the user writes code, and the compiler maximizes CT
evaluation.

Key properties:
- **Transparent**: `--staging-report` shows exactly what was CT and what wasn't
- **Explainable**: every RT classification carries a provenance reason
- **Actionable**: suggestions tell the user what to change for more CT evaluation
- **Escapable**: `@runtime` forces RT when CT evaluation is unwanted

No multi-stage needed: two stages (CT/RT) confirmed sufficient by MetaOCaml experience.
Third stage historically used only to guarantee inlining, which monomorphization handles.


## Target-Aware CT Evaluation

The evaluator simulates target semantics, not host:
- Integer arithmetic uses target-width wrapping
- Byte reinterpretation uses target endianness
- CT cache keyed by target spec

Current caveat: floating point uses host behavior with tracking counter.
Non-finite float inputs/results (NaN/Inf) left unresolved.

Target query builtins available and fold immediately from TargetSpec:
- `target_word_size_bits() -> Int`
- `target_pointer_alignment() -> Int`
- `target_is_big_endian() -> Bool`


## Rust-Style Error Handling

Result, Option, `?` operator. Errors represented as types in v1. Errors as effects
is under consideration for future versions.