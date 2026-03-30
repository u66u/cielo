pub mod analysis;
pub mod common;
pub mod frontend;
pub mod ir;
pub mod passes;
pub mod pipeline;
pub mod sema;

pub use cielo_memory::{MemoryModel, RefcountAlgorithm, RegionAlgorithm, TracingCollector};
pub use common::gc::{GcConfig, GcFeatureFlags, GcMode, GcPreset};
pub use pipeline::compiler::{CompiledC, Compiler, CompilerConfig, Endianness, TargetSpec};

pub const RUNTIME_HEADER: &str = cielo_backend_c::RUNTIME_HEADER;

pub use cielo_backend_c::capabilities as c_backend_capabilities;

pub const CIELO_VERSION: &str = env!("CARGO_PKG_VERSION");
