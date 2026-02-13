# Comptime Implementation Plan

## v2 Roadmap (Deferred, Not Active)

Priority order:

1. query architecture migration (salsa-style dependency tracking)
2. partially-static data (`Outcome::Partial`)
3. flow-sensitive mutable-state staging
4. per-clause partial discharge
5. totality checker for fuel bypass
6. background comptime execution model

### V2-Stage 1: Query architecture

Deliverable:

- query-db backbone for staging queries and invalidation

Primary references:

- Salsa docs + repo

### V2-Stage 2: Partial values

Deliverable:

- mixed known/stuck data propagation across constructors and field reads

Primary references:

- Yallop/von Glehn/Kammar 2018

### V2-Stage 3: Flow-sensitive mutable staging

Deliverable:

- program-point-sensitive variable outcome tracking with join semantics

Primary references:

- `docs/v2/comptime_additions.md`

### V2-Stage 4: Per-clause conditional discharge

Deliverable:

- conditional discharge status keyed by argument patterns

Primary references:

- Effekt selective control/residualization patterns

### V2-Stage 5: Totality + fuel bypass

Deliverable:

- proven-total functions bypass fuel limits

Primary references:

- sized-types literature listed in `docs/effects-impl.md`

### V2-Stage 6: Background comptime

Deliverable:

- async comptime blocks with cached promotion from RT fallback to CT

Primary references:

- query/invalidation model from V2-Stage 1

---

## Oracle Cookbook

For each stage change:

1. run reference staging evaluator (split modules are current oracle)
2. run fused stage entrypoint
3. compare normalized residual outputs (and emitted C where appropriate)
4. run metamorphic perturbations:
   - alpha renaming
   - dead-code insertion (discardable-only)
   - equivalent control-flow reshaping

---

## References

Staging and partial evaluation:

1. Jones, Gomard, Sestoft. *Partial Evaluation and Automatic Program Generation*.
   https://www.itu.dk/~sestoft/pebook/pebook.html
2. Kovacs. *Staged Compilation with Two-Level Type Theory* (ICFP 2022).
   https://doi.org/10.1145/3547641
3. Kovacs. *Closure-Free Functional Programming in a Two-Level Type Theory* (ICFP 2024).
   https://doi.org/10.1145/3674648

Normalization:

4. Appel, Jim. *Shrinking Lambda Expressions in Linear Time*.
   https://doi.org/10.1017/S0956796897002839

Query/incrementality:

5. Salsa overview.
   https://salsa-rs.github.io/salsa/how_salsa_works.html
6. Salsa repository.
   https://github.com/salsa-rs/salsa

Partially-static data:

7. Yallop, von Glehn, Kammar. *Partially-static data as free extension of algebras*.
   https://doi.org/10.1145/3236795

Implementation references:

8. `docs/v1/comptime_passes.md`
9. `docs/v1/implementation_ref.md`
10. `docs/v2/comptime_additions.md`
