//! Salsa orchestration for the real compiler pipeline.
//!
//! Query bodies only compose ordinary Rust passes. The passes do not receive a
//! database handle, and the database does not contain mutable IR builders.

use std::sync::Arc;

mod artifacts;
mod database;
mod inputs;
mod queries;

pub use artifacts::*;
pub use database::{CieloDatabase, Db, QueryEvent, QueryMemoryStats};
pub use inputs::{CompileProfile, SourceFile, TargetProfile};
pub use queries::*;

use cielo_backend_c as backend_c;
use cielo_ir::target::TargetSpec;
use cielo_memory::{GcConfig, MemoryInput};
use cielo_runtime::{assemble_program, cfg_lower, linearize};
use cielo_staging::passes::{comptime, monomorphize, normalize};

#[salsa::tracked(no_eq, returns(clone))]
pub fn monomorphized_file(
    db: &dyn Db,
    source: SourceFile,
    target: TargetProfile,
) -> Arc<MonomorphizedFile> {
    let typed = typed_file(db, source, target);
    let mono = monomorphize::run(typed.typed.clone());
    Arc::new(MonomorphizedFile {
        source: typed.source,
        mono,
        interner: typed.interner.clone(),
    })
}

#[salsa::tracked(no_eq, returns(clone))]
pub fn classified_file(
    db: &dyn Db,
    source: SourceFile,
    target: TargetProfile,
) -> Arc<ClassifiedFile> {
    let mono = monomorphized_file(db, source, target);
    let classified = comptime::evaluate_classify(mono.mono.clone(), TargetSpec::from(target));
    Arc::new(ClassifiedFile {
        source: mono.source,
        classified,
        interner: mono.interner.clone(),
    })
}

#[salsa::tracked(no_eq, returns(clone))]
pub fn staged_file(db: &dyn Db, source: SourceFile, target: TargetProfile) -> Arc<StagedFile> {
    let classified = classified_file(db, source, target);
    let residual = comptime::residualize_specialize(classified.classified.clone());
    Arc::new(StagedFile {
        source: classified.source,
        residual,
        interner: classified.interner.clone(),
    })
}

#[salsa::tracked(no_eq, returns(clone))]
pub fn linear_file(db: &dyn Db, source: SourceFile, target: TargetProfile) -> Arc<LinearFile> {
    let staged = staged_file(db, source, target);
    let mut residual = normalize::run(staged.residual.clone());
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

#[salsa::tracked(no_eq, returns(clone))]
pub fn memory_file(
    db: &dyn Db,
    source: SourceFile,
    target: TargetProfile,
    gc: GcConfig,
) -> Arc<MemoryFile> {
    let runtime = runtime_file(db, source, target);
    let memory = cielo_memory::lower(
        MemoryInput {
            runtime: &runtime.runtime,
        },
        gc,
    );
    Arc::new(MemoryFile { runtime, memory })
}

#[salsa::tracked(no_eq, returns(clone))]
pub fn emitted_file(
    db: &dyn Db,
    source: SourceFile,
    target: TargetProfile,
    gc: GcConfig,
) -> Arc<EmittedFile> {
    let memory = memory_file(db, source, target, gc);
    let c_source = backend_c::emit(
        &memory.memory.cfg,
        &memory.runtime.interner,
        &memory.runtime.runtime.constants,
        memory.memory.emit_arc_trace_comments,
    );
    Arc::new(EmittedFile { memory, c_source })
}

pub fn compile(db: &dyn Db, source: SourceFile, profile: CompileProfile) -> Arc<EmittedFile> {
    emitted_file(db, source, profile.target, profile.gc)
}

pub fn compile_memory(db: &dyn Db, source: SourceFile, profile: CompileProfile) -> Arc<MemoryFile> {
    memory_file(db, source, profile.target, profile.gc)
}
