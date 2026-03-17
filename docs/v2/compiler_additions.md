# V2 Compiler Additions

This document describes compiler infrastructure features deferred from v1. It builds
on `docs/v1/Compiler_design.md` — only additions and changes are listed here.

---

## Capability-to-Region Unification

### Motivation

v1 has two separate memory management stories:
1. Handler evidence structs and capability objects (allocated during handler installation)
2. Region-scoped allocations (`Region(BlockLit)` / `Alloc` nodes)

Both use scoped lifetimes. Both are freed when their scope exits. Unifying them reduces
implementation complexity and enables shared optimizations.

### Design

Map handler capability scopes to region scopes. Each `Handle` node implicitly creates a
region. Handler evidence (the capability struct, clause function pointers, captured state)
is allocated in this region.

Handle { handler, body, next } → Region { let evidence = Alloc(capability_struct, region); let body_result = body_with_evidence(evidence); next(body_result) }

text


When the handler scope exits, the region is freed, and all evidence is deallocated.

### Impact on handler lowering

The linearize pass (handler lowering) emits region operations instead of custom evidence
management:

// v1: custom evidence allocation let evidence = alloc_evidence(handler_clauses); let result = call_with_evidence(body, evidence); free_evidence(evidence);

// v2: region-based region r { let evidence = alloc(capability_struct, r); let result = call_with_evidence(body, evidence); } // evidence freed when region exits

text


### Impact on ARC

Handler evidence that never escapes its region (common case: ~95% of handlers) can be
stack-allocated. The escape analysis already needed for ARC optimization naturally covers
handler evidence when it's region-allocated.

### Impact on C emission

Region-allocated evidence becomes stack variables in C (when escape analysis confirms
non-escape) or arena-allocated (when evidence may escape, e.g., captured in a closure
that outlives the handler).

