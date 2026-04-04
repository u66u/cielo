//! Semantic analysis and facts for Core programs.

use cielo_base::DiagnosticBag;
use cielo_ir::core::CoreProgram;

pub mod facts;
pub mod ownership;
pub mod ty;
pub mod typecheck;

pub use facts::SemanticTables;
pub use ownership::OwnershipClass;
pub use ty::{Persistability, TypeKind, TypeStore};

#[derive(Clone, Debug)]
pub struct TypedCore {
    program: CoreProgram,
    diagnostics: DiagnosticBag,
    facts: SemanticTables,
}

impl TypedCore {
    pub fn new(
        program: CoreProgram,
        diagnostics: DiagnosticBag,
        facts: SemanticTables,
    ) -> Self {
        Self {
            program,
            diagnostics,
            facts,
        }
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

pub fn check_core(program: CoreProgram, mut diagnostics: DiagnosticBag) -> TypedCore {
    let facts = typecheck::typecheck_core(&program, &mut diagnostics);
    TypedCore::new(program, diagnostics, facts)
}
