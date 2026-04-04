//! Salsa orchestration for the real compiler pipeline.
//!
//! Query bodies only compose ordinary Rust passes. The passes do not receive a
//! database handle, and the database does not contain mutable IR builders.

use std::sync::{Arc, Mutex};

use cielo_backend_c as backend_c;
use cielo_base::{DiagnosticBag, Interner, SourceId};
use cielo_frontend::{ast::Program, parser::parse_source};
use cielo_ir::{
    cfg::CfgProgram,
    linear::LinearProgram,
    target::{Endianness, TargetSpec},
};
use cielo_lowering::{LowerConfig, LowerOutput, TargetBuiltinSymbols, lower_program};
use cielo_memory::{GcConfig, MemoryInput, MemoryProgram};
use cielo_runtime::{cfg_lower, linearize};
use cielo_sema::{TypedCore, check_core};
use cielo_staging::{
    passes::{comptime, monomorphize, normalize},
    pipeline::phases::Residualized,
};

#[salsa::db]
pub trait Db: salsa::Database {}

#[derive(Clone, Debug, Default)]
pub struct QueryEvent {
    pub description: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueryMemoryStats {
    pub query: String,
    pub entries: usize,
    pub metadata_bytes: usize,
    pub field_bytes: usize,
    pub heap_bytes: Option<usize>,
}

#[salsa::db]
#[derive(Clone)]
pub struct CieloDatabase {
    storage: salsa::Storage<Self>,
    events: Arc<Mutex<Vec<QueryEvent>>>,
}

impl Default for CieloDatabase {
    fn default() -> Self {
        let events = Arc::new(Mutex::new(Vec::new()));
        let callback_events = Arc::clone(&events);
        let callback = Box::new(move |event: salsa::Event| {
            if matches!(&event.kind, salsa::EventKind::WillExecute { .. }) {
                callback_events
                    .lock()
                    .expect("Salsa event log mutex poisoned")
                    .push(QueryEvent {
                        description: format!("{event:?}"),
                    });
            }
        });
        Self {
            storage: salsa::Storage::new(Some(callback)),
            events,
        }
    }
}

impl CieloDatabase {
    pub fn take_query_events(&self) -> Vec<QueryEvent> {
        std::mem::take(&mut *self.events.lock().expect("Salsa event log mutex poisoned"))
    }