[Research ref: Müller, Schuster, Starup, Ostermann, Brachthäuser — "From Capabilities
to Regions: Enabling Efficient Compilation of Lexical Effect Handlers" (OOPSLA 2023).]

---

## Normalizer on Linear IR

### Motivation

v1 runs the normalizer only on Core IR (after Residualize+Specialize). Handler lowering
(linearize) introduces new simplification opportunities:

- DirectCall nodes that were previously Handle+Perform can be inlined
- Dead handler scaffolding (evidence allocation for eliminated handlers) can be removed
- Val-return patterns introduced by handler clause inlining

### Design

Extend the normalizer to work on Linear IR. The shrink-inline-shrink sandwich applies
identically, but the reduction rules operate on `LinearStmt` instead of `StmtKind`.

Required: the IrNode trait (already adopted in v1) makes the normalizer's generic
traversal work on both IRs. The reduction rules themselves need Linear-specific pattern
matching.

```rust
fn shrink_linear_stmt(
    program: &mut LinearProgram,
    stmt_id: LinearStmtId,
    usage: &DenseMap<VarId, Usage>,
) -> bool {
    let Some(stmt) = program.stmt(stmt_id) else { return false };
    match &stmt.kind {
        // Same rules as Core, but on Linear node types
        LinearStmt::Let { binding, .. } if usage[*binding] == Usage::Never => { ... }
        LinearStmt::Val { binding, value, next } => {
            if let Some(LinearStmt::Return(e)) = program.stmt(*value).map(|s| &s.kind) {
                // val-return commutation
                ...
            }
        }
        // Linear-specific: DirectCall to known small function → inline
        LinearStmt::DirectCall { callee, args, .. } => { ... }
        _ => false,
    }
}

Position in pipeline

After linearize, before effect-qualified opts:

text

... → linearize → normalize_linear → cfg_lower → effect-qualified opts → ARC → C

Effect-Qualified Optimizations (expanded)
Motivation

v1 gates optimizations on inst_effect_class (a per-instruction classification). v2 expands this to use the full effect property set for more precise optimization decisions.
Additional optimizations enabled by effect properties

Commutative effects: If two operations have commutative effects (e.g., two independent LocalState.get calls on different state cells), they can be reordered. v1 conservatively preserves order for all effectful operations.

Idempotent effects: If an operation has idempotent effects (e.g., LocalState.set(x, v) followed by LocalState.set(x, v) with the same value), the second can be eliminated. Requires value identity tracking.

Discardable effects: If an operation's result is unused and its effects are discardable (e.g., LocalState.get where the result is dead), the operation can be eliminated. v1 already handles this for Pure operations via dead code elimination.
Optimization table
Optimization	Required effect property	v1 status
Dead code elimination	Discardable	Pure only
CSE (common subexpression)	Commutative + Idempotent	Pure only
LICM (loop-invariant code motion)	Thunkable (cap_level < IO)	Implemented
Read-after-write forwarding	LocalState (not SharedState)	Implemented
Write-after-write elimination	LocalState (not SharedState)	Implemented
Read-after-read CSE	LocalState (not SharedState)	Implemented
Commutative reordering	Commutative flag	Not implemented
Idempotent elimination	Idempotent flag	Not implemented
Discardable dead code	Discardable flag	Not implemented
IrNode Trait Extensions
Motivation

v1's IrNode trait covers child_stmts() and child_exprs() for structural traversal. v2 extends it for richer generic analyses.
Additional trait methods

Rust

pub trait IrStmtNode {
    type StmtId: Copy + Eq + Hash;
    type ExprId: Copy + Eq + Hash;
    type VarId: Copy + Eq + Hash;

    fn child_stmts(&self) -> SmallVec<[Self::StmtId; 4]>;
    fn child_exprs(&self) -> SmallVec<[Self::ExprId; 4]>;
    
    // v2 additions:
    fn bound_vars(&self) -> SmallVec<[Self::VarId; 2]>;
    fn used_vars(&self) -> SmallVec<[Self::VarId; 4]>;
    fn is_pure(&self) -> bool;
    fn effect_class(&self) -> EffectClass;
}

These enable generic implementations of:

    Free variable tracking (using bound_vars and used_vars)
    Usage analysis (using used_vars)
    Purity checks (using is_pure and effect_class)

Generic analyses

Rust

pub fn compute_free_vars<P: IrProgram>(
    program: &P,
    root: P::StmtId,
) -> HashSet<P::VarId> {
    // Generic over IR type
    ...
}

pub fn compute_usage<P: IrProgram>(
    program: &P,
    entrypoints: &[P::FuncId],
) -> DenseMap<P::VarId, Usage> {
    // Generic over IR type
    ...
}

Nanopass Architecture (assessment)
v2 assessment

Nanopass (many small transformation passes, each doing one thing) was considered and rejected for v1 in favor of fused passes. For v2, reassess:

Arguments for nanopass:

    Each pass is independently testable and verifiable
    Easier to add new transformations without modifying existing passes
    Better for teaching/documentation

Arguments against nanopass:

    More IR traversals (slower compilation)
    More intermediate IR representations (more memory, more conversion code)
    Fused passes already capture the important optimizations

v2 decision: Keep fused passes for the hot path (Evaluate+Classify, Residualize+Specialize). Add nanopass-style small passes for new v2 transformations (handler fusion, flow-sensitive analysis) that are naturally separate from the existing fused passes.
E-Graph / Equality Saturation (assessment)
v2 assessment

E-graphs (equality saturation) were considered for optimization. Assessment:

Where e-graphs would help:

    Finding optimal rewrite sequences for effect-qualified optimizations (commutative reordering + CSE + LICM interact and have overlapping applicability)
    Superoptimization of small hot loops

Where e-graphs would not help:

    Handler specialization (fundamentally about function copying, not term rewriting)
    Staging analysis (not a rewriting problem)
    Dead code elimination (trivial without e-graphs)

v2 decision: Do not adopt e-graphs for v2. The effect-qualified optimization table above can be implemented as a priority-ordered sequence of rewrite passes without equality saturation. Revisit for v3 if the interaction between effect-qualified optimizations becomes complex enough to warrant it.
Improved C Emission
Motivation

v1 emits C in a straightforward IR-to-C translation. v2 improves C output quality to help C compilers optimize better.
Improvements

Structured control flow recovery: Instead of emitting all control flow as goto, recover if/else, while, for patterns from the CFG. C compilers optimize structured control flow better than goto-heavy code.

Type-aware emission: Emit C structs that match Cielo's data layout. Use restrict pointers where alias analysis confirms non-aliasing. Use const where possible.

Inline small functions: Emit small functions (≤ 5 lines of C) with static inline to help the C compiler's inliner.

Arena-aware emission: When a function's allocations are all region-scoped (detected by escape analysis), emit stack allocations or alloca instead of heap allocations:

C

// v1: always heap
CieloPoint* p = cielo_alloc(sizeof(CieloPoint));

// v2: stack when escape analysis confirms
CieloPoint p_storage;
CieloPoint* p = &p_storage;

SSA vs structured C

v1 TBD: "Do we just emit SSA, or do we want proper C to let compilers optimize heuristically?"

v2 answer: Emit structured C. The Linear IR preserves enough structure (if/match nodes, function boundaries) to recover structured control flow. SSA-style C (all gotos) defeats C compiler heuristics that look for loop patterns.
PGO Integration
Motivation

v1 TBD asks about extracting benefit from profile-guided optimization.
v2 approach

Lightweight PGO: instrument CT evaluation to record which functions are hot (called many times with Known args). Use this to guide speculative inlining thresholds:

    Functions called >100 times during CT evaluation with different Known args → raise inline threshold (they're likely hot at RT too)
    Functions never called during CT evaluation → lower inline threshold (save compile time)

This is not traditional PGO (no runtime profiling). It uses CT evaluation as a proxy for runtime behavior, which is valid because CT evaluation explores the same call graph.
Stack allocation via staging

v1 TBD identifies the opportunity: if a variable allocated in a function never escapes (Usage Analysis + escape analysis), it can be stack-allocated. Staging helps because:

    CT evaluation can "unroll" control flow, turning complex lifetimes into linear lifetimes
    The residualizer can see the unrolled lifetime and emit stack allocation
    This works for Pure and Direct functions

For Control functions (that yield), stack allocation is unsafe unless the allocation is in a region that persists across the yield. v2 uses the capability-to-region unification to make this decision: if the allocation is in the handler's region and the handler is still active after yield, the allocation persists.
