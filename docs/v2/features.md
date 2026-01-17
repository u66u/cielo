# V2 Features

This document describes features deferred from v1. It builds on `docs/v1/Features.md`
and `docs/v1/Decisions.md` — only differences and additions are listed here.

---

## Row Polymorphism

v1 uses explicit effect parameters. v2 adds row polymorphism for effect rows, enabling:

```cielo
fn map_with_effects[T, U, E](list: List[T], f: T -> U with E) -> List[U] with E {
    match list {
        | Nil => Nil
        | Cons(head, tail) => Cons(f(head), map_with_effects(tail, f))
    }
}
```

The effect variable `E` is universally quantified. The caller's effect row is threaded
through without explicit enumeration.

### Impact on staging

Row polymorphism interacts with staging: effect rows must be concrete before the
Evaluate+Classify pass runs. Monomorphization resolves all row variables before
staging analysis. No change to the staging passes themselves — they already require
concrete effect rows.

---

## Handler Fusion

When multiple handlers are syntactically nested and handle disjoint effects with
statically known implementations, compose them into a single fused handler that
threads all state simultaneously. Eliminates intermediate dispatch.

```cielo
// Before fusion (two handler installations, two dispatch points per operation):
with state_handler(0) {
    with reader_handler(config) {
        body_using_state_and_reader()
    }
}

// After fusion (single composite handler, one dispatch point):
with fused_state_reader_handler(0, config) {
    body_using_state_and_reader()
}
```

Empirically yields 4-24x speedups on handler-heavy code (search transformers,
constraint programming).

### Prerequisites

- Row polymorphism (to express fused handler types)
- Disjointness check on effect sets (already available via `SortedEffectRow::subtract`)

### Position in pipeline

Between Residualize+Specialize and Normalize. Or integrated into Residualize+Specialize
as an extension of the handler specialization logic.

[Research ref: Trifanov et al. — "Staging Effect Handlers for Modular Search" (PEPM 2026).
Validates staging + handler specialization as complementary. Shows handler composition
can be staged away.]

---

## Generalized Handler Specialization

v1 handler specialization handles direct wrappers and simple forwarding bodies. v2
extends to cases where the return clause varies across recursive call sites.

When the return clause varies, parameterize the specialized function by the return
clause (pass as continuation argument). This is selective CPS applied to specialization.

```cielo
// Problem case: recursive function where each call wraps the result differently
fn build_list(n: Int) -> List[Int] with State[Int] {
    if n == 0 { Nil }
    else {
        let x = State.get()
        State.set(x + 1)
        Cons(x, build_list(n - 1))  // return clause wraps result in Cons
    }
}

// v2 specialization: parameterize by return continuation
fn build_list_specialized(n: Int, state: Int, k: List[Int] -> R) -> R {
    if n == 0 { k(Nil) }
    else {
        let x = state
        build_list_specialized(n - 1, x + 1, |rest| k(Cons(x, rest)))
    }
}
```

### When v1 aborts

v1 aborts specialization when it detects varying return clauses across recursive call
sites (the With-Do case). v2 adds the return-clause parameter.

---

## Totality Checking

Totality checking for CT evaluation. If a function can be proven to terminate, it
can be CT-evaluated without fuel, with guaranteed success.

### Approach

Sized types or structural recursion checking: the compiler verifies that recursive
calls are on structurally smaller arguments.

```cielo
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
// Compiler: can't prove termination
// Falls back to fuel-based comptime evaluation
```

### User experience

Totality is inferred and reported, not annotated:

```
info: `length` verified as total — comptime evaluation guaranteed
info: `ackermann` not verified as total — comptime evaluation uses fuel limit (1000000 steps)
```

### Impact on staging

Total functions get guaranteed staging (no fuel exhaustion possible). Non-total
functions continue to use fuel-limited evaluation. The staging report distinguishes
"CT (total)" from "CT (within fuel)".

[Research ref: Hughes, Pareto, Sabry — sized types. Abel — sized types in Agda.]

---

## Incremental Staging Diagnostics

When recompiling, report expressions whose stage changed (CT→RT or RT→CT) with
explanation of what caused the change. IDE/LSP feature.

```
[staging-diff] 3 expressions changed stage:
  CT→RT: `timeout_ms` at config.co:15 (reason: config.toml changed, new field is non-integer)
  RT→CT: `max_retries` at client.co:42 (reason: now loaded from @comptime block)
  RT→CT: `retry_delay` at client.co:43 (same reason)
```

### Implementation

Diff previous and current StagingTables. Match expressions by span+kind+structural
fingerprint (not by ExprId, which may change between compilations). Report changes
with reasons derived from the Outcome's Reason enum.

