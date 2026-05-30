## Observational equivalence across stages

Even if something is "pure", evaluating it at CT vs RT might produce different observable
results unless semantics is nailed down:

    floating point differences (host compiler vs target runtime, different rounding modes,
    SIMD, NaN behavior)
    "undefined behavior" or implementation-defined things
    pointer/address identity (ptr_to_int, hashing addresses)
    concurrency scheduling / nondeterminism

If your language defines these tightly, you can CT-evaluate more safely. If not, you must
restrict CT evaluation.

### v1 mitigations

- Integer arithmetic: an operation that overflows `i64`, divides by zero, or is
  `MIN / -1` is not folded at all — it residualizes and the C runtime traps on it.
  `MIN % -1` folds to `0`, matching `cv_mod`. Results that fit `i64` are then
  narrowed to the target word width, as are integer literals.
- Float arithmetic: host behavior with `folded_float_host` tracking counter. Non-finite
  inputs/results (NaN/Inf) are left unresolved — the evaluator produces `Stuck` for
  expressions that would produce NaN/Inf, preventing cross-target divergence.
  Float `%` is not folded and not accepted by the typechecker: `%` is Int-only, and
  `cv_mod` traps on a float operand.
- Pointer/address identity: not observable in Cielo v1 (no ptr_to_int, no address hashing).
- Concurrency: runtime-only effect. Cannot appear in CT evaluation.

### CT cache keying

CT cache entries are keyed by `{target spec, evaluator policy, compiler version}`. This
ensures that CT results computed for one target are not reused for another, even if the
source is identical. Changing the evaluator policy (e.g., wrapping behavior for a new
target width) invalidates the entire cache.


## Float literal forms

`.` is overloaded: it is a float point, a field projection (`p.a`), and the separator in
`do Effect.op`. The lexer resolves this by only reading `.` as a float point when a digit
follows it, which makes these the accepted forms:

| Form | Example | Status |
| --- | --- | --- |
| digits `.` digits | `1.5` | accepted |
| digits with exponent | `1e9`, `2E+2` | accepted, and always a `Float`, never an `Int` |
| both | `1.5e-3` | accepted |
| trailing point | `1.` | rejected, `LEX_BAD_NUMBER` |
| digits then a name | `1.foo`, `1e` | rejected, `LEX_BAD_NUMBER` |
| leading point | `.5` | rejected — see below |
| overflowing literal | `1e400` | rejected, `LEX_BAD_FLOAT` |

`1.foo` is an error rather than `1` `.` `foo` because an integer has no fields, so no
reading of it is valid; splitting it into three tokens only moves the complaint somewhere
that does not name the mistake. The same argument covers `1.`.

`.5` is not rejected by any rule of its own — a leading `.` is consumed as a field access
on whatever preceded it long before a number is read, so `x .5` is `x.5` with a malformed
field name. Writing `0.5` is the only spelling.

An out-of-range literal is rejected rather than saturated to infinity, because every stage
below treats a non-finite float as unfoldable and unpoolable — a mistyped exponent would
otherwise become a value the compiler quietly refuses to reason about.

Floats do not convert to an exit code. A `main` returning `Float` exits `0`; a program
that wants to observe a float has to reduce it to `Int` itself.

### NaN is not ordered

`cv_ordering` returns `CV_UNORDERED` for a NaN operand, and all four of `<`, `<=`, `>`,
`>=` are false on it. A three-way comparison that folded unordered onto "equal" would make
`nan <= x` and `nan >= x` both true. `cv_equal` is plain `==`, so `nan == nan` is false and
`-0.0 == 0.0` is true — both correct IEEE, both surprising.


## Char literal forms

A `Char` is one Unicode scalar value, not one byte. `Literal::Char` is a Rust `char`,
`cv_char` takes a `uint32_t`, and codegen emits the code point, so the width was already
settled everywhere except the lexer and `cv_print`; the literal follows the representation
rather than narrowing it to ASCII.

| Form | Example | Status |
| --- | --- | --- |
| one character | `'a'`, `'é'`, `'字'` | accepted |
| escape | `'\n'`, `'\t'`, `'\r'`, `'\\'`, `'\''`, `'\0'` | accepted — this is the whole set |
| empty | `''` | rejected, `LEX_EMPTY_CHAR` |
| more than one character | `'ab'`, `'\n\t'` | rejected, `LEX_MULTI_CHAR` |
| any other escape | `'\q'`, `'\u{41}'` | rejected, `LEX_BAD_ESCAPE` |
| no closing quote | `'a` | rejected, `LEX_UNTERMINATED_CHAR` |

`'"'` needs no escape, because a character literal is quoted with `'`. An unknown escape is
an error rather than the escaped byte, so a typo cannot quietly become a different
character.

`cv_print` encodes UTF-8 rather than using `%c`, which truncates everything above U+007F to
one byte of a sequence. `Char` is otherwise unconnected to `String` in v1: there is no
`ord`, `chr` or string indexing, and the builtins are `print`, `str_len` and `str_concat`.

