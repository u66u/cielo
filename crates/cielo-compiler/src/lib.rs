mod passes;
mod pipeline;

pub use cielo_memory::{GcConfig, GcFeatureFlags, GcMode, GcPreset};
pub use pipeline::compiler::{
    CompiledC, Compiler, CompilerConfig, Endianness, TargetSpec, V0PipelineTimings,
};

pub const RUNTIME_HEADER: &str = cielo_backend_c::RUNTIME_HEADER;

pub const CIELO_VERSION: &str = env!("CARGO_PKG_VERSION");
