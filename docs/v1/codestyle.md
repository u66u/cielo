## DRY + cohesion/continuity of code

No short/useless helper functions that are used once somewhere and can be inlined, unless they bring extra clarity to code.
No almost identical functions that aren't used frequently and can be merged into a single one maybe with an optional parameter.
Err on the side of using/extending already present code as opposed adding new functions, unless they bring additional clarity/prevent dependency confusion

## Separate structs with corresponding fields for each pass

Each phase produces a distinct struct. Phase transitions consume the previous struct.
Only tables needed by downstream passes are carried forward.

Example:
```rust
pub struct Parsed {
    pub ast: AstProgram,
    pub diagnostics: DiagnosticBag,
}

pub struct Typed {
    pub program: CoreProgram,
    pub diagnostics: DiagnosticBag,
    pub sema: SemanticTables,
}

pub struct Monomorphized {
    pub program: CoreProgram,
    pub diagnostics: DiagnosticBag,
    pub sema: SemanticTables,
    pub mono: MonomorphizationSummary,
}

pub struct Staged {
    pub program: CoreProgram,
    pub diagnostics: DiagnosticBag,
    pub sema: SemanticTables,
    pub staging: StagingTables,
    // mono consumed — not needed downstream
}

pub struct Residualized {
    pub program: CoreProgram,
    pub diagnostics: DiagnosticBag,
    pub sema: SemanticTables,
    pub residual: ResidualTables,
    // staging consumed — not needed downstream
}
```

Each transition method consumes self:
```rust
impl Monomorphized {
    pub fn into_staged(self, staging: StagingTables) -> Staged {
        Staged {
            program: self.program,
            diagnostics: self.diagnostics,
            sema: self.sema,
            staging,
        }
    }
}
```

Tables not read after a phase boundary are dropped at the transition.
Attempting to access dropped tables is a compile-time type error.

## Optimal data structures

Use smallvec (when explicit data structures are not necessary), arenas by IDs, bitflags,
other data structures and practices that improve performance, don't be fanatical about
it though — we prioritize ease of reasoning over perf.

DenseMap<K, V> (Vec-backed, indexed by newtype IDs) for per-node side tables.
HashMap for sparse mappings (function-level caches, dedup tables).
SmallVec<[T; 4]> for child lists and short collections.

## Rich error reporting

In our compiler errors are first class nodes, they accumulate as we go through phases.
Use miette to display them cleanly and point to specific source code places.

Staging errors include provenance chains: follow DependsOnVar links through the outcomes
table to produce multi-line explanations of why something is RT.

## Authority

`docs/v1/Decisions.md` is authoritative for v0/v1 style acceptance policy and phase integrity.
This file is tactical guidance.

## Advanced pattern gating (v0/v1)

Use advanced syntax patterns only when they satisfy the policy in `docs/v1/Decisions.md`.

- Adopt now:
  - extension traits for domain semantics
  - scope guards (RAII) for context push/pop (evaluator capability stack, handler stack)
  - pattern matching compression when explicit and exhaustive
  - newtype wrappers for invariants and phase IDs
  - display impls for diagnostics and source-like dumps
  - generic fixpoint helpers for monotone analysis (normalizer shrinking loop, usage fixpoint)
  - IrNode trait for generic traversal across Core and Linear IRs
- Defer (case-by-case):
  - iterator alchemy for allocation-free traversal
  - const-generic fixed-capacity containers
  - bitset-based effect sets
- Avoid for now:
  - blanket `From/Into` IR construction tricks
  - closure-based IR builder DSLs
  - manual function-table dispatch instead of direct enum matching

## Stylistic choices

Overall code style is clean, simple, intuitive, self-documenting, and humanized.
Use code compression only when it does not reduce clarity, e.g. instead of defining
each id manually:

```rust
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct VarId(u32);
```