### Chars order, but do not do arithmetic

`'a' < 'b'` typechecks. `cv_ordering` compares `CV_CHAR` by scalar value and `ct_common`
folds `(Comparison, Char, Char)` identically, so both stages agree. Equality is the same
story via `cv_equal`.

Arithmetic stays `Int`/`Float` only — `'a' + 'b'` is `TYPE_NUMERIC_REQUIRED` — and so does
a mixed pair like `'a' < 1`.

`String` is deliberately not ordered even though `cv_ordering` runs `strcmp` on it, because
`ct_common` has no `(Comparison, Str, Str)` arm. Allowing it would mean a comparison the
runtime answers and comptime declines, which is the same asymmetry the `%`-on-floats rule
exists to prevent.


## Effect handler bugs

### Example: effect handled by the "wrong" handler

This bug is about **reusing the same effect label for two different purposes**, and then
calling a callback inside a handler.

The shape of the bug:

You have a function that uses an effect internally as an implementation trick (early return,
search, etc). It installs a handler for that effect. Inside that handler, it calls a user
callback. If the user callback also uses the same effect label, your internal handler will
accidentally intercept it.

Concrete example (early-exit using `Yield`)

```cielo
effect YieldInt { yield(x: Int): Unit }

// implementation trick: return first matching element by "yielding" it
fn find_first(pred: Int -> Bool, xs: List[Int]) -> Option[Int] {
  with handler collect {
    return (_) => None
    YieldInt.yield(x, _resume) => Some(x)   // stop at first yield
  } {
    for x in xs {
      if pred(x) {
        do YieldInt.yield(x)
      }
    }
    None
  }
}
```

Now user code:

```cielo
fn pred(x: Int) -> Bool {
  if x == 7 {
    do YieldInt.yield(999)  // user meant "log/trace", not "return from find_first"
  }
  x % 2 == 0
}

find_first(pred, [1,7,10])
```

**What happens (bug):**

- `pred` runs inside `find_first`'s handler.
- `pred` does `YieldInt.yield(999)`.
- The nearest handler for `YieldInt` is `collect`, so it returns `Some(999)`.
- Totally wrong result.

"But we have a specific handler for a specific effect—how can it be wrong?"

It's "wrong" because **the effect label collided**:

- `find_first` wanted `YieldInt` as an internal control-flow tool.
- the user used the same `YieldInt` for their own meaning.
- dynamic dispatch says: nearest handler wins.

This can happen even if your effect system is fully explicit, and even if there's no row
polymorphism at all.

**Solution: Fresh instances / local labels (lexical handlers / capabilities)**

Instead of one global `YieldInt`, generate a fresh "instance" for the internal use so it
can't collide. This is exactly what lexical handlers / capability instances are good at.

### Implementation in Cielo

Each `with handler` creates a fresh capability ID. Effect operations resolve to the nearest
capability in the lexical scope chain. Two handlers for the same effect type get different
capabilities — they cannot be confused.

