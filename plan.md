# Cielo v0 Implementation Plan

## Goal
Build a minimal end-to-end compiler that validates the core idea:
effects + handlers can determine comptime vs runtime staging.

## Locked decisions
- Monomorphization runs before CT propagation/BTA/residualize.
- Effect annotations are erased after BTA; downstream passes use effect summaries.
- v0 effect surface is minimal handlers (single-shot resumptions).
- Backend-first strategy: lower to a linear runtime IR, then emit C.
- Error model: `ErrorNode` + structured diagnostics.

## Current status
- [x] Bookkeeping pass: docs updated for pipeline and effect-annotation lifecycle.
- [x] Core crate scaffolding and module layout.
- [x] Core IDs/spans/diagnostics primitives.
- [x] Type/effect model (including sorted effect rows).
- [x] Core IR (`Expr`/`Stmt` split) and program containers.
- [x] Pass-state structs with explicit contracts between phases.
- [x] Minimal compile driver skeleton.
- [x] Tokenizer/parser subset.
- [x] AST -> Core lowering (initial subset).
- [x] Type+effect checker (initial Core pass; literals/arithmetic/bool ops).
- [ ] Effects in frontend/Core (in progress):
  - [x] Effect declarations + function `with` effect annotations parsed.
  - [x] `perform` / `handle` surface syntax parsing.
  - [x] Lower effect operations/handlers into Core effect nodes.
  - [ ] Type/effect checking for effect operations and handler clauses (in progress).
    - [x] Statement effect rows for `perform`.
    - [x] Handler discharge subtraction in effect summary.
    - [x] Operation signature checking and clause arity/type validation.
- [x] Monomorphizer (v0 identity table).
- [x] CT propagation (v0 literal folding cache).
- [x] BTA/provenance (v0 expr-stage classification + stage directives).
- [x] Residualizer (v0 identity wrapper).
- [x] `@comptime/@runtime` directives parsed, lowered, and applied in BTA.
- [ ] Handler lowering to linear IR (in progress).
  - [x] Residual Core -> linear runtime IR pass (`passes::linearize`).
  - [ ] Runtime scaffolding for handlers (`cielo_handler_push/pop`, handled-effect interception) (in progress).
  - [ ] Full runtime semantics for handlers/resumptions (continuations + clause body control flow).
- [ ] C emitter (in progress).
  - [x] Linear IR -> C translation unit (`passes::c_emit`) with runtime value model and stubs.
  - [x] Match lowering via ctor runtime helpers (`cielo_ctor_is_variant`, `cielo_ctor_field`).
  - [ ] Full continuation-aware handler lowering.

## Execution order
1. Core foundation (now):
   - crate layout, IDs, spans, diagnostics, type/effect primitives, core IR, pass structs
2. Frontend:
   - tokenizer + parser for strict subset
   - AST -> Core lowering
3. Semantics:
   - name resolution + type/effect checker + concrete effects assertion
   - effect operation/handler typing and effect row checks
4. Staging:
   - monomorphization -> CT propagation -> BTA -> residualization
5. Backend:
   - runtime lowering to linear IR -> C emitter
   - effect handler lowering path for non-discharged handlers
6. Quality:
   - golden tests per pass + diagnostics snapshots

## Contracts we will enforce in code
- Every pass has explicit input/output structs.
- IR invariants are structural (e.g., purity by enum split, not by ad-hoc checks).
- Passes communicate via side tables keyed by typed IDs.
- Diagnostics accumulate; recoverable failures become error nodes.

## Immediate next actions
- [x] Implement operation/handler clause semantic checks (arity + types + unknown op diagnostics).
- [x] Improve diagnostics rendering to line/column (not just byte-offset spans).
- [x] Add effectful `if`/`block` lowering coverage where needed for v0 core examples.
- [x] Design v0 residual effect-summary rewrite after BTA (erase arrow annotations, keep metadata).
- [x] Add oracle-style end-to-end tests for stage directives + handler discharge interactions.
- [ ] Replace C backend TODO stubs with real handler+match lowering (in progress).
  - [x] Match lowering is implemented.
  - [ ] Handler lowering is partially implemented with runtime scope scaffolding (in progress).

