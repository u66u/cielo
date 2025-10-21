use std::collections::HashMap;

use crate::common::ids::FuncId;
use crate::pipeline::phases::{MonomorphizationSummary, Monomorphized, Typed};

pub fn run(typed: Typed) -> Monomorphized {
    // v0: no generic instantiation yet. We still build an explicit summary table
    // so downstream passes can depend on a stable monomorphization interface.
    let mut source_to_mono: HashMap<FuncId, Vec<FuncId>> = HashMap::new();
    for idx in 0..typed.program.functions().len() {
        let id = FuncId::new(idx);
        source_to_mono.insert(id, vec![id]);
    }

    typed.into_monomorphized(MonomorphizationSummary { source_to_mono })
}
