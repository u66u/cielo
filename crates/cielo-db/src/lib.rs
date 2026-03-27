//! Salsa composition layer for Cielo.
//!
//! Salsa lives here, at the edge of the compiler.  The parsing and lowering
//! functions used by the queries remain ordinary Rust code in their own
//! crates.  This keeps the database from becoming a service object that every
//! pass has to carry around.

use std::sync::{Arc, Mutex};

use cielo_backend_api::MachineModule;
use cielo_base::{DiagnosticBag, Interner, SourceId};
use cielo_frontend::ast::Item;
use cielo_frontend::parser::parse_source;
use cielo_ir::boundary::{ModuleFacts, RuntimeModule, StagedCore, TypedCore};
use cielo_ir::core::CoreProgram;
use cielo_ir::target::{Endianness, TargetSpec};
use cielo_lowering::{LowerConfig, TargetBuiltinSymbols, lower_program};
use cielo_memory::{
    MemoryInput, MemoryModule, RefcountModule, RefcountProfile, RegionModule, RegionProfile,
    TracingModule, TracingProfile,
};
use cielo_sema::facts::SemanticTables;
use cielo_sema::typecheck::typecheck_core;

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

/// Source is a Salsa input.  The compiler can create several of these in one
/// database and compile them under different profiles.
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SemanticsProfile {
    pub effects: bool,
    pub staging: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct StagingProfile {
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ControlProfile {
    pub concurrency: bool,
}

pub use cielo_memory::{MemoryModel, MemoryProfile};

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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct CompileProfiles {
    pub semantics: SemanticsProfile,
    pub staging: StagingProfile,
    pub control: ControlProfile,
    pub memory: MemoryProfile,
    pub target: TargetProfile,
}

#[derive(Clone, Debug)]
pub struct ParsedModule {
    pub source: SourceId,
    pub path: String,
    pub program: cielo_frontend::ast::Program,
    pub diagnostics: DiagnosticBag,
    pub interner: Interner,
}

#[derive(Clone, Debug)]
pub struct CoreModule {
    pub source: SourceId,
    pub program: CoreProgram,
    pub diagnostics: DiagnosticBag,
    pub interner: Interner,
}

#[derive(Clone, Debug)]
pub struct TypedCoreModule {
    pub core: Arc<CoreModule>,
    pub semantics: SemanticsProfile,
    pub facts: SemanticTables,
    pub diagnostics: DiagnosticBag,
}

impl ParsedModule {
    pub fn facts(&self, text_len: usize) -> ModuleFacts {
        let mut functions = 0;
        let mut effects = 0;
        for item in &self.program.items {
            match item {
                Item::Function(_) => functions += 1,
                Item::Effect(_) => effects += 1,
                Item::Struct(_) | Item::Enum(_) | Item::Error(_) => {}
            }
        }
        ModuleFacts {
            source: self.source,
            bytes: text_len,
            items: self.program.items.len(),
            functions,
            effects,
            diagnostics: self.diagnostics.entries().len(),
        }
    }
}

#[salsa::tracked(no_eq, returns(clone))]
pub fn parsed_module(db: &dyn Db, source: SourceFile) -> Arc<ParsedModule> {
    let mut interner = Interner::new();
    let parsed = parse_source(
        source.text(db),
        SourceId::from_u32(source.source_id(db)),
        &mut interner,
    );
    Arc::new(ParsedModule {
        source: SourceId::from_u32(source.source_id(db)),
        path: source.path(db),
        program: parsed.program,
        diagnostics: parsed.diagnostics,
        interner,
    })
}

#[salsa::tracked(no_eq, returns(clone))]
pub fn lowered_core(db: &dyn Db, source: SourceFile, target: TargetProfile) -> Arc<CoreModule> {
    let parsed = parsed_module(db, source);
    let mut interner = parsed.interner.clone();
    let main = interner.intern("main");
    let target_builtins = TargetBuiltinSymbols::intern(&mut interner);
    let lowered = lower_program(
        &parsed.program,
        LowerConfig::with_entrypoint(main)
            .with_target_builtins(TargetSpec::from(target), target_builtins),
    );
    let mut diagnostics = parsed.diagnostics.clone();
    diagnostics.extend(lowered.diagnostics);
    Arc::new(CoreModule {
        source: parsed.source,
        program: lowered.program,
        diagnostics,
        interner,
    })
}

#[salsa::tracked(no_eq, returns(clone))]
pub fn checked_core(
    db: &dyn Db,
    source: SourceFile,
    semantics: SemanticsProfile,
    target: TargetProfile,
) -> Arc<TypedCoreModule> {
    let core = lowered_core(db, source, target);
    let mut diagnostics = core.diagnostics.clone();
    let facts = typecheck_core(&core.program, &mut diagnostics);
    Arc::new(TypedCoreModule {
        core,
        semantics,
        facts,
        diagnostics,
    })
}

#[salsa::tracked(returns(clone))]
pub fn typed_module(
    db: &dyn Db,
    source: SourceFile,
    semantics: SemanticsProfile,
    target: TargetProfile,
) -> Arc<TypedCore> {
    let parsed = parsed_module(db, source);
    let checked = checked_core(db, source, semantics, target);
    let mut facts = parsed.facts(source.text(db).len());
    facts.functions = checked.core.program.functions().len();
    facts.effects = checked.core.program.effects().len();
    facts.diagnostics = checked.diagnostics.entries().len();
    Arc::new(TypedCore {
        module: facts,
        declarations: facts.items,
        callable_effects: if semantics.effects { facts.effects } else { 0 },
    })
}

#[salsa::tracked(returns(clone))]
pub fn staged_module(
    db: &dyn Db,
    source: SourceFile,
    semantics: SemanticsProfile,
    staging: StagingProfile,
    target: TargetProfile,
) -> Arc<StagedCore> {
    let typed = typed_module(db, source, semantics, target);
    Arc::new(StagedCore {
        residual_items: if staging.enabled {
            typed.declarations.saturating_sub(typed.module.effects)
        } else {
            typed.declarations
        },
        compile_time_functions: if staging.enabled {
            typed.module.functions / 2
        } else {
            0
        },
        typed: (*typed).clone(),
    })
}

#[salsa::tracked(returns(clone))]
pub fn runtime_module(
    db: &dyn Db,
    source: SourceFile,
    semantics: SemanticsProfile,
    staging: StagingProfile,
    control: ControlProfile,
    target: TargetProfile,
) -> Arc<RuntimeModule> {
    let staged = staged_module(db, source, semantics, staging, target);
    let functions = staged.typed.module.functions;
    Arc::new(RuntimeModule {
        allocations: staged.typed.module.items.saturating_sub(functions),
        calls: functions,
        suspension_points: if control.concurrency { functions } else { 0 },
        staged: (*staged).clone(),
    })
}

#[salsa::tracked(returns(clone))]
pub fn refcount_module(
    db: &dyn Db,
    source: SourceFile,
    semantics: SemanticsProfile,
    staging: StagingProfile,
    control: ControlProfile,
    profile: RefcountProfile,
    target: TargetProfile,
) -> Arc<RefcountModule> {
    let runtime = runtime_module(db, source, semantics, staging, control, target);
    Arc::new(cielo_memory::refcount::lower(
        MemoryInput { runtime: &runtime },
        profile,
    ))
}

#[salsa::tracked(returns(clone))]
pub fn tracing_module(
    db: &dyn Db,
    source: SourceFile,
    semantics: SemanticsProfile,
    staging: StagingProfile,
    control: ControlProfile,
    profile: TracingProfile,
    target: TargetProfile,
) -> Arc<TracingModule> {
    let runtime = runtime_module(db, source, semantics, staging, control, target);
    Arc::new(cielo_memory::tracing::lower(
        MemoryInput { runtime: &runtime },
        profile,
    ))
}

#[salsa::tracked(returns(clone))]
pub fn region_module(
    db: &dyn Db,
    source: SourceFile,
    semantics: SemanticsProfile,
    staging: StagingProfile,
    control: ControlProfile,
    profile: RegionProfile,
    target: TargetProfile,
) -> Arc<RegionModule> {
    let runtime = runtime_module(db, source, semantics, staging, control, target);
    Arc::new(cielo_memory::regions::lower(
        MemoryInput { runtime: &runtime },
        profile,
    ))
}

/// Strategy dispatch is intentionally ordinary Rust.  The selected branch is
/// the only branch whose strategy-specific work is requested.
#[salsa::tracked(returns(clone))]
pub fn memory_module(
    db: &dyn Db,
    source: SourceFile,
    semantics: SemanticsProfile,
    staging: StagingProfile,
    control: ControlProfile,
    memory: MemoryProfile,
    target: TargetProfile,
) -> Arc<MemoryModule> {
    let result = match memory.model {
        MemoryModel::ReferenceCounting => MemoryModule::ReferenceCounting(
            (*refcount_module(
                db,
                source,
                semantics,
                staging,
                control,
                memory.refcount,
                target,
            ))
            .clone(),
        ),
        MemoryModel::Tracing => MemoryModule::Tracing(
            (*tracing_module(
                db,
                source,
                semantics,
                staging,
                control,
                memory.tracing,
                target,
            ))
            .clone(),
        ),
        MemoryModel::Regions => MemoryModule::Regions(
            (*region_module(
                db,
                source,
                semantics,
                staging,
                control,
                memory.regions,
                target,
            ))
            .clone(),
        ),
    };
    Arc::new(result)
}

#[salsa::tracked(returns(clone))]
pub fn machine_module(
    db: &dyn Db,
    source: SourceFile,
    semantics: SemanticsProfile,
    staging: StagingProfile,
    control: ControlProfile,
    memory: MemoryProfile,
    target: TargetProfile,
) -> Arc<MachineModule> {
    let memory = memory_module(db, source, semantics, staging, control, memory, target);
    let memory_operations = match memory.as_ref() {
        MemoryModule::ReferenceCounting(module) => module.retain_release_sites,
        MemoryModule::Tracing(module) => module.safepoints + module.root_maps,
        MemoryModule::Regions(module) => module.constraints,
    };
    Arc::new(MachineModule {
        runtime: memory.manifest().clone(),
        memory_operations,
        word_size_bits: target.word_size_bits,
    })
}

pub fn compile_memory(
    db: &dyn Db,
    source: SourceFile,
    profiles: CompileProfiles,
) -> Arc<MemoryModule> {
    memory_module(
        db,
        source,
        profiles.semantics,
        profiles.staging,
        profiles.control,
        profiles.memory,
        profiles.target,
    )
}

pub fn compile(db: &dyn Db, source: SourceFile, profiles: CompileProfiles) -> Arc<MachineModule> {
    machine_module(
        db,
        source,
        profiles.semantics,
        profiles.staging,
        profiles.control,
        profiles.memory,
        profiles.target,
    )
}
