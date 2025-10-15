# V1

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

Why this matters for staging: The hierarchy gives the compiler coarser-grained but faster staging decisions. Anything below IO in the hierarchy is a comptime candidate. Anything at IO or above is runtime. The fine-grained analysis (looking at handler values) only needs to happen for the middle layers.

This also gives users an intuitive mental model: "IO and above = runtime, everything else = compiler figures it out."
```

## Rules for automatic staging

The compiler needs decidable rules. Here's a candidate set:

Level 0 — Always comptime:

    Literal expressions
    Pure functions applied to comptime-known arguments
    Type-level computation

Level 1 — Comptime if handler is comptime:

    Effectful computation where all effects are handled by handlers with comptime-known arguments. After handler application, if no effects remain and no runtime values are needed → comptime.

Level 2 — Runtime:

    Anything with unhandled effects (IO, FFI, system calls)
    Anything depending on values only known at runtime
    Anything the user explicitly marks as runtime (for config-dependent behavior, etc.)

The critical rule: A computation's stage is determined by the residual effects after all enclosing handlers are applied, plus the binding times of its free variables.

Diagnostics: measure comptime cost of each fn or other unit of computation and report to user, allow them to set fuel (budget) for memory, time, iterations, etc


## Using effects to derive comtpime vs runtime

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
handler db_mock {
    query(sql) => resume(static_test_data)   // comptime!
}

// Runtime handler (actual DB connection):
handler db_real(conn: Connection) {
    query(sql) => resume(conn.execute(sql))   // runtime (conn is runtime, execute has IO)
}

The rule: After all handlers are applied, look at the residual effects. If none remain (or only eliminable ones with comptime handlers remain), and all free variables are comptime, the computation is comptime.
```


## Shared State vs. Local State Rewrites

The thesis mentions rewrites that are valid for Local state but invalid for Shared state. Here they are:

**Rewrite 1: Read-after-Write (Forwarding)**

Rust

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

Rust

```
// Code
x := 1;
x := 2;

// Optimization
x := 2; // Valid for Local. INVALID for Shared.
```

_Why Invalid for Shared?_ Another thread might be observing the transition state where `x` was 1.

**Rewrite 3: Read-after-Read (CSE)**

Rust

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
    

Rust

1. ```
    if effect == LocalState {
        enable_dead_store_elimination();
        enable_constant_propagation();
    }
    if effect == AtomicState {
        disable_optimizations(); // Treat as IO/Volatile
    }
    ```
## Equality (`==`) vs. Preorder (⊑⊑) Semantics

This is the most important architectural takeaway from the thesis.

**Equality Semantics (A=BA=B):**  
"I can replace code A with code B **and vice versa**, and the program does _exactly_ the same thing."

- _Example:_ `2 + 2` can be replaced with `4`. `4` can be replaced with `2 + 2`.

**Preorder Semantics (A⊑BA⊑B):**  
"I can replace code A with code B because B is **better** (or more defined) than A."

- _Example:_ `launch_missiles(); undefined_behavior()` ⊑⊑ `launch_missiles(); crash()`.
- _Example:_ `flip_coin()` (nondeterministic) ⊑⊑ `true` (deterministic).

