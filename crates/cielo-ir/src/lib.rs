//! Intermediate representations owned independently from compiler algorithms.

pub mod builtins;
pub mod cfg;
pub mod constants;
pub mod core;
pub mod effect;
pub mod function_graph;
pub mod linear;
pub mod ownership;
pub mod region;
pub mod runtime;
pub mod target;
pub mod walk;

pub use core::CoreProgram;
