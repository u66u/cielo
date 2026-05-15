//! Semantic analysis and facts for Core programs.

use std::sync::Arc;

use cielo_base::{DiagnosticBag, Interner};
use cielo_ir::core::CoreProgram;

pub mod facts;
pub mod ownership;
pub mod ty;
pub mod typecheck;

pub use facts::SemanticTables;
pub use ty::{Persistability, TypeKind, TypeStore};

#[derive(Clone, Debug)]
pub struct TypedCore {
    program: CoreProgram,
    diagnostics: DiagnosticBag,
    facts: SemanticTables,
    names: Option<Arc<Interner>>,
}

impl TypedCore {
    pub fn new(
        program: CoreProgram,
        diagnostics: DiagnosticBag,
        facts: SemanticTables,
        names: Option<Arc<Interner>>,
    ) -> Self {
        Self {
            program,
            diagnostics,
            facts,
            names,
        }
    }

    /// Carried so a pass that re-typechecks the rewritten program keeps naming
    /// types the way the original check did. Diagnostic text only.
    pub fn names(&self) -> Option<Arc<Interner>> {
        self.names.clone()
    }

    pub fn program(&self) -> &CoreProgram {
        &self.program
    }

    pub fn diagnostics(&self) -> &DiagnosticBag {
        &self.diagnostics
    }

    pub fn facts(&self) -> &SemanticTables {
        &self.facts
    }

    pub fn into_parts(self) -> (CoreProgram, DiagnosticBag, SemanticTables) {
        (self.program, self.diagnostics, self.facts)
    }
}

/// `names` is optional because Core is self-contained: a caller that has no
/// interner still typechecks, it just gets symbol ids in the diagnostic text.
pub fn check_core(
    program: CoreProgram,
    mut diagnostics: DiagnosticBag,
    names: Option<Arc<Interner>>,
) -> TypedCore {
    let facts = typecheck::typecheck_core(&program, &mut diagnostics, names.as_deref());
    TypedCore::new(program, diagnostics, facts, names)
}