## Notes
- `reference/` is now populated and can be used for targeted adaptation (not copy/paste) of handler/effect internals.

---

# Cielo v1 Implementation Plan (active)

## Goal
Ship the first semantically meaningful upgrade over v0: explicit CT-only function semantics and stronger effect metadata, while preserving the existing end-to-end C pipeline.

## v1 slices (ordered)
- [x] S1: macro single-source cleanup for repeated compiler tables (keywords/builtin types/primitives).
- [x] S2: CT-only function declaration + enforcement.
  - [x] parse function-level `@comptime fn ...`
  - [x] lower to `FunctionDecl.ct_only = true`
  - [x] reject calls where any argument is runtime after BTA (`hard error`)
  - [x] tests for accepted/rejected callsites
- [x] S3: Effect property model extension.
  - [x] explicit effect properties with `LocalState` vs `SharedState` split
  - [x] wire properties into semantic tables
  - [x] `is_thunkable(effect_row)` helper
  - [x] BTA marks non-thunkable effect results as runtime provenance
- [x] S4: BTA provenance upgrade (ASCAPE baseline).
  - [x] emit reason chains for RT outcomes in diagnostics-friendly form
  - [x] include CT-only violations and unresolved-effect blockers in chain text
  - [x] expose provenance sample output in `--dump=sema` and main summary
  - [x] lock baseline with focused provenance tests
- [ ] S5: Handler lowering groundwork for v1 (in progress).
  - [x] classify call sites into `PureCall`/`DirectCall`/`ControlCall`
  - [x] keep semantics conservative (no unsound rewrites): wrappers are identity hooks
  - [x] thread conventions into linear IR + C lowering hooks
  - [x] support tail resumptive clauses (`| op(..., resume) => resume(value)`) by threading clause result into operation continuation
  - [x] support non-tail resumptions via explicit `Resume` core node and continuation reification into linear control flow
  - [x] add reference-aligned tail-resumption elimination in linearization (no extra resume wrapper for identity-tail paths)
  - [ ] first-class/multi-shot resumptions and resume value capture
- [ ] S6: Handler specialization (bounded, in progress).
  - [x] specialize at most once per `(function, handler)` pair (groundwork for handle-wrapped callsites)
  - [x] retie recursive edges to specialized copies
  - [x] dedupe equivalent handlers by structural shape key (avoid duplicate specialization)
  - [x] push handlers into specialized function bodies for direct call wrappers (`handle { f(...) }`)
  - [x] extend pushdown through wrapper-only bodies (`let` chains + return-forwarding `val` wrappers)
  - [ ] extend pushdown beyond wrapper-only bodies (block/control-heavy handler bodies)
  - [x] avoid over-specialization for structurally equivalent handlers
- [x] S7: Type inference overhaul for v1 usability.
  - [x] HM-style unification engine with scheme-based environments
  - [x] function-level generic parameter templates from signature symbols
  - [x] let-generalization for pure `let` bindings (value-restriction baseline)
  - [x] ADT declaration field types preserved through lowering
  - [x] constructor field typing checks (struct/enum arity and type mismatch diagnostics)
- [x] S8: Residualizer semantics parity with CT/BTA tables.
  - [x] replace CT-cached expressions with literal forms in Core IR
  - [x] prune `if` branches using CT branch decisions / bool cache
  - [x] prune `match` roots when scrutinee variant is statically known
  - [x] lock behavior with manual Core-level pass tests
- [x] S9: Source `if`/`match` integration into Core lowering.
  - [x] add frontend `match` syntax (`match ... { | Arm => ... | _ => ... }`)
  - [x] lower `if` expressions into Core `StmtKind::If` value statements
  - [x] lower `match` expressions into Core `StmtKind::Match` arms/default
  - [x] add parser/lowering/backend tests to ensure end-to-end behavior
