//! Intermediate representations owned independently from compiler algorithms.

pub mod boundary;
pub mod cfg;
pub mod constants;
pub mod core;
pub mod effect;
pub mod function_graph;
pub mod linear;
pub mod target;
pub mod walk;

// Compatibility namespace for the compiler crate while imports migrate to the
// public modules above.
pub mod ir {
    pub use crate::cfg;
    pub use crate::core;
    pub use crate::linear;
}

pub use core::CoreProgram;
