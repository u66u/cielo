# Cielo database and crate split

This is the migration plan for branch `009-db`.  The first pass keeps the
existing compiler behavior intact while making the ownership boundaries
explicit.  The goal is to make new memory managers and analysis variants easy
to add without turning the driver or a shared phase struct into the owner of
everything.

## Dependency direction

```text
cielo-backend-api -> (no Cielo dependencies)
cielo-frontend    -> cielo-base
cielo-ir          -> cielo-base
cielo-lowering    -> cielo-base + cielo-frontend + cielo-ir
cielo-sema        -> cielo-base + cielo-ir
cielo-staging    -> cielo-base + cielo-frontend + cielo-ir + cielo-sema
cielo-memory      -> cielo-base + cielo-ir + cielo-staging + cielo-backend-api
cielo-backend-c   -> cielo-backend-api + cielo-base + cielo-ir + cielo-staging
cielo-db          -> frontend + lowering + sema + memory + backend-api
cielo-compiler    -> database + feature crates (temporary integration layer)
cielo-driver      -> compiler
```

An arrow points from a crate to a dependency.  Cargo verifies that this graph
is acyclic.

- `cielo-base`: IDs, spans, diagnostics, symbols, and small containers.
- `cielo-ir`: representation types and boundary facts; no compiler driver or
  Salsa dependency.
- `cielo-frontend`: AST, lexer, and parser.
- `cielo-lowering`: the real AST-to-Core lowering.
- `cielo-sema`: type/effect checking and facts that are valid for one Core
  snapshot.
- `cielo-staging`: comptime evaluation, BTA, monomorphization, handler
  specialization, residualization, and their phase products.  It exposes
  ordinary Rust functions and does not know Salsa.
- Future `cielo-concurrency`: one semantic concern, exposing ordinary Rust
  functions.
- `cielo-memory`: neutral runtime input plus separate `refcount`, `tracing`,
  and `regions` implementations.  A strategy owns its analyses and output
  type.  The active ARC CFG analysis now lives under `refcount`; the first
  slice also has three concrete strategy families and a structured
  `RuntimeManifest`.
- `cielo-backend-api`: the shared machine/runtime contract.  It has no
  dependency on `cielo-memory`; strategy-specific root maps, ownership facts,
  and region constraints stop before this boundary.  Backends advertise
  runtime capabilities and report missing manifest requirements before
  emission.
- `cielo-backend-c`: pure CFG-to-C rendering and the C runtime header.  The
  temporary orchestration wrapper stays in `cielo-compiler` until CFG lowering
  has its own crate.
- `cielo-db`: the only crate that knows Salsa.  It composes pure compiler
  functions into coarse queries.
- `cielo-driver`: CLI, file I/O, profile selection, and final emission.

The old `cielo` package is now `crates/cielo-compiler` (with library name
`cielo`) so existing integration-test imports stay stable.  The repository
root has no `src`, `tests`, or `benches` directory.

## Salsa policy

Use Salsa for source inputs and meaningful products, not for individual IR
nodes or mutable pass state.  Profiles are currently small immutable values in
the relevant query keys, so several experiment variants can coexist in one
database.  If an IDE later needs to mutate profiles in place, make each profile
dimension its own `#[salsa::input]`; do not introduce one global config input.

Current query boundaries:

```text
parsed_module(SourceFile)                         real parser
lowered_core(SourceFile, TargetProfile)           real AST -> Core lowering
checked_core(SourceFile, SemanticsProfile, TargetProfile)
                                                  real module typechecking
typed_module(...)                                 temporary small summary
staged_module(...)                                temporary small summary
runtime_module(...)                               temporary small summary

refcount_module(..., RefcountProfile, ...)        separate strategy query
tracing_module(..., TracingProfile, ...)          separate strategy query
region_module(..., RegionProfile, ...)            separate strategy query
memory_module(..., MemoryProfile, ...)             thin dispatcher
machine_module(..., TargetProfile)                common backend boundary
```

The queries return ordinary immutable `Arc` products.  Query bodies call pure
Rust code and do not perform file writes, invoke external tools, or mutate
builders.  The driver performs those side effects after the final artifact is
returned.

Configuration is split by semantic effect.  A backend change must not be a
dependency of type checking; a memory strategy change must not be a dependency
of parsing or staging.

## Artifact boundaries

```text
SourceFile
  -> ParsedModule
  -> CoreModule
  -> TypedCoreModule
  -> StagedCore
  -> RuntimeModule
  -> MemoryModule (strategy-specific)
  -> MachineModule
  -> Artifact
```

