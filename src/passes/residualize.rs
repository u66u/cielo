// Pass 6/8: residualize (v0 identity residualization)
//
// Inputs:
// - BTA-classified program
//
// Outputs:
// - Residualized wrapper + residual side tables
//
// Invariants:
// - Program IR is preserved in v0
// - Residual metadata object always exists
//
// Diagnostics:
// - None in v0
//
// Complexity:
// - O(1) in v0 (wrapper construction only)

use crate::pipeline::phases::{BtaClassified, ResidualTables, Residualized};

pub fn run(bta: BtaClassified) -> Residualized {
    // v0: residualization is identity for now; CT values are recorded in side tables.
    bta.into_residualized(ResidualTables::default())
}
