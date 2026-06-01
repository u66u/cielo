//! C backend for the managed runtime CFG.

pub mod c_constants;
pub mod cfg_codegen;
mod structure;
mod trampoline;

pub use cfg_codegen::{EMITTED_BODIES_MARKER, emit};

pub const RUNTIME_HEADER: &str = include_str!("cielo_runtime.h");