- [x] S10: Match-pruning binder materialization in residualizer.
  - [x] when pruning known-variant `match`, preserve binder semantics with synthetic `let`s
  - [x] add Core-level pass test for binder flow
  - [x] add source end-to-end backend test for bindered match pruning

## Implementation notes
- Follow `docs/Decisions.md` phase integrity policy (phase-safe IDs + attached analyses).
- Keep each slice independently testable and end-to-end runnable (`--emit-c --run-c`).
- Prefer conservative semantics first, optimize later.
- For S5 convention grounding, align with `reference/core/Transformer.scala` (`CallingConvention`).
- For S6 bounded specialization grounding, align with `reference/core/optimizer/StaticArguments.scala`
  and recursion analysis notes in `reference/core/optimizer/Reachable.scala`.

---

# Cielo v1.1 Implementation Plan (active)

## Goal
Harden CT/RT boundary semantics and observability for practical iteration speed:
- explicit knownness split (`KnownLocal` vs `KnownPersistable`)
- target-aware CT evaluator behavior for integer arithmetic
- dependency-traced `ComptimeReadFiles` invalidation metadata
- constant-table embedding policy in C emission
- persisted staging-diff artifacts across rebuilds

## Scope and sequence
- [x] P1: CT propagation metadata hardening.
  - [x] Add `CtCacheKey` and typed file dependency records (`CtFileDep`) in phase tables.
  - [x] Include target + evaluator policy + compiler version in cache key.
  - [x] Record normalized file dependencies with content hashes for CT-only file reads.
- [x] P2: Target-aware CT integer semantics.
  - [x] Thread `TargetSpec` into CT pass.
  - [x] Apply target-width wrapping/sign-extension for integer unary/binary ops.
  - [x] Keep bool/float/string behavior unchanged in v1.1.
- [x] P3: Knownness split in BTA.
  - [x] Add `Knownness::{Unknown, KnownLocal, KnownPersistable}` side table.
  - [x] Classify CT-cache-backed expressions as `KnownLocal`.
  - [x] Upgrade to `KnownPersistable` when type persistability is not `NonPersistable`.
- [x] P4: Constant table embedding policy in C backend.
  - [x] Add deduplicated string constant pool for emitted C.
  - [x] Apply per-constant and per-compilation-unit byte caps.
  - [x] Keep unsupported embeddings out (no runtime pointer/resource embedding).
- [x] P5: Staging-diff diagnostics artifact.
  - [x] Persist compact snapshot (`ExprStableId -> stage, top reason, cause hash`).
  - [x] Diff previous snapshot on rebuild and render changed CT/RT classifications.
  - [x] Keep this as tooling-level utility (non-semantic).

## Explicit non-goals for v1.1
- Full MetaOCaml-style `mkid/lift_t/genlet` API surface.
- Multi-shot resumptions.
- Full nested-handler fusion/specialization.
- Full IEEE-strict floating-point CT emulation.

## Validation matrix
- CT integer width behavior differs between 32-bit and 64-bit targets.
- `ComptimeReadFiles` dependency hashes change when file contents change.
- Knownness table marks cached literals as persistable-known.
- C emission deduplicates repeated string literals in const pool under cap.
- Staging snapshot diff reports RT->CT and CT->RT flips with top cause.

---

# v1.1 Closure Hierarchy (prospecting, high value first)

This section defines the highest-value remaining work needed to consider v1.1
feature-complete against `docs/Decisions.md` + `docs/Comptime passes.md`.
P1..P5 delivered the baseline; items below close semantic and robustness gaps.

## H1. Handler pipeline semantic closure (highest value)
- [x] Status: complete (v1.1 scope)
- Goal:
  - Make handler lowering semantics match the doc contract exactly:
    per-clause calling convention and selective CPS for Control paths.
- Deliverables:
  - [x] Convention classification is clause-driven (not only row-driven),
    with explicit downstream representation for Pure/Direct/Control cases.
  - [x] ControlCall path lowered with explicit continuation reification discipline
    (single-shot still, no first-class resumption values).
  - [x] Tail-resumption elimination remains sound with mutable/effectful edge cases.
