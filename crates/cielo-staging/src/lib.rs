//! Comptime, staging, and residualization passes.
//!
//! This crate owns the phase products consumed by those passes. It depends on
//! the frontend, Core IR, and semantic facts through their public contracts;
//! it does not mirror their module trees.

pub mod passes;
pub mod pipeline;

pub use pipeline::phases::*;

pub const CIELO_VERSION: &str = env!("CARGO_PKG_VERSION");