Each product contains facts valid at that boundary only.  Facts from a previous
rewrite are not carried forward unless a later pass explicitly consumes them.
The neutral runtime representation contains allocation, load/store, call,
spawn/suspend, and continuation facts, but no retain/release, safepoint, or
region operation.

## Strategy rule

The dispatch point selects a family and requests only that family query:

```rust
match memory.model {
    MemoryModel::ReferenceCounting => refcount_module(db, memory.refcount),
    MemoryModel::Tracing => tracing_module(db, memory.tracing),
    MemoryModel::Regions => region_module(db, memory.regions),
}
```

`refcount`, `tracing`, and `regions` may have different analyses and output
structures.  They converge at `MachineModule`, not at an artificial common
memory plan.  Each family has a separate algorithm choice: for example tracing
currently distinguishes mark-and-sweep from semi-space collection, while
regions distinguishes lexical, constraint-based, and flow-sensitive variants.
The driver exposes these as `--memory`, `--refcount-algorithm`,
`--tracing-collector`, and `--region-algorithm` experiment switches.

## Cycle rules

- Put only stable vocabulary below sibling algorithm crates.  Do not make a
  catch-all model crate own algorithm facts or arena identities.
- Put orchestration above sibling crates.  The Salsa wrapper stays in
  `cielo-db`; a pure integration solver can live below it.
- If two analyses are genuinely simultaneous, expose one solver returning one
  product (for example `StageEffectSolution`) instead of two queries calling
  each other.
- Compute recursive SCCs explicitly inside one query.  Do not rely on an
  accidental Salsa query cycle for ordinary compiler recursion.

## Implementation order

1. **Done:** add the workspace and this document.
2. **Done:** extract `cielo-base` and make the old `common` module re-export it.
3. **Done:** move the real Core/Linear/CFG definitions into `cielo-ir` without
   redesigning Core.
4. Add `cielo-db` with source/profile inputs and coarse queries.  Keep query
   wrappers thin and pure. **Done:** parsing, Core lowering, and typechecking
   are real queries; execution events and memo memory statistics are exposed.
5. Add strategy-specific memory products and a neutral runtime manifest.
   **Done:** this lives in `cielo-memory`, with a backend-neutral machine
   boundary in `cielo-backend-api`.
6. Move frontend and semantic code behind those boundaries. **Done:** their
   files and tests are physically owned by `cielo-frontend`, `cielo-lowering`,
   and `cielo-sema`; there are no `include!` shims.
7. Move the CLI to `cielo-driver`. **Done.** The repository root is now a pure
   workspace and has no `src`, `tests`, or `benches` directory.
8. **Done:** move monomorphization/comptime/BTA/residualization, their phase
   products, normalization, constant tables, local analyses, and staging
   reports/diffs into `cielo-staging`.  The old `cielo::passes` and
   `cielo::pipeline::*` paths are reexports only.
9. Replace the temporary `RuntimeModule` summary with the real runtime CFG,
   then finish porting the existing RC implementation into
   `cielo-memory::refcount` (the active CFG liveness/ARC insertion slice is
   already there).  Tracing and region experiments remain siblings and may use
   different facts.
10. **Partly done:** pure C rendering and the runtime header live in
    `cielo-backend-c`.  Move CFG/linear lowering out next, then move the thin C
    orchestration wrapper and retire the `cielo-compiler` compatibility
    modules.

## First-pass acceptance checks

- `cargo check --workspace` and the existing test suite still pass.
- A source edit invalidates parse and downstream products, while changing only
  the memory profile does not call parse/type queries.
- Selecting reference counting does not execute tracing or region code.
- The dependency graph has no Cargo cycles and `cielo-db` is the only Salsa
  consumer.
- The old Core remains unchanged in this branch; replacing it later should
  require changing an artifact adapter, not the database or driver contract.
- No unit tests live under any crate's `src`; each crate owns integration tests
  in its own `tests` directory.

## Deliberate temporary seams

- `parsed_module`, `lowered_core`, and `checked_core` are real compiler work.
  The current `staged_module` and `runtime_module` queries are summaries until
  comptime file reads become explicit database inputs and the real runtime CFG
  is the query output.
- `cielo-memory` temporarily depends on `cielo-staging` because the legacy ARC
  CFG pass still accepts `SemanticTables` and writes legacy `ArcStats`.  The
  real `RuntimeCfg -> refcount/tracing/regions` boundary removes that edge.
- `cielo-compiler` remains a compatibility and integration crate for linear
  lowering, CFG construction, and backend orchestration.  It no longer owns
  parsing, Core lowering, typechecking, staging, the active ARC CFG pass, or C
  rendering.