    pub fn query_memory_stats(&self) -> Vec<QueryMemoryStats> {
        let info = (self as &dyn salsa::Database).memory_usage();
        let mut stats = info
            .queries
            .values()
            .map(|entry| QueryMemoryStats {
                query: entry.debug_name().to_owned(),
                entries: entry.count(),
                metadata_bytes: entry.size_of_metadata(),
                field_bytes: entry.size_of_fields(),
                heap_bytes: entry.heap_size_of_fields(),
            })
            .collect::<Vec<_>>();
        stats.sort_by(|lhs, rhs| lhs.query.cmp(&rhs.query));
        stats
    }
}

#[salsa::db]
impl salsa::Database for CieloDatabase {}

#[salsa::db]
impl Db for CieloDatabase {}

#[salsa::input]
#[derive(Debug)]
pub struct SourceFile {
    #[returns(copy)]
    pub source_id: u32,
    #[returns(clone)]
    pub path: String,
    #[returns(deref)]
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TargetProfile {
    pub word_size_bits: u8,
    pub endianness: Endianness,
    pub pointer_alignment: u8,
}

impl Default for TargetProfile {
    fn default() -> Self {
        Self::from(TargetSpec::default())
    }
}

impl From<TargetSpec> for TargetProfile {
    fn from(target: TargetSpec) -> Self {
        Self {
            word_size_bits: target.word_size_bits,
            endianness: target.endianness,
            pointer_alignment: target.pointer_alignment,
        }
    }
}

impl From<TargetProfile> for TargetSpec {
    fn from(target: TargetProfile) -> Self {
        Self {
            word_size_bits: target.word_size_bits,
            endianness: target.endianness,
            pointer_alignment: target.pointer_alignment,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CompileProfile {
    pub target: TargetProfile,
    pub gc: GcConfig,
}

impl Default for CompileProfile {
    fn default() -> Self {
        Self {
            target: TargetProfile::default(),
            gc: GcConfig::default(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ParsedFile {
    pub source: SourceId,
    pub path: String,
    pub ast: Program,
    pub diagnostics: DiagnosticBag,
    pub interner: Interner,
}

#[derive(Clone, Debug)]
pub struct CoreFile {
    pub source: SourceId,
    pub core: LowerOutput,
    pub interner: Interner,
}

#[derive(Clone, Debug)]
pub struct TypedFile {
    pub source: SourceId,
    pub typed: TypedCore,
    pub interner: Interner,
}

#[derive(Clone, Debug)]
pub struct StagedFile {
    pub source: SourceId,
    pub residual: Residualized,
    pub interner: Interner,
}

#[derive(Clone, Debug)]
pub struct RuntimeFile {
    pub source: SourceId,
    pub residual: Residualized,
    pub linear: LinearProgram,
    pub cfg: CfgProgram,
    pub interner: Interner,
}

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
pub fn core_file(
    db: &dyn Db,
    source: SourceFile,
    target: TargetProfile,
) -> Arc<CoreFile> {
    let parsed = parsed_file(db, source);
    let mut interner = parsed.interner.clone();
    let main = interner.intern("main");
    let builtins = TargetBuiltinSymbols::intern(&mut interner);
    let lowered = lower_program(
        &parsed.ast,
        LowerConfig::with_entrypoint(main)
            .with_target_builtins(TargetSpec::from(target), builtins),
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
pub fn typed_file(
    db: &dyn Db,
    source: SourceFile,
    target: TargetProfile,
) -> Arc<TypedFile> {
    let core = core_file(db, source, target);
    let (program, diagnostics) = core.core.clone().into_parts();
    Arc::new(TypedFile {
        source: core.source,
        typed: check_core(program, diagnostics),
        interner: core.interner.clone(),
    })
}

#[salsa::tracked(no_eq, returns(clone))]
pub fn staged_file(
    db: &dyn Db,
    source: SourceFile,
    target: TargetProfile,
) -> Arc<StagedFile> {
    let typed = typed_file(db, source, target);
    let mono = monomorphize::run(typed.typed.clone());
    let classified = comptime::evaluate_classify(mono, TargetSpec::from(target));
    let residual = comptime::residualize_specialize(classified);
    Arc::new(StagedFile {
        source: typed.source,
        residual,
        interner: typed.interner.clone(),
    })
}

#[salsa::tracked(no_eq, returns(clone))]
pub fn runtime_file(
    db: &dyn Db,
    source: SourceFile,
    target: TargetProfile,
) -> Arc<RuntimeFile> {
    let staged = staged_file(db, source, target);
    let mut residual = normalize::run(staged.residual.clone());
    let sema = residual.sema().clone();
    let linear = {
        let (program, diagnostics) = residual.program_and_diagnostics_mut();
        linearize::run(program, &sema, diagnostics)
    };
    let cfg = cfg_lower::run(&linear);
    Arc::new(RuntimeFile {
        source: staged.source,
        residual,
        linear,
        cfg,
        interner: staged.interner.clone(),
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
            cfg: &runtime.cfg,
            core: runtime.residual.program(),
            sema: runtime.residual.sema(),
            diagnostics: runtime.residual.diagnostics(),
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
        &memory.runtime.residual.residual().constant_table,
        gc.arc_emit_trace_enabled(),
    );
    Arc::new(EmittedFile { memory, c_source })
}

pub fn compile(
    db: &dyn Db,
    source: SourceFile,
    profile: CompileProfile,
) -> Arc<EmittedFile> {
    emitted_file(db, source, profile.target, profile.gc)
}

pub fn compile_memory(
    db: &dyn Db,
    source: SourceFile,
    profile: CompileProfile,
) -> Arc<MemoryFile> {
    memory_file(db, source, profile.target, profile.gc)
}
