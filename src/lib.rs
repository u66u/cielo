pub mod analysis;
pub mod common;
pub mod frontend;
pub mod ir;
pub mod passes;
pub mod pipeline;
pub mod sema;

pub use common::gc::{GcConfig, GcFeatureFlags, GcMode, GcPreset};
pub use pipeline::compiler::{CompiledC, Compiler, CompilerConfig, Endianness, TargetSpec};

pub const CIELO_VERSION: &str = env!("CARGO_PKG_VERSION");
