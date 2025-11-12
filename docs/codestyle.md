## Separate structs with corresponding fields for each pass

Example:
```
pub struct Parsed {
    pub hir: Hir,
}

pub struct Desugared {
    pub hir: Hir,
}

pub struct Typed {
    pub hir: Hir,
    pub env: Env,
    pub sema: Semantics,
}

pub struct Lowered {
    pub anf: Vec<AnfFunc>,
    pub env: Env,
}

impl Desugared {
    pub fn typecheck(self, intern: &Interner) -> Typed {
        let tc = TypeChecker::new(intern, self.hir.exprs.len(), self.hir.stmts.len());
        let checked = tc.check_program(&self.hir);
        Typed {
            hir: self.hir,
            env: checked.env,
            sema: checked.sema,
        }
    }
}
```
etc...

## Optimal data structures

Use smallvec (when explicit data structures are not necessary), arenas by IDs, bitflags, other data structures and practices that improve performance, don't be fanatical about it though - we prioritize ease of reasoning over perf.

## Rich error reporting
In our compiler errors are first class nodes, they accumulate as we go through phases. Use miette to display them cleanly and point to specific source code places.

## Authority
`docs/Decisions.md` is authoritative for v0/v1 style acceptance policy and phase integrity.
This file is tactical guidance.

## Advanced pattern gating (v0/v1)
Use advanced syntax patterns only when they satisfy the policy in `docs/Decisions.md`.

- Adopt now:
  - extension traits for domain semantics
  - scope guards (RAII) for context push/pop
  - pattern matching compression when explicit and exhaustive
  - newtype wrappers for invariants and phase IDs
  - display impls for diagnostics and source-like dumps
  - generic fixpoint helpers for monotone analysis
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
Use code compression only when it does not reduce clarity, e.g. instead of defining each id manually:

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct VarId(u32);

Generate the boilerplate once:
```
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
When appropriate, add expressive "demo scene" style only if it preserves readability and follows the gating policy above, e.g.
```
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
maintainability. Others are deferred/avoided in v0/v1 per `docs/Decisions.md`.
Example of NEWTYPE + DEREF:
```
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

## Mentors
If you have doubts, sometimes you'll need to interpolate, be creative and decide if your code is appropriate. You can refer to styles of other great rust codebases: ripgrep, rust-analyzer, cranelift, ruff, etc.

## Pass Contracts

Every compiler pass must document a short contract. Keep it concise and explicit.

### Required core fields (all passes)

- Inputs
    - Exact IR/state consumed (for example: `Typed`, side tables, target config).
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


## Test extensively!
Especially with oracle testing where possible. Test should be in a separate /tests folder, not in files inside src/
