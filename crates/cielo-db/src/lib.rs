//! Salsa orchestration for the real compiler pipeline.
//!
//! Query bodies only compose ordinary Rust passes. The passes do not receive a
//! database handle, and the database does not contain mutable IR builders.

use std::sync::Arc;

mod database;
mod inputs;

pub use database::{CieloDatabase, Db, QueryEvent, QueryMemoryStats};
pub use inputs::{CompileProfile, SourceFile, TargetProfile};

use cielo_backend_c as backend_c;
use cielo_base::{DiagnosticBag, Interner, SourceId};
use cielo_frontend::{ast::Program, parser::parse_source};
use cielo_ir::{linear::LinearProgram, target::TargetSpec};
use cielo_lowering::{LowerConfig, LowerOutput, TargetBuiltinSymbols, lower_program};
use cielo_memory::{GcConfig, MemoryInput, MemoryProgram};
use cielo_runtime::{assemble_program, cfg_lower, linearize};
use cielo_sema::{TypedCore, check_core};
use cielo_staging::{
    passes::{comptime, monomorphize, normalize},
    pipeline::phases::{BtaClassified, Monomorphized, Residualized},
};

macro_rules! file_artifact {
    ($(#[$meta:meta])* $name:ident { $($field:ident: $ty:ty),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Debug)]
        pub struct $name {
            pub source: SourceId,
            $(pub $field: $ty,)+
            pub interner: Interner,
        }
    };
}

file_artifact!(
    /// Parsed syntax and diagnostics for one source file.
    ParsedFile {
    path: String,
    ast: Program,
    diagnostics: DiagnosticBag,
    }
);

file_artifact!(
    /// Core lowering output for one source file.
    CoreFile { core: LowerOutput }
);
file_artifact!(
    /// Typechecked Core and facts for one source file.
    TypedFile { typed: TypedCore }
);
file_artifact!(
    /// Core after concrete function instances have been created.
    MonomorphizedFile { mono: Monomorphized }
);
file_artifact!(
    /// Monomorphized Core with comptime/runtime classifications.
    ClassifiedFile { classified: BtaClassified }
);
file_artifact!(
    /// Runtime-only Core after residualization and specialization.
    StagedFile { residual: Residualized }
);
file_artifact!(
    /// Normalized runtime Core and its Linear IR.
    LinearFile {
        residual: Residualized,
        linear: LinearProgram,
    }
);
file_artifact!(
    /// Self-contained input to memory lowering and backend emission.
    RuntimeFile { runtime: cielo_ir::runtime::RuntimeProgram }
);

#[derive(Clone, Debug)]
pub struct MemoryFile {
    pub runtime: Arc<RuntimeFile>,
    pub memory: MemoryProgram,
}

#[derive(Clone, Debug)]
pub struct EmittedFile {
    pub memory: Arc<MemoryFile>,
    pub c_source: String,
}

#[salsa::tracked(no_eq, returns(clone))]
pub fn parsed_file(db: &dyn Db, source: SourceFile) -> Arc<ParsedFile> {
    let source_id = SourceId::from_u32(source.source_id(db));
    let mut interner = Interner::new();
    let parsed = parse_source(source.text(db), source_id, &mut interner);
    Arc::new(ParsedFile {
        source: source_id,
        path: source.path(db),
        ast: parsed.program,
        diagnostics: parsed.diagnostics,
        interner,
    })
}

#[salsa::tracked(no_eq, returns(clone))]
pub fn core_file(db: &dyn Db, source: SourceFile, target: TargetProfile) -> Arc<CoreFile> {
    let parsed = parsed_file(db, source);
    let mut interner = parsed.interner.clone();
    let main = interner.intern("main");
    let builtins = TargetBuiltinSymbols::intern(&mut interner);
    let lowered = lower_program(
        &parsed.ast,
        LowerConfig::with_entrypoint(main).with_target_builtins(TargetSpec::from(target), builtins),
    );
    let mut diagnostics = parsed.diagnostics.clone();
    diagnostics.extend(lowered.diagnostics);
    Arc::new(CoreFile {
        source: parsed.source,
        core: LowerOutput::new(lowered.program, diagnostics),
        interner,
    })
}

#[salsa::tracked(no_eq, returns(clone))]
pub fn typed_file(db: &dyn Db, source: SourceFile, target: TargetProfile) -> Arc<TypedFile> {
    let core = core_file(db, source, target);
    let (program, diagnostics) = core.core.clone().into_parts();
    Arc::new(TypedFile {
        source: core.source,
        typed: check_core(program, diagnostics),
        interner: core.interner.clone(),
    })
}

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
