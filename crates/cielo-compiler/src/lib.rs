pub mod analysis;
pub mod common;
pub mod frontend;
pub mod ir;
pub mod passes;
pub mod pipeline;
pub mod sema;

pub use cielo_memory::{GcConfig, GcFeatureFlags, GcMode, GcPreset};
pub use pipeline::compiler::{CompiledC, Compiler, CompilerConfig, Endianness, TargetSpec};

pub const RUNTIME_HEADER: &str = cielo_backend_c::RUNTIME_HEADER;

pub const CIELO_VERSION: &str = env!("CARGO_PKG_VERSION");