Generate the boilerplate once:
```rust
macro_rules! define_id {
    ($name:ident) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        pub struct $name(u32);

        impl $name {
            pub const INVALID: Self = Self(u32::MAX);
            #[inline]
            pub fn new(index: usize) -> Self { Self(index as u32) }
            #[inline]
            pub fn index(self) -> usize { self.0 as usize }
        }

        impl From<usize> for $name {
            #[inline]
            fn from(i: usize) -> Self { Self::new(i) }
        }
    };
}
```

When appropriate, add expressive "demo scene" style only if it preserves readability
and follows the gating policy above, e.g.
```rust
macro_rules! binops {
    ($($variant:ident : $prec:expr, $tok:pat),* $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum BinOp { $($variant),* }

        impl BinOp {
            pub fn precedence(self) -> u8 {
                match self { $(Self::$variant => $prec),* }
            }
        }

        impl TryFrom<TokenKind> for BinOp {
            type Error = ();
            fn try_from(tok: TokenKind) -> Result<Self, ()> {
                match tok {
                    $($tok => Ok(Self::$variant),)*
                    _ => Err(()),
                }
            }
        }
    };
}

binops! {
    Add:  10, TokenKind::Plus,     Sub:  10, TokenKind::Minus,
    Mul:  20, TokenKind::Star,     Div:  20, TokenKind::Slash,
    Mod:  20, TokenKind::Percent,  Eq:   5,  TokenKind::EqEq,
    Ne:   5,  TokenKind::BangEq,   Lt:   5,  TokenKind::Less,
    Gt:   5,  TokenKind::Greater,  Le:   5,  TokenKind::LessEq,
    Ge:   5,  TokenKind::GreaterEq,
    And:  3,  TokenKind::Keyword(Keyword::And),
    Or:   2,  TokenKind::Keyword(Keyword::Or),
}
```

Patterns like extension traits, scope guards, pattern compression, fixpoint helpers,
display impls, and newtype wrappers are preferred when they improve correctness and
maintainability. Others are deferred/avoided in v0/v1 per `docs/v1/Decisions.md`.

Example of NEWTYPE + DEREF:
```rust
pub struct SortedEffectRow(SmallVec<[EffectLabelId; 4]>);

impl SortedEffectRow {
    pub fn new(mut effects: SmallVec<[EffectLabelId; 4]>) -> Self {
        effects.sort_unstable();
        effects.dedup();
        Self(effects)
    }

    pub fn union(&self, other: &Self) -> Self {
        let mut merged = self.0.clone();
        for &e in &other.0 {
            if let Err(pos) = merged.binary_search(&e) {
                merged.insert(pos, e);
            }
        }
        Self(merged)
    }

    pub fn subtract(&self, other: &Self) -> Self {
        Self(self.0.iter().copied()
            .filter(|e| other.0.binary_search(e).is_err())
            .collect())
    }
}

impl std::ops::Deref for SortedEffectRow {
    type Target = [EffectLabelId];
    fn deref(&self) -> &[EffectLabelId] { &self.0 }
}

// Now you can write:
if row.is_empty() { /* pure */ }
if row.contains(&io_label) { /* has IO */ }
for &eff in row.iter() { /* iterate */ }
let residual = body_effects.subtract(&handler_ops);
```

## IrNode trait pattern

Both Core and Linear IRs implement a shared traversal trait. Generic analyses are written
once against the trait:

```rust
// ir/walk.rs
pub trait IrStmtNode {
    type StmtId: Copy + Eq + std::hash::Hash;
    type ExprId: Copy + Eq + std::hash::Hash;
    fn child_stmts(&self) -> SmallVec<[Self::StmtId; 4]>;
    fn child_exprs(&self) -> SmallVec<[Self::ExprId; 4]>;
}

pub trait IrProgram {
    type StmtId: Copy + Eq + std::hash::Hash;
    type ExprId: Copy + Eq + std::hash::Hash;
    type StmtNode: IrStmtNode<StmtId = Self::StmtId, ExprId = Self::ExprId>;
    fn stmt(&self, id: Self::StmtId) -> Option<&Self::StmtNode>;
}
```

