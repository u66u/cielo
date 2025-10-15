## Observational equivalence across stages

Even if something is “pure”, evaluating it at CT vs RT might produce different observable results unless semantics is nailed down:

    floating point differences (host compiler vs target runtime, different rounding modes, SIMD, NaN behavior)
    “undefined behavior” or implementation-defined things
    pointer/address identity (ptr_to_int, hashing addresses)
    concurrency scheduling / nondeterminism

If your language defines these tightly, you can CT-evaluate more safely. If not, you must restrict CT evaluation.

## Effect handler bugs

### Example: effect handled by the “wrong” handler (and yes, it can happen without row polymorphism)

This bug is not primarily about row polymorphism. It’s about **reusing the same effect label for two different purposes**, and then calling a callback inside a handler.

The shape of the bug:

You have a function that uses an effect internally as an implementation trick (early return, search, etc). It installs a handler for that effect. Inside that handler, it calls a user callback. If the user callback also uses the same effect label, your internal handler will accidentally intercept it.

Concrete example (early-exit using `Yield`)

cielo

```
effect YieldInt { yield(x: Int): Unit }

# implementation trick: return first matching element by "yielding" it
fn find_first(pred: Int -> Bool, xs: List[Int]) -> Option[Int] {
  with handler collect {
    return (_) => None
    YieldInt.yield(x, _resume) => Some(x)   # stop at first yield
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

cielo

```
fn pred(x: Int) -> Bool {
  if x == 7 {
    do YieldInt.yield(999)  # user meant "log/trace", not "return from find_first"
  }
  x % 2 == 0
}

find_first(pred, [1,7,10])
```

**What happens (bug):**

- `pred` runs inside `find_first`’s handler.
- `pred` does `YieldInt.yield(999)`.
- The nearest handler for `YieldInt` is `collect`, so it returns `Some(999)`.
- Totally wrong result.

“But we have a specific handler for a specific effect—how can it be wrong?”

It’s “wrong” because **the effect label collided**:

- `find_first` wanted `YieldInt` as an internal control-flow tool.
- the user used the same `YieldInt` for their own meaning.
- dynamic dispatch says: nearest handler wins.

This can happen even if your effect system is fully explicit, and even if there’s no row polymorphism at all.

Solution: **Fresh instances / local labels (lexical handlers / capabilities)**  
Instead of one global `YieldInt`, generate a fresh “instance” for the internal use so it can’t collide.  
This is exactly what lexical handlers / capability instances are good at.
Moreover, there's a paper about implementing rows and capabilities as modal effects. We don't want full modal effects, but we can make “fresh instances” a first-class internal concept in our compiler/IR (even if surface syntax hides it).
