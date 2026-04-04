use std::collections::HashMap;

// Pass 3/9: monomorphize (v0 skeleton)
//
// Inputs:
// - Typed Core program
//
// Outputs:
// - Monomorphized wrapper with source->mono summary table
//
// Invariants:
// - No IR mutation in v0 (identity mapping)
// - Every function maps to at least one monomorphized instance (itself)
//
// Diagnostics:
// - None in v0
//
// Complexity:
// - O(function_count)

use cielo_base::FuncId;
use crate::pipeline::phases::{MonomorphizationSummary, Monomorphized, Typed};

pub fn run(typed: Typed) -> Monomorphized {
    // v0: no generic instantiation yet. We still build an explicit summary table
    // so downstream passes can depend on a stable monomorphization interface.
    let mut source_to_mono: HashMap<FuncId, Vec<FuncId>> = HashMap::new();
    for idx in 0..typed.program().functions().len() {
        let id = FuncId::new(idx);
        source_to_mono.insert(id, vec![id]);
    }

    typed.into_monomorphized(MonomorphizationSummary { source_to_mono })
}