This eliminates duplicated traversal code between Core and Linear analyses.
Functions like `stmt_mentions_var`, `collect_reachable_functions`, `fold_stmts`
become generic over `IrProgram`.

The trait is minimal by design. It does not attempt to abstract over expression
structure (which differs significantly between Core and Linear) — only the tree
walking skeleton is shared.

## Outcome and Value types

The CT evaluator's result types live in a shared module consumed by the evaluator,
residualizer, and diagnostics:

```rust
// passes/staging_types.rs (or pipeline/phases.rs)

#[derive(Clone, Debug)]
pub enum Outcome {
    Known(Value),
    Stuck(Reason),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    Char(char),
    Str(String),
    Struct { ty: SymbolId, fields: Vec<Value> },
    Enum { ty: SymbolId, variant: SymbolId, fields: Vec<Value> },
}
```

Value is richer than Literal (supports compound types). Literal stays for the C
emission path. `Value::as_literal()` converts trivial Values to Literals for
inline embedding.

## Evaluator environment pattern

The evaluator carries an environment as a scope stack (not a single flat map).
Push on function entry / handler entry, pop on exit. RAII scope guards ensure
pop happens even on early return:

```rust
struct EvalEnv {
    scopes: Vec<EnvScope>,
}

struct EnvScope {
    vars: SmallVec<[(VarId, Outcome); 8]>,
}

impl EvalEnv {
    fn push_scope(&mut self) { self.scopes.push(EnvScope::default()); }
    fn pop_scope(&mut self) { self.scopes.pop(); }
    fn bind(&mut self, var: VarId, outcome: Outcome) { ... }
    fn lookup(&self, var: VarId) -> Option<&Outcome> { ... } // walks scopes top-down
}
```

The scope guard pattern:
```rust
struct ScopeGuard<'a> { env: &'a mut EvalEnv }
impl Drop for ScopeGuard<'_> { fn drop(&mut self) { self.env.pop_scope(); } }

fn with_scope(env: &mut EvalEnv) -> ScopeGuard<'_> {
    env.push_scope();
    ScopeGuard { env }
}
```

## Model codebases

ripgrep, rust-analyzer, cranelift, ruff, some mlir patterns, etc.

## Pass Contracts

Every compiler pass must document a short contract. Keep it concise and explicit.

### Required core fields (all passes)

- Inputs
    - Exact IR/state consumed (for example: `Staged`, side tables, target config).
- Outputs
    - Exact artifacts produced (IR + side tables + diagnostics).
- Invariants
    - Properties guaranteed after the pass.
- Diagnostics
    - What errors/warnings can be emitted and at what granularity.
- Complexity
    - Expected time/space shape (linear walk, fixed-point, etc.).

### Optional fields (use when relevant)

- Preconditions
    - Facts assumed true on entry.
- Postconditions
    - Extra guarantees relied on by downstream passes.
- Failure behavior
    - Recovery strategy (`ErrorNode` propagation, poison, hard abort).
- Determinism
    - Ordering guarantees needed for stable output.
- Test checklist
    - Minimum required tests for this pass.

### Template

```
Pass: <name>

Purpose:
- ...

Inputs:
- ...

Outputs:
- ...

Invariants:
- ...

Diagnostics:
- ...

Complexity:
- ...

Optional:
Preconditions:
- ...
Postconditions:
- ...
Failure behavior:
- ...
Determinism:
- ...
Test checklist:
- ...
```

### Pass contracts for fused passes

