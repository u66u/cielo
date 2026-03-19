//! Small, immutable products shared by compiler stages.
//!
//! These are intentionally summaries in the first migration slice.  They give
//! the database and strategy layers a stable contract while the existing dense
//! Core/Linear/CFG arenas are moved behind it in later slices.

use cielo_base::SourceId;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ModuleFacts {
    pub source: SourceId,
    pub bytes: usize,
    pub items: usize,
    pub functions: usize,
    pub effects: usize,
    pub diagnostics: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypedCore {
    pub module: ModuleFacts,
    pub declarations: usize,
    pub callable_effects: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedCore {
    pub typed: TypedCore,
    pub compile_time_functions: usize,
    pub residual_items: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeModule {
    pub staged: StagedCore,
    pub allocations: usize,
    pub calls: usize,
    pub suspension_points: usize,
}
