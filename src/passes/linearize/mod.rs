// Pass 8/9: linearize (Residual Core -> linear runtime IR)
//
// Inputs:
// - Residualized Core program
//
// Outputs:
// - Arena-backed LinearProgram with explicit expression/statement ids
//
// Invariants:
// - Variable identities are preserved
// - Function ids are remapped densely after reachability pruning
// - Core node sharing is preserved via memoized ID mapping
//
// Diagnostics:
// - `LINEARIZE_UNKNOWN_CALLEE` when a call target cannot be resolved
// - `LINEARIZE_UNKNOWN_HANDLER` when a handler id cannot be resolved
//
// Complexity:
// - O(expr_count + stmt_count)

mod analysis;
mod lowering;
mod types;

use crate::ir::linear::LinearProgram;
use crate::pipeline::phases::Residualized;

pub fn run(mut residual: Residualized) -> Linearized {
    let sema = residual.sema().clone();
    let linear = {
        let (program, diagnostics) = residual.program_and_diagnostics_mut();
        lowering::lower_program(program, &sema, diagnostics)
    };
    Linearized { residual, linear }
}

#[derive(Clone, Debug)]
pub struct Linearized {
    pub residual: Residualized,
    pub linear: LinearProgram,
}
