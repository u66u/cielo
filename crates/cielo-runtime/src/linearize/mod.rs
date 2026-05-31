// Pass 8/9: linearize (Residual Core -> linear runtime IR)
//
// Inputs:
// - Staged runtime Core program
//
// Outputs:
// - Arena-backed LinearProgram with explicit expression/statement ids
//
// Invariants:
// - Variable identities are preserved
// - Function ids are remapped densely after reachability pruning
// - Core node sharing is preserved via memoized ID mapping
// - An erased `resume` continues with the whole rest of the handled body, not
//   with the rest of the block the `perform` sat in
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

use cielo_base::diagnostics::DiagnosticBag;
use cielo_ir::core::CoreProgram;
use cielo_ir::linear::LinearProgram;
use cielo_sema::SemanticTables;

pub fn run(
    program: &CoreProgram,
    sema: &SemanticTables,
    diagnostics: &mut DiagnosticBag,
) -> LinearProgram {
    lowering::lower_program(program, sema, diagnostics)
}
