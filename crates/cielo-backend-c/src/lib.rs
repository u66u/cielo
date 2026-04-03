//! C backend for the managed runtime CFG.

pub mod c_constants;
pub mod cfg_codegen;

pub use cfg_codegen::emit;

pub const RUNTIME_HEADER: &str = include_str!("cielo_runtime.h");
