pub mod analysis;
pub mod common;
pub mod frontend;
pub mod ir;
pub mod passes;
pub mod pipeline;
pub mod sema;

pub use pipeline::compiler::{CompiledC, Compiler, CompilerConfig, Endianness, TargetSpec};