- Notes:
  - 2026-02-17: resume single-shot check is now path-sensitive (branch-exclusive
    single resumes accepted; same-path double resumes still diagnosed).
  - 2026-02-17: tail-resumption checker now memoizes and treats stmt cycles
    conservatively as non-tail to avoid unsound direct lowering.
  - 2026-03-24: tail-resumption classification now rejects intermediate
    `Call`/`Perform` paths conservatively; direct tail-collapse is applied only
    to `Direct`-classified clauses.
  - Ambivalent: Core IR currently has no mutable-variable stmt equivalent to
    Effekt `Stmt.Var`; if/when that lands, tail-resumption gating must mirror
    the reference caveat explicitly.
- Why first:
  - This is the core execution semantics for algebraic effects in v1.1.
- References:
  - `docs/Decisions.md` (calling convention policy + single-shot scope)
  - `docs/Comptime passes.md` (Calling Convention Classification section)
  - `docs/Implementation ref.md`:
    - `core/Transformer.scala` (calling convention split)
    - `cps/Transformer.scala` (selective CPS model)
    - `core/optimizer/RemoveTailResumptions.scala`

## H2. Handler specialization expansion (bounded but broader)
- [x] Status: complete for v1 bounded scope
- Goal:
  - Extend specialization pushdown beyond direct/wrapper-only bodies while
    preserving termination and readability.
- Deliverables:
  - [x] Handle wrappers that include simple control-light structure
    (`let`/`val`/`if`/`match`) when they still forward to a unique
    specialization candidate.
  - [x] Keep strict bail-out rules for complex/control-heavy cases.
  - [x] Add reachability-aware pruning so unspecialized copies disappear reliably.
- Notes:
  - 2026-02-17: reachable-pruning + `FuncId` table remapping landed; tests assert
    cross-table id integrity after pruning.
  - 2026-04-09: wrapper-callee detection and body rewriting now accept
    `match` forwarding wrappers without a default branch when all arms resolve
    to the same callee; arm-callee mismatches still bail out conservatively.
  - Incomplete by design: generalized specialization with return-clause
    parameterization remains deferred to v2 selective CPS.
- Why second:
  - Big runtime win on realistic handler-heavy code, low risk when bounded.
- References:
  - `docs/Decisions.md` (specialization policy)
  - `docs/Comptime passes.md` (Handler Specialization section)
  - `docs/Implementation ref.md`:
    - `core/optimizer/StaticArguments.scala`
    - `core/optimizer/Reachable.scala`
    - `core/optimizer/Normalizer.scala`

## H3. Persistability boundary enforcement end-to-end
- [ ] Status: in progress
- Goal:
  - Enforce Trivial/Serializable/NonPersistable at the CT->RT boundary with
    predictable codegen behavior and diagnostics.
- Deliverables:
  - [x] Residual boundary checks reject non-persistable crossings consistently (in progress).
  - [ ] Serializable constants use structural pooling (beyond strings where feasible) (in progress),
    with deterministic size-cap policy.
  - [x] Diagnostics point to the exact crossing and reason (in progress).
- Notes:
  - 2026-02-17: string pooling + size caps are already in place from P4; broader
    structural pooling and boundary diagnostics are still open.
  - 2026-03-16: residualizer now gates CT literal embedding on BTA stage (`Ct`)
    + knownness (`KnownPersistable`), so runtime-forced / boundary-rejected
    expressions no longer embed cached CT literals.
  - 2026-03-16: non-persistable boundary diagnostics now anchor to the runtime
    boundary statement span and include the boundary statement id.
  - 2026-03-18: C emitter now pools repeated runtime scalar literals
    (`Int`/`Bool`/`Char`) via `static const CieloValue` symbols; floats and
    structural ADT constants remain future work.
  - 2026-03-22: C emitter now pools repeated literal ADT constructor values
    (`MakeStruct`/`MakeEnum`) with deterministic per-entry + per-compilation-unit
    byte caps; oversized or budget-spilled literals fall back to inline ctor calls.
  - 2026-04-08: persistability boundary indexing is now stage-context-aware;
    expression uses confined to `@comptime` stage blocks are excluded from CT→RT
    boundary diagnostics/reclassification, eliminating false positives for
    non-escaping CT-local values.
