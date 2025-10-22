use crate::pipeline::phases::{BtaClassified, ResidualTables, Residualized};

pub fn run(bta: BtaClassified) -> Residualized {
    // v0: residualization is identity for now; CT values are recorded in side tables.
    bta.into_residualized(ResidualTables::default())
}
