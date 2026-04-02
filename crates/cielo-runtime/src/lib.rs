//! Runtime control-flow lowering.
//!
//! This crate owns the mandatory transition from residual Core to the linear
//! runtime IR and then to CFG. It contains no Salsa or backend code.

pub mod cfg_lower;
pub mod linearize;