---

## Background @comptime Computation

`@comptime` blocks can be isolated and computed in the background at build time.
While computing, they are handled by a runtime fallback. Once finished, the computed
data replaces the runtime path.

Opt-in to avoid confusion:

```cielo
@comptime(background: true) {
    expensive_table_generation()
}
```

### Semantics

- First compilation: `@comptime(background)` block starts evaluating in a background thread.
  The runtime code path is emitted as if the block were `@runtime`.
- When evaluation completes: result is cached. Next compilation uses the cached result as
  a normal `@comptime` block.
- If source changes: cached result invalidated, background evaluation restarts.

### Impact on staging

Background comptime blocks are classified as Stuck during their first compilation
(they have a runtime fallback). On subsequent compilations, they are Known (cached).
The staging report shows this:

```
[staging] background_table at data.co:20: computing (will be CT next build)
```

---

## Effect Composition

Compose effects from smaller pieces:

```cielo
effect RWState[S] = State.get[S] + State.set[S]
effect ReadOnly[S] = State.get[S]
```

### Open questions

- Subtyping between composed and individual effects
- How composition interacts with handler dispatch (does a handler for `RWState`
  handle `ReadOnly` operations?)
- How composition interacts with row polymorphism

---

## Errors as Effects

Explore whether `Result<T, E>` and the `?` operator can be unified with effect
handlers:

```cielo
effect Raise[E] {
    fn raise(e: E) -> Nothing
}

// ? operator desugars to:
// match expr { Ok(v) => v, Err(e) => do Raise.raise(e) }

fn parse_config(path: String) -> Config with Raise[ParseError] {
    let contents = read_file(path)?   // Raise on Err
    let tokens = tokenize(contents)?
    parse_tokens(tokens)
}

// Handler converts to Result:
let config: Result[Config, ParseError] = handle parse_config("x.toml") with Raise[ParseError] {
    | raise(e, _resume) => Err(e)
    | return(v) => Ok(v)
}
```

### Open questions

- Performance: can the handler overhead be fully eliminated for the common case?
  (Handler specialization + discharge should handle this.)
- Ergonomics: is `with Raise[E]` in function signatures acceptable, or too verbose
  compared to `-> Result<T, E>`?
- For v1, errors as types (Result, Option, ?) is the pragmatic choice.

---

## Lazy Evaluation Primitive (`need`)

From ECBPV (Extended Call-by-Push-Value): `M need x. N` binds a computation variable
that is evaluated at most once on first use.

### Potential uses

- Internal IR primitive for representing sharing decisions after inlining/ANF
- Language-level memoization with equational laws
- CT evaluation cache could be formalized as `need` bindings

### Caveats

- Mixing evaluation orders breaks associativity (eager `to` + lazy `need` don't compose freely)
- Effect system must handle the "first use has effects, subsequent uses are pure" pattern
  (solvable with pointed effect algebras where `Pure ≤ E` for all `E`)

[Research ref: ECBPV thesis Section 5.1, Figure 5.2 pp.106-107]

---

## Modal Effect Types

Separate effects and functions with modalities. From "Rows and capabilities as modal effects":

- **Abs** modality: "this is data-like; using it doesn't depend on the surrounding effect environment"
- Maps to: persistability check (Abs ≈ persistable)

Full modal type system deferred due to implementation complexity. v1 uses the insight
(Abs ↔ persistable) without the full type-level machinery.

---

## Rank-2 Effect Polymorphism

Can hide internal handler effects while keeping external APIs "pure":

```cielo
fn with_fresh_state[A](body: forall E. () -> A with State[Int] + E) -> A {
    handle body() with State[Int] { ... }
}
// External type: with_fresh_state : (forall E. () -> A with State[Int] + E) -> A
// No State[Int] in the external signature
```

Matches staging goals: internal handler effects are discharged, external API is pure.
Implementation requires rank-2 types in the type checker.

---

## V2 Carry-Over Notes (from v1 plan)

- Generalized handler specialization by return-clause parameterization is deferred to v2
  selective CPS.
- If mutable-variable stmt forms are added to Core IR, tail-resumption gating must mirror
  the mutable-state caveat from `RemoveTailResumptions`.
- Persistability/codegen expansion still pending beyond v1 scalar/string pooling:
  - recursive aggregate pooling for nested ADT constants (top-level ctor pooling is v1)
  - non-finite float pooling policy (NaN/Inf handling if adopted)
- Full query architecture replacing function-level memoization (Salsa-style).
- Partially-static data (Partial variant in Outcome enum).
- Capability-to-region unification for handler evidence allocation.
- Flow-sensitive mutable state tracking in evaluator.