**Pass: Evaluate+Classify**
```
Inputs:
- Monomorphized (CoreProgram + SemanticTables + MonomorphizationSummary)
- TargetSpec (from compiler config)

Outputs:
- Staged (CoreProgram + SemanticTables + StagingTables)
- StagingTables contains: outcomes, var_outcomes, handler_analysis,
  branch_decisions, usage, file_deps, cache_key, eval_stats

Invariants:
- Every reachable ExprId has an Outcome entry
- Every reachable VarId has a var_outcomes entry
- Every HandlerId has a handler_analysis entry
- All Known values are target-width-correct
- Branch decisions are consistent with outcomes (LiveTrue iff condition is Known(true))

Diagnostics:
- CT_ONLY_WITH_RT_ARGS: hard error for CT-only functions called with Stuck args
- COMPTIME_BLOCK_RT_REF: hard error for @comptime blocks referencing Stuck variables
- COMPTIME_BLOCK_NON_PERSISTABLE: hard error for non-persistable values crossing boundary
- FUEL_EXHAUSTED: warning when fuel runs out (includes expression and fuel count)
- SIZE_EXHAUSTED: warning when size budget runs out

Complexity:
- O(program_size * max_fuel) worst case per function
- Typically O(program_size) with function-level memoization

Preconditions:
- All effect rows concrete (no unresolved unification vars)
- All types monomorphized

Failure behavior:
- CT-only errors are hard (abort function analysis, propagate error)
- Fuel/size exhaustion produces Stuck (not error), evaluation continues
- Missing IR nodes produce Stuck with diagnostic

Determinism:
- Fully deterministic given same inputs + target spec + fuel
- No dependency on evaluation order of independent functions (each memoized independently)
```

**Pass: Residualize+Specialize**
```
Inputs:
- Staged (CoreProgram + SemanticTables + StagingTables)

Outputs:
- Residualized (CoreProgram + SemanticTables + ResidualTables)
- ResidualTables contains: function_effect_summary, constant_table

Invariants:
- No Known expression remains as non-literal code in residual
- Discharged handlers produce no Handle node in residual
- Constant table entries are structurally deduplicated
- Effect summaries reflect only operations present in residual

Diagnostics:
- SPECIALIZE_TERMINATION: warning if specialization depth exceeded (shouldn't happen with Set guard)

Complexity:
- O(program_size + specialization_count * avg_function_size)

Preconditions:
- StagingTables complete and consistent

Failure behavior:
- Missing outcomes default to Stuck (emit code, don't embed)
- Specialization failures fall through (keep handler in residual)
```

**Pass: Normalize**
```
Inputs:
- Residualized (CoreProgram + SemanticTables + ResidualTables)

Outputs:
- Residualized (same struct, program mutated in place)

Invariants:
- Shrinking phase is idempotent after fixpoint (second run changes nothing)
- No Recursive-usage function was inlined
- All Once-usage functions were inlined (if reachable)

Diagnostics:
- None (cleanup pass, no user-visible errors)

Complexity:
- Shrink: O(program_size * depth) per fixpoint iteration
- Speculative inline: O(program_size) single pass
- Total: O(program_size * depth) typical

Determinism:
- Fully deterministic
```

## Test extensively!

Especially with oracle testing where possible. Test should be in a separate /tests
folder, not in files inside src/.

### Staging-specific test categories

- **CT evaluation correctness:** Programs with known CT results. Assert specific Outcome
  values in the staging tables.
- **Branch elimination:** Programs with CT branch conditions. Assert dead branches are not
  walked (check that variables in dead branches have no Outcome entry).
- **Handler discharge:** Programs with various handler patterns. Assert correct
  ClauseDischargeStatus per clause.
- **Provenance chains:** Programs with RT expressions. Assert correct Reason at each node.
  Walk DependsOnVar links and verify the chain terminates at a root cause.
- **Residualization:** End-to-end tests. Compile, emit C, run. Assert correct output.
- **Normalizer idempotence:** Apply shrink twice. Assert identical IR.
- **Normalizer size:** Assert shrink never increases node count. Assert speculative inline
  only increases by bounded amount.
- **Target-aware arithmetic:** Same program compiled for 32-bit and 64-bit targets. Assert different CT results for overflow cases.