The capability stack is used by:
1. **Type+effect checker:** to resolve which handler an effect operation targets.
2. **Evaluate+Classify:** to determine handler discharge status (evaluator carries its own
   capability stack mirroring the type checker's scoping).
3. **Linearize:** to correctly lower handler installations with per-handler identity.

### Testing this invariant

The "YieldInt" example above is a required test case. The test must verify that:
- `find_first(pred, [1,7,10])` produces `Some(10)` (the first even number), not `Some(999)`.
- The inner `YieldInt.yield(999)` in `pred` targets a different handler (or produces an
  error if no handler is in scope for the user's `YieldInt` capability).


## Fuel and size exhaustion

CT evaluation is bounded by fuel (steps) and size (allocations). When either limit is
reached, the evaluator produces `Stuck(FuelExhausted)` or `Stuck(SizeExhausted)` for
that expression and continues classifying the rest of the program.

### Pitfalls

- **Fuel exhaustion mid-function:** A function that exhausts fuel partway through produces
  Stuck for its result, but the evaluator must not leave the environment in an inconsistent
  state. The environment is restored to its pre-call state when fuel runs out.

- **Fuel exhaustion in one branch:** `if ct_cond { expensive_ct() } else { cheap_ct() }`
  where `expensive_ct` exhausts fuel — only the expensive branch is Stuck. If the condition
  selects the cheap branch, the overall expression is Known.

- **Size exhaustion in CT allocation:** When Alloc exhausts the size budget, the allocation
  and all dependent expressions become Stuck. But earlier allocations that completed
  successfully remain Known.

- **Determinism across machines:** Fuel limits must be deterministic (step count, not wall
  time). The same program with the same fuel produces the same staging decisions on any
  machine.

### User control

Users can set fuel/size per `@comptime` block:
```cielo
@comptime(fuel: 1_000_000, size: 10_mb) {
    expensive_table_generation()
}
```

Default fuel and size are compiler-wide configuration.


## Single-shot resumption constraint

v1 supports single-shot resumptions only. The continuation captured by a handler clause
may be resumed at most once. Resuming twice is diagnosed as `LINEARIZE_MULTI_SHOT_RESUME`.

### Where this matters

- **Search/backtracking handlers:** Nondeterminism (Amb) wants multi-shot to explore
  multiple branches. In v1, this must be encoded differently (CPS, explicit worklist).
- **Handler clause analysis:** Resume use count is tracked per clause via path-sensitive
  analysis. Branch-exclusive single resumes (one resume per branch of an if/match) are
  accepted. Same-path double resumes are diagnosed.

### Implementation

`clause_resume_use_bound` in `linearize.rs` computes a conservative upper bound on how
many times a clause resumes:
- Sequential statements: `plus` (both execute, so uses add)
- Branching statements: `max` (only one branch executes, so take the worst case)
- Cycles: conservatively `Many`

If the bound is `Many`, a diagnostic is emitted. The handler clause is still lowered
(as ControlCall) but correctness is not guaranteed for the multi-shot path.


## Handler discharge with RT value passthrough

A handler can be dischargeable (all captures Known, clause bodies CT-eligible) while the
overall handled expression remains RT. This is correct and expected.

Example:
```cielo
let initial_state = 0                   // Known
let rt_input = get_user_input()         // Stuck (IO)

with state_handler(initial_state) {
    // state operations are CT (handler is dischargeable)
    // but rt_input flows through as RT value
    let x = State.get()                 // CT: handler evaluates this
    let y = process(rt_input)           // RT: depends on rt_input
    State.set(x + 1)                    // CT: handler evaluates this
    y                                   // RT: overall result is RT
}
```

After residualization:
- Handler construct is erased (discharged)
- State.get() / State.set() calls are eliminated (evaluated at CT)
- `process(rt_input)` remains as residual code
- The `x + 1` in `State.set(x + 1)` is folded to a constant

This is the "handler discharge is independent of value staging" principle.


## @comptime block restrictions

`@comptime` blocks are the most restricted evaluation context. Everything that enters
must be Known + persistable. Everything that leaves must be persistable.

### What can go wrong

- **Non-persistable capture:** `@comptime { use(file_handle) }` — file_handle is an OS
  resource. Even if it were somehow Known, its type is non-persistable.

- **Effect leakage:** `@comptime { do IO.print("hello") }` — IO is not in the CT-allowed
  effect set. Hard error.

- **Non-persistable result:** `@comptime { create_closure_over_rt() }` — if the closure
  captures RT values, it's non-persistable and can't cross back to the surrounding context.

- **Indirect non-persistability:** `@comptime { Struct(1, file_handle) }` — the struct
  contains a non-persistable field. The struct itself is non-persistable.

### Diagnostic quality

Error messages for `@comptime` violations should include:
- Which variable is problematic
- Why it's RT (provenance chain)
- Why its type is non-persistable (which field/component is the culprit)
- What the user could do instead

Example:
```
error: cannot use `conn` inside @comptime block
  --> src/main.co:15:5
   |
12 | let conn = Database.connect(url)
   |     ---- `conn` is runtime because `Database.connect` performs IO
   |
15 |     @comptime { precompute(conn) }
   |                            ^^^^ `conn` has type `Connection` which is not persistable
   |                                  because `Connection` contains an OS resource
   |
   = help: consider loading the data at build time with `ComptimeReadFiles`
```


## Val-val flattening with variable shadowing

The normalizer's val-val flattening rule:
```
val x = { val y = s1; s2 }; s3
→ val y = s1; val x = s2; s3
```

This is unsound if `y` is free in `s3` (the inner `y` would shadow the outer one after
flattening). The normalizer must check that `y` is not free in `s3` before applying this
rule, or alpha-rename `y` to a fresh variable.

This is a classic capture-avoidance issue. The same applies to any flattening/inlining
transformation in the normalizer.


## Target-width integer semantics

The CT evaluator computes in `i64` and then narrows the result to the target word
size. The two steps fail differently: exceeding `i64` is a fault the runtime traps
on, so the evaluator declines to fold rather than inventing an answer the compiled
program would never produce; narrowing to a smaller target word is ordinary
wrapping. This means:

- `i64::MAX + 1` does not fold on any target — it residualizes, and the emitted C
  traps with `integer addition overflow`
- `2147483647 + 1` on a 64-bit target folds to `2147483648`; on a 32-bit target it
  folds to `i32::MIN` (as i64 representation)
- `i64::MIN / -1` does not fold on any target — `cv_div` traps on it

All integer literals are normalized to the target word width during CT folding. A literal
`0xFFFFFFFF` on a 32-bit target is `-1` (i32), not `4294967295` (u32 interpreted as i64).

If the target word size changes between compilations, the entire CT cache is invalidated
(cache key includes target spec).