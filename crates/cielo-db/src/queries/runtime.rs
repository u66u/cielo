use std::sync::Arc;

use cielo_runtime::{assemble_program, cfg_lower, linearize};

use crate::{Db, LinearFile, RuntimeFile, SourceFile, TargetProfile, staged_file};

#[salsa::tracked(no_eq, returns(clone))]
pub fn linear_file(db: &dyn Db, source: SourceFile, target: TargetProfile) -> Arc<LinearFile> {
    let staged = staged_file(db, source, target);
    let mut residual = cielo_staging::passes::normalize::run(staged.residual.clone());
    let sema = residual.sema().clone();
    let linear = {
        let (program, diagnostics) = residual.program_and_diagnostics_mut();
        linearize::run(program, &sema, diagnostics)
    };
    Arc::new(LinearFile {
        source: staged.source,
        residual,
        linear,
        interner: staged.interner.clone(),
    })
}

#[salsa::tracked(no_eq, returns(clone))]
pub fn runtime_file(db: &dyn Db, source: SourceFile, target: TargetProfile) -> Arc<RuntimeFile> {
    let linear = linear_file(db, source, target);
    let cfg = cfg_lower::run(&linear.linear);
    let runtime = assemble_program(
        cfg,
        &linear.linear,
        linear.residual.sema(),
        linear.residual.residual().constant_table.clone(),
        linear.residual.diagnostics().clone(),
    );
    Arc::new(RuntimeFile {
        source: linear.source,
        runtime,
        interner: linear.interner.clone(),
    })
}
