//! Implemented memory management for runtime CFGs.
//!
//! Reference counting is the only managed strategy currently implemented.
//! Add a strategy dispatcher only when a second implementation exists.

pub mod config;
pub mod refcount;

pub use config::{GcConfig, GcFeatureFlags, GcMode, GcPreset};
pub use refcount::{ArcStats, MemoryInput, MemoryProgram, MemoryReport, lower};

// Future memory strategies belong beside `refcount`, with their own inputs,
// analyses, and result types. They should not be added to configuration until
// they can lower a real runtime CFG.
