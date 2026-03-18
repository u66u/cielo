//! Stable, low-level compiler vocabulary.
//!
//! This crate deliberately does not know about a compiler phase, an IR
//! builder, Salsa, or a backend.  It is the bottom of the dependency graph.

pub mod densemap;
pub mod diagnostics;
pub mod fixpoint;
pub mod ids;
pub mod reporting;
pub mod span;
pub mod symbols;

pub use diagnostics::{Diagnostic, DiagnosticBag, ErrorNode, Severity};
pub use ids::*;
pub use span::Span;
pub use symbols::Interner;