- Why third:
  - Prevents subtle unsoundness and makes CT results deployment-safe.
- References:
  - `docs/Decisions.md` (three-tier persistability policy)
  - `docs/Comptime passes.md` (three-tier persistability + residualization notes)

## H4. CT evaluator target semantics completion
- [ ] Status: in progress
- Goal:
  - Finish target-aware behavior beyond integer-width arithmetic where applicable.
- Deliverables:
  - [ ] Endianness/alignment/layout-sensitive ops consult `TargetSpec`.
  - [x] Evaluator instrumentation is stable and useful for diagnostics and test oracles.
  - [x] Documented float caveats remain explicit if strict emulation is deferred.
- Notes:
  - 2026-03-22: `CtEvalStats` now records deterministic fold/miss/iteration counters,
    exposed in sema dump output and locked by pass-level oracle tests.
  - 2026-04-05: CT fold surface now covers float arithmetic/comparison/equality and
    non-numeric equality (`Bool`/`Char`/`String`/`Unit`), with host-float fold
    accounting applied uniformly.
  - 2026-04-06: non-finite host-float folds (`NaN`/`Inf` operands or arithmetic
    overflow to non-finite) are kept unresolved to avoid target-fragile caching.
- Why fourth:
  - Needed for trustworthy cross-target CT behavior.
- References:
  - `docs/Decisions.md` (target-aware CT evaluation policy)
  - `docs/Comptime passes.md` (Target-aware evaluation section)
  - `docs/Implementation ref.md`:
    - `core/vm/VM.scala`
    - `core/vm/Instrumentation.scala`

## H5. Incremental CT/BTA cache robustness + staging diff stability
- [x] Status: complete (v1.1 scope)
- Goal:
  - Make rebuild behavior deterministic, explainable, and cheap for file-driven CT.
- Deliverables:
  - [x] Persistent query-style cache keyed by `CtCacheKey` + dependency content hashes.
  - [x] Clear invalidation reasons for `ComptimeReadFiles`.
  - [x] Staging diff stability improvements (favor stable expression identity/fingerprint
    over fragile index-only comparisons when feasible).
- Notes:
  - 2026-03-28: staging snapshot ids switched to span+kind+structural fingerprints
    (index-independent), reducing false churn under expression renumbering.
  - 2026-03-28: `ComptimeReadFiles` invalidation reasons are persisted and diffed via
    `.ctdeps.tsv` snapshots (added/removed/changed deps + cache-key field changes).
  - 2026-04-03: persistent CT query cache (`.ctquery.tsv`) restores prior CT cache
    results when `{CtCacheKey + normalized deps + program fingerprint}` matches.
- Why fifth:
  - High productivity multiplier once semantics are solid.
- References:
  - `docs/Decisions.md` (ComptimeReadFiles + ASCAPE diagnostics)
  - `docs/Comptime passes.md` (ComptimeReadFiles in demand evaluation)
  - Practical model in build/query systems (Shake/Salsa-style dependency invalidation).

## Cross-cutting quality bar for all H1-H5
- Phase integrity:
  - Keep phase-specific IDs + analysis/IR pairing strict per `docs/Decisions.md`.
- Testing:
  - Add oracle tests for each milestone:
    - pass-level shape assertions
    - end-to-end emitted-C behavior
    - adversarial edge cases (resume misuse, specialization bail-outs,
      non-persistable crossings, dep-hash invalidation).
- Tooling:
  - Keep `main` dump outputs authoritative for pass introspection (`ast/core/sema/linear/c`).

## Need to handle at some point
- Closures
- Memory management with Nim's optimized arc gc