**Why Koka (and most langs) use Equality by default:**  
They assume the program is deterministic and total (doesn't crash).
	
**Why you want Preorder for Optimization:**  
Compiler optimizations are often directional.

1. **Dead Code Elimination:** You remove `let x = pure_fn()`.
    - Is `let x = pure_fn()` _equal_ to `()`? Yes.
2. **Removing Nondeterminism:**
    - Replacing `rand()` with `4` is valid in one direction (refinement), but not the other.
    - If you rely on equality semantics, you technically _cannot_ optimize nondeterministic code or undefined behavior constant folding.

**Practical takeaway:** You don't need to code "preorders" into your compiler. You just need to mentally verify that every optimization pass moves "down" the preorder (making the code more specific/efficient) and never "up" (making it more vague/effectful).


## Effect capability ordering

Give effects a coarse ordering:

```
Pure ≤ Diverge ≤ Alloc ≤ LocalState ≤ IO ≤ FFI
LocalState ≤ SharedState?  (usually NO; SharedState is “more dangerous” than LocalState)
```

Then define:

- `capability(effect)` → one of those levels
- `capability(effect_row)` → the **max** level in the row (or “top” if unknown)

Now you can formalize lots of compiler decisions:

- **staging**: comptime allowed iff `capability(effect_row) < IO` (plus other checks)
- **code motion**: allowed iff `is_thunkable(effect_row)` (which can be derived from the level + flags)
- **optimizations**: “this rewrite is allowed if the new code’s capability is ≤ old code’s capability”

This doesn’t require fancy types. It’s just metadata + a compare function.


## Effect-Qualified Optimizations

**Do** tag your effect rows with properties (e.g., "Commutative", "Idempotent", "Discardable").

- **The Win:** The paper shows that `Dead Code Elimination` is valid if `M; N <= N`. You can implement this by checking if the effects in `M` satisfy the "Discardable" property (like reading state, but not writing it).

**Ambient effect contexts are a natural fit for “this region is comptime” vs “runtime” contexts.**

## Using modalities for `@comptime`
### Briefing

From paper "Rows and capabilities as modal effects" (2026)
Mental framework: **Entering `@comptime { ... }` is like switching to a restricted world**  
where only certain effects are allowed and only certain outside values are allowed in

Absolute/relative:
- **Abs** means: “this is _data-like_; using it doesn’t secretly depend on the surrounding effect environment.”
    - `Int` is Abs even if it’s runtime.
    - A closure is generally not Abs, even if it was created at comptime.

So for staging we need at least 2 checks:
- Can I know this value now? (your provenance/BTA)
- Even if I “know it”, is it a kind of thing that makes sense to embed / serialize / use in the comptime evaluator?

However, we will not implement full modal types calculus. Certainly not for v1 - we are just taking the most applicable insights.

### simple rule-set for `@comptime` using the modality mental model

Think of `@comptime` as entering a new “mode”:

- Allowed effects inside: `{Pure, Alloc?, LocalState?, ComptimeReadFiles?, ...}` (whatever you decide)
- Disallowed effects inside: `{IO, FFI, SharedState, Concurrency, ...}`

Then when code inside `@comptime` references an outer variable `x`, require:

Rule: Cross-stage persistence (Cielo version)

`x` is allowed inside `@comptime` iff:

1. `stage(x) == Comptime` (it’s actually known now)
2. `is_persistable(type(x)) == true` (it’s safe to carry into comptime)

`is_persistable` is your “Abs-like” predicate. It can be very simple:

- persistable: ints/bools/strings/arrays/structs of persistables
- not persistable: file handles, pointers to runtime memory, OS resources, thread handles, etc.
- functions/closures: only persistable if explicitly wrapped in a representation you control (e.g. `ComptimeFn` or `Code`), otherwise no

Example

cielo

```
let n = 5                  # comptime
let fh = open("x.txt")     # runtime handle (IO)

@comptime {
  n + 1        # OK
  use(fh)      # error: fh is runtime and not persistable
}
```

This is exactly the “lock prevents access unless it’s absolute/persistable” idea, translated into staging.

Why this helps:
- It replaces fuzzy heuristics with a crisp rule users can learn.
- It improves diagnostics: “this is runtime because it depends on `fh` (runtime handle).”
- It makes your comptime engine more reliable (no accidental runtime resources inside it).

### Usage Analysis Infrastructure

Every binding is classified as Never (dead), Once (always inline), Many (inline if small),
or Recursive (never inline, needs fuel for CT). Computed via reachability from program
entry points.

Feeds into:
- Dead code elimination (drop Never bindings)
- Inlining heuristics (inline Once always, Many if body ≤ threshold)
- CT evaluation (Recursive functions get fuel-limited evaluation)
- Handler specialization (detect static arguments in recursive calls)

### Normalizer Reduction Rules

Post-residualization cleanup. Beta-reduction with usage-aware inlining. Val commutation
rules that flatten nested bindings and introduce join points at branch/match boundaries.
Constant folding on branches and match scrutinees.

These fire after handler specialization removes dispatch overhead, exposing further
simplification.

### Scope Escape as Unified Check

The same "free variables of a type" algorithm serves three purposes:
1. Handler result types must not mention handler capabilities
2. Region results must not mention region captures
3. Cross-stage values must not mention CT-only resources (persistability check)

Single implementation, three call sites with different error messages.

# V2

## Totality Checking 

The termination problem is your biggest practical headache for comptime evaluation. Fuel limits work but are unsatisfying — they make staging unpredictable. The same code might stage on one machine (fast enough) but not another.

If you can prove a function terminates, you can stage it without fuel, with guaranteed success. You don't need full totality checking (that's Agda). You need:

Sized types or structural recursion checking: the compiler verifies that recursive calls are on structurally smaller arguments.

text

fn length(list: List[A]) -> Nat {
    match list {
        Nil => 0,
        Cons(_, tail) => 1 + length(tail)  
        // tail is structurally smaller than list ✓
    }
}
// Compiler: length is total → comptime evaluation guaranteed to terminate
// No fuel needed

fn ackermann(m: Nat, n: Nat) -> Nat { ... }
// Compiler: can't prove termination (it does terminate, but the argument is complex)
// Falls back to fuel-based comptime evaluation

