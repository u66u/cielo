//! Public compiler facade.

mod facade;

pub use cielo_memory::{
    ArcConfig, ArcFeatures, MemoryPreset, MemoryProfile, MemoryReport, MemoryStrategy,
};
pub use facade::{Compiler, CompilerConfig, Endianness, TargetSpec};

pub const RUNTIME_HEADER: &str = cielo_backend_c::RUNTIME_HEADER;
pub const CIELO_VERSION: &str = env!("CARGO_PKG_VERSION");
