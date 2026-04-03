//! Comptime, staging, and residualization passes.
//!
//! This crate owns the phase products consumed by those passes.  The
//! compatibility modules mirror the old `cielo` paths while callers migrate
//! to these direct APIs.

pub mod analysis;
pub mod common;
pub mod frontend;
pub mod ir;
pub mod passes;
pub mod pipeline;
pub mod sema;

pub use pipeline::phases::*;

pub const CIELO_VERSION: &str = env!("CARGO_PKG_VERSION");
