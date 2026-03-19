//! Source-facing compiler components.

pub mod ast;
pub mod lexer;
pub mod parser;

pub use ast::Program;
pub use parser::{ParseOutput, parse_source};
