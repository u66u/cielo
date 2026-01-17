# V2 Comptime Additions

This document describes comptime/staging features deferred from v1. It assumes
familiarity with `docs/v1/Comptime_passes.md` and only covers deltas.

---

## Partially-Static Data

### Motivation

v1 collapses any struct with a Stuck field to fully Stuck. This leaves CT value on the
table: a `Point(Known(1.0), Stuck(reason))` forces both fields to runtime even though
`x` is perfectly known.

### Design

Add a third Outcome variant:

```rust
enum Outcome {
    Known(Value),
    Partial(PartialValue),
    Stuck(Reason),
}

enum PartialValue {
    Struct {
        ty: SymbolId,
        fields: Vec<Outcome>,
    },
    Enum {
        ty: SymbolId,
        variant: SymbolId,
        fields: Vec<Outcome>,
    },
    Closure {
        params: Vec<VarId>,
        body: StmtId,
        env: Vec<(VarId, Outcome)>,
    },
}

Evaluator changes

Struct/enum constructors: If all fields Known → Known(Struct(...)) as before. If all fields Stuck → Stuck as before. If mixed → Partial(PartialValue::Struct { ... }).

Field access on Partial: Returns the individual field's Outcome. Code that only reads Known fields of a Partial struct is fully CT-evaluated:

Rust

fn eval_field_access(&mut self, obj: &Outcome, field_idx: usize) -> Outcome {
    match obj {
        Outcome::Known(Value::Struct { fields, .. }) => Outcome::Known(fields[field_idx].clone()),
        Outcome::Partial(PartialValue::Struct { fields, .. }) => fields[field_idx].clone(),
        Outcome::Stuck(reason) => Outcome::Stuck(reason.clone()),
        _ => Outcome::Stuck(Reason::UnclassifiedRuntime),
    }
}

Function calls with Partial arguments: Conservatively treat as Stuck in v2.0. In v2.1, add field-level tracking: if the function body only reads Known fields of the Partial arg, evaluate. Requires an analysis pass on the function body to determine which fields are accessed. Expensive but high payoff for config-struct patterns.

Match on Partial enum: If the variant tag is Known (it always is — an enum is either fully Known or Partial with Known tag), the match can select the correct arm and bind fields to their individual Outcomes:

Rust

// match Known_or_Partial(Some(Known(42), Stuck(reason))) {
//     Some(x, y) => ...  // x = Known(42), y = Stuck(reason)
//     None => ...
// }

Residualizer changes

Emit mixed constructors with Known fields as literals and Stuck fields as code:

Rust

fn residualize_make(&mut self, tag: SymbolId, fields: &[ExprId]) -> Expr {
    let residual_fields: Vec<Expr> = fields.iter().map(|&expr_id| {
        match self.outcomes[expr_id] {
            Outcome::Known(ref v) => self.embed_value(v),
            _ => self.residualize_expr(expr_id),
        }
    }).collect();
    Expr::MakeStruct { ty: tag, fields: residual_fields }
}

C emission produces:

C

CieloPoint p = { 1.0, _r5 };  // 1.0 is CT (literal), _r5 is RT (register)

Persistability of Partial values

A Partial value is not persistable (it contains Stuck components that cannot cross the stage boundary). Partial values cannot be used inside @comptime blocks. They are an optimization for residual code quality, not a staging mechanism.
Interaction with normalizer

The normalizer does not need changes for Partial values. After residualization, Partial structs become ordinary constructor calls with some literal arguments — the normalizer treats them like any other expression.
Interaction with constant table

Partial values are not placed in the constant table. Only fully Known values are eligible. The Known fields of a Partial struct are inlined as literals at the constructor call site.

[Research ref: Yallop, von Glehn, Kammar — "Partially-static data as free extension of algebras" (ICFP 2018). Key insight: partial static data is the free extension of a static algebra by dynamic generators.]
Query Architecture
Motivation

v1 uses function-level memoization with cycle detection in the evaluator: (FuncId, Vec<Value>) → Outcome. This is a minimal query pattern that works for the fused Evaluate+Classify pass but does not support:

    Fine-grained incremental recomputation (re-evaluate only affected queries when a file changes)
    Cross-pass memoization (sharing computed results between staging and later passes)
    Demand-driven evaluation order (evaluate a function only when its result is needed, not when it appears in the IR walk)

Design

Adopt a Salsa-style query framework. Each analysis becomes a query with:

Rust

trait Query {
    type Key: Hash + Eq + Clone;
    type Value: Clone;
    
    fn execute(&self, key: &Self::Key, db: &QueryDb) -> Self::Value;
}

struct QueryDb {
    storage: HashMap<TypeId, Box<dyn Any>>,  // per-query-type storage
    deps: HashMap<QueryKey, Vec<QueryKey>>,  // dependency graph
    in_progress: HashSet<QueryKey>,          // cycle detection
}

Queries for the staging pipeline

Rust

// "What is the staging outcome for this expression?"
struct ExprOutcomeQuery;
impl Query for ExprOutcomeQuery {
    type Key = ExprId;
    type Value = Outcome;
    fn execute(&self, key: &ExprId, db: &QueryDb) -> Outcome {
        // ... evaluate expression, recording dependencies on sub-queries
    }
}

// "What is the staging outcome for this function call?"
struct FuncCallQuery;
impl Query for FuncCallQuery {
    type Key = (FuncId, Vec<Value>);  // function + concrete args
    type Value = Outcome;
    fn execute(&self, key: &(FuncId, Vec<Value>), db: &QueryDb) -> Outcome {
        // ... evaluate function body with args bound
    }
}

// "What is the handler analysis for this handler?"
struct HandlerAnalysisQuery;
impl Query for HandlerAnalysisQuery {
    type Key = HandlerId;
    type Value = HandlerAnalysis;
}

// "What is the usage of this variable?"
struct UsageQuery;
impl Query for UsageQuery {
    type Key = VarId;
    type Value = Usage;
}

Incremental invalidation

When a file changes (detected via BLAKE3 content hash), invalidate all queries that transitively depended on a ComptimeReadFiles query reading that file. Re-execute only those queries. Other queries retain their cached results.

Dependency tracking: each query records which other queries it called during execution. The dependency graph is persisted between compilations (.ctquery.tsv).
Cycle handling

Cycles are detected via the in_progress set. When a query encounters a cycle:

    For staging queries: return Stuck(FuelExhausted) — conservative but correct. The recursive function will be classified as fuel-limited.
    For usage queries: return Usage::Recursive.

Migration path from v1

v1's function-level cache is a single-query system. To migrate:

    Extract the evaluator's memoization into a FuncCallQuery.
    Add ExprOutcomeQuery for per-expression caching (v1 stores outcomes in a DenseMap; this becomes the query's storage).
    Add dependency tracking wrappers around file reads.
    Add cross-compilation persistence.

The evaluator's core logic (eval_expr, eval_stmt) does not change — only the memoization layer around function calls is replaced with the query framework.

[Reference: Salsa (rust-analyzer's query framework). Adapt the demand-driven + memoized

    cycle-detecting pattern. Cielo's staging queries are simpler than Salsa's because staging is a single-phase analysis, not a cross-phase incremental system.]

Flow-Sensitive Mutable State Tracking
Motivation

v1 conservatively marks mutable variables as Stuck permanently after any Stuck write. This prevents CT evaluation of code like:

cielo

let mut acc = 0        // Known(0)
acc = acc + ct_value   // Known (still CT)
acc = acc + rt_value   // Stuck — acc is now permanently Stuck
acc = 0                // v1: still Stuck. v2: Known(0) again!
let result = acc + 1   // v1: Stuck. v2: Known(1)

Design

Track per-variable outcome at each program point, not just once per variable:

Rust

struct FlowState {
    var_states: HashMap<VarId, Outcome>,
}

At each Put(var, value):

    If value is Known → var becomes Known at this point
    If value is Stuck → var becomes Stuck at this point

At each Get(var):

    Return the current flow state for var

At join points (after if/match):

    If both branches agree (both Known with same value) → Known
    If branches disagree → Stuck

Complexity

Flow-sensitive analysis is more expensive than v1's single-assignment tracking. For each function, the analysis walks the body with a FlowState that branches at if/match and joins at convergence points. This is O(n * |vars|) per function where n is the number of statements.
Interaction with handlers

Mutable state inside handler clauses interacts with backtracking. If a handler clause is non-tail-resumptive and captures mutable state, the flow analysis must assume the variable could be in any state after the handler clause executes (because the continuation might be resumed with different state).

For tail-resumptive clauses, flow analysis can track through the resume point (state flows linearly).
Prerequisite

Requires the Partial value infrastructure (a variable can be Partial if assigned a Partial struct). Without Partial, flow-sensitive tracking has less payoff.
Extended CT-Allowed Effects
Motivation

v1 allows a fixed set of effects during CT evaluation: {Pure, Diverge, Alloc, LocalState, ComptimeReadFiles}. Users may define custom CT-only effects (e.g., ComptimeLog, BuildConfig) that are deterministic given their inputs and should be CT-evaluable.
Design

Effect properties gain a ct_evaluable flag:

Rust

struct EffectProperties {
    cap_level: CapLevel,
    is_local: bool,
    is_discardable: bool,
    is_commutative: bool,
    cardinality: Cardinality,
    ct_only: bool,
    ct_evaluable: bool,  // NEW: can this effect be evaluated at CT?
}

The evaluator checks ct_evaluable instead of hardcoding the allowed set. User-defined effects can opt in:

cielo

effect ComptimeLog {
    @ct_evaluable
    fn log(msg: String) -> ()
}

The evaluator must have a handler for the effect in the CT capability stack. The handler must be CT-evaluable (all captures Known, clause body CT-eligible).
Safety

CT-evaluable effects must be deterministic given their inputs. The compiler does not verify this — it's a user assertion. Non-deterministic CT evaluation would produce unstable staging results (different CT values on different builds).

The staging report warns when a user-defined CT-evaluable effect is exercised:

text

[staging] Custom CT effect `ComptimeLog` exercised 14 times during evaluation

Per-Clause Partial Discharge
Motivation

v1 classifies each handler clause as fully Discharged or not. v2 allows partial discharge: a clause that is CT-evaluable for some argument patterns but not others.
Example

cielo

effect Transform {
    fn apply(x: Int) -> Int
}

handler conditional_transform {
    | apply(x, resume) => {
        if x > 0 { resume(x * 2) }        // CT-evaluable path
        else { resume(external_call(x)) }  // not CT-evaluable (IO)
    }
}

When apply is called with Known(5) (positive), the clause takes the CT-evaluable path. When called with Known(-1), it takes the non-CT-evaluable path.
Design

Clause discharge becomes conditional on the argument values:

Rust

enum ClauseDischargeStatus {
    Discharged,                            // always CT-evaluable
    ConditionallyDischarged(Vec<Pattern>), // CT-evaluable for matching args
    TailResumptive,                        // not CT-evaluable, tail-resumptive
    NeedsCPS,                              // not CT-evaluable, needs CPS
}

The evaluator attempts CT evaluation of the clause body. If it succeeds → Discharged for this call. If it fails (hits a Stuck point inside the clause) → fall back to TailResumptive or NeedsCPS classification.
Complexity

This requires speculative evaluation of clause bodies, which may fail. The evaluator must be able to roll back state changes from a failed clause evaluation. This is where the evaluator's stack-based mutable state design pays off: save the stack pointer, attempt evaluation, restore on failure.

v1 explicitly does not use rollback for staging (stated in docs/v1/Compiler_design.md). v2 adds it specifically for per-clause conditional discharge.
Staging Snapshots and Diffs
Persistent staging snapshots

The full StagingTables are serialized to disk between compilations. The format is:

    .ctdeps.tsv: file dependencies (path + BLAKE3 hash)
    .ctquery.tsv: query cache (key + result + dependency list)
    .ctstaging.bin: binary dump of outcomes + handler analysis + branch decisions

Staging diffs

On recompilation, load the previous snapshot and diff against the current results. Expressions are matched by span+kind+structural fingerprint (not ExprId).

```Rust

struct StagingDiff {
    ct_to_rt: Vec<StagingChange>,
    rt_to_ct: Vec<StagingChange>,
    unchanged_ct: usize,
    unchanged_rt: usize,
}

struct StagingChange {
    span: Span,
    kind: &'static str,        // "variable", "call", "branch", etc.
    old_outcome: OutcomeSummary,
    new_outcome: OutcomeSummary,
    reason: String,            // human-readable explanation
}

IDE integration

The staging diff is exposed via LSP:

    Inline decorations showing CT/RT status per expression
    Hover showing provenance chain
    Code action: "make this comptime" with suggested refactoring
```