//! Salsa orchestration for the compiler's vertical query graph.
//!
//! Query modules compose ordinary Rust passes. The database does not own
//! mutable IR builders and pass implementations never receive `&dyn Db`.

mod artifacts;
mod database;
mod inputs;
mod queries;

pub use artifacts::*;
pub use database::{CieloDatabase, Db, QueryEvent, QueryMemoryStats};
pub use inputs::{CompileProfile, SourceFile, TargetProfile};
pub use queries::*;