Make totality a property the compiler infers and reports, not something the user must prove. Total functions get guaranteed staging. Non-total functions get fuel-limited staging. The user sees:

text

info: `length` verified as total — comptime evaluation guaranteed
info: `ackermann` not verified as total — comptime evaluation uses fuel limit (1000000 steps)

This is practical (no annotation burden), useful (most utility functions ARE structurally recursive), and connects to a deep body of research (sized types: Hughes, Pareto, Sabry; Abel's work on sized types in Agda).


## Provenance Tracking

A more refined staging analysis tracks not just "comptime vs runtime" but why something is runtime:


fn process(config: Config, request: Request) -> <i/o> Response {
    let timeout = config.timeout    // runtime because config is runtime
    let path = request.path         // runtime because request is runtime
    let default = 30                // comptime
    ...
}

If the compiler tracks provenance ("this is runtime because it depends on config"), it can tell the user:

info: `process` is runtime
  because: `config` is runtime (parameter)
  because: `request` is runtime (parameter)
hint: if `config` were comptime (e.g., loaded from a build-time file),
      `timeout` and dependent computations would be comptime



This helps the programmer make informed staging decisions. It also enables staging suggestions: "make X comptime and you gain Y."

This is essentially abstract interpretation with an explaining abstract domain — not just {comptime, runtime} but {comptime, runtime(reason)}. The partial evaluation community calls this "binding-time improvement" — restructuring code to make more of it static.


# Maybe some distant future (not planned for now)


## If you want laziness: `need` (call-by-need / sharing) is a small “once” primitive with big implications

### C1) ECBPV adds a single construct: `M need x. N` (Section 5.1)

This is the cleanest thing in the whole part 2 if you care about “compute once then reuse”:

- `need` binds a **computation variable** (not a value variable).
- The computation behind the variable is evaluated **at most once**, on first use, and memoized.

They give core axioms for `need` (Figure 5.2, p.106–107). Two especially important ones:

1. **Forcing-on-first-use law** (sequencing axiom):  
    `M need x. (x to y. N) ≡ M to z. N[x ↦ return z]` (modulo their exact syntax)  
    This is the “real meaning” of sharing: when you actually force it, it becomes a one-time eval.
    
2. **GC / dead binding law**:  
    `M need x. N ≡ N` if `x` not free in `N`
    

**Why this is relevant to your project**

- `need` is essentially a _language-level memoization/caching primitive with equational laws_.
- That’s conceptually very close to:
    - “CT evaluation caches results”,
    - incremental compilation caches queries,
    - “compiler as database” (but with semantics, not just convenience).

Even if you never expose `need` to users, it’s an excellent **internal IR primitive** for representing sharing decisions (especially after inlining/ANF, where sharing matters).

### C2) Mixing evaluation orders breaks associativity (important warning)

They explicitly note you _don’t_ get all associativity laws when mixing `to` (eager sequencing) and `need` (lazy/shared). Example they say does **not** hold:

- `(M to x. N) need y. P ≡ M to x. (N need y. P)` is not valid in general.

**What to steal**

- This is a general warning: as soon as you add “sharing”, “masking”, “transactions”, “async cancellation”, etc., you must be careful about which algebraic laws still hold.
- For effect handlers, the same phenomenon appears: handler order matters; “obvious refactorings” can change semantics.

### C3) The effect system problem for `need`: you really want flow-sensitivity, but you can dodge it

Section 5.4 is a very practical insight:

- In call-by-need, the _first_ use of a variable may have effects; subsequent uses are effect `1`.
- To be precise, you’d need an effect system that tracks whether a variable has been forced already (flow/usage-sensitive).

He proposes a cheap escape hatch: restrict effect algebras to be **pointed** (Definition 5.4.1): `1 ≤ e` for all `e`. Then over-approximating repeated uses as still having effect `e` is sound (it’s not an underestimate).

**Why you care**

- This exact trick is relevant to staging too:
    - if you don’t want complex flow-sensitive “this got computed at CT already” reasoning in your type/effect inference,
    - you can conservatively overapproximate, provided your effect ordering supports it.

**In your setting**

- Koka-style “set of effects” (`∅ ⊆ e`) is pointed, so you can use this idea immediately.

### C4) Kripke logical relations of varying arity (Section 5.3)

To prove equivalence of call-by-name and call-by-need under divergence-only effects, ordinary logical relations over closed terms aren’t enough because `need` creates “action at a distance”.

They use Kripke relations indexed by computation-variable contexts and enforce “closed under divergence”.

**What to steal**

- This is mostly theory, but the meta-lesson is practical:
    - once you have sharing/memoization, proving/program reasoning needs stronger invariants than “local step semantics”.
- If you later try to give a semantics for “CT caching” or “incremental compilation memoization” with effects, this is the kind of technique that shows up.
