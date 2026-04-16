use std::sync::Arc;

use cielo_runtime::{assemble_program, cfg_lower, linearize};

use crate::{Db, LinearFile, RuntimeFile, SourceFile, TargetProfile, staged_file};

#[salsa::tracked(no_eq, returns(clone))]
pub fn linear_file(db: &dyn Db, source: SourceFile, target: TargetProfile) -> Arc<LinearFile> {
    let staged = staged_file(db, source, target);
    let mut staged_core = cielo_staging::passes::normalize::run(staged.staged.clone());
    let sema = staged_core.sema().clone();
    let linear = {
        let (program, diagnostics) = staged_core.program_and_diagnostics_mut();
        linearize::run(program, &sema, diagnostics)
    };
    Arc::new(LinearFile {
        source: staged.source,
        staged: staged_core,
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
        linear.staged.sema(),
        linear.staged.facts().constant_table.clone(),
        linear.staged.diagnostics().clone(),
    );
    Arc::new(RuntimeFile {
        source: linear.source,
        runtime,
        interner: linear.interner.clone(),
    })
}
