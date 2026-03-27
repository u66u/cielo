use cielo_backend_api::RuntimeRequirement;
use cielo_db::{CieloDatabase, CompileProfiles, MemoryModel, SourceFile, compile, compile_memory};
use cielo_memory::{MemoryModule, TracingCollector};
use salsa::Setter;

#[test]
fn source_changes_flow_through_coarse_queries() {
    let mut db = CieloDatabase::default();
    let source = SourceFile::new(&db, 0, "memory.cielo".to_owned(), "fn main() {}".to_owned());
    let profiles = CompileProfiles::default();
    let first = compile(&db, source, profiles);
    let first_events = db.take_query_events();
    assert!(!first_events.is_empty());
    assert_eq!(first.word_size_bits, 64);
    assert!(
        first
            .runtime
            .requirements()
            .contains(&RuntimeRequirement::ReferenceCounting)
    );

    let _cached = compile(&db, source, profiles);
    let cached_events = db.take_query_events();
    assert!(cached_events.is_empty());

    source
        .set_text(&mut db)
        .to("fn main() {}\nfn helper() {}".to_owned());
    let _second = compile(&db, source, profiles);
    let changed_events = db.take_query_events();
    assert!(
        changed_events
            .iter()
            .any(|event| event.description.contains("parsed_module"))
    );
}

#[test]
fn strategy_selection_changes_only_the_memory_product() {
    let db = CieloDatabase::default();
    let source = SourceFile::new(&db, 0, "memory.cielo".to_owned(), "fn main() {}".to_owned());
    let mut profiles = CompileProfiles::default();
    let rc = compile_memory(&db, source, profiles);
    let _ = db.take_query_events();
    profiles.memory.model = MemoryModel::Tracing;
    let tracing = compile_memory(&db, source, profiles);
    let strategy_events = db.take_query_events();
    assert!(
        strategy_events
            .iter()
            .any(|event| event.description.contains("memory_module"))
    );
    assert!(
        strategy_events
            .iter()
            .any(|event| event.description.contains("tracing_module"))
    );
    assert!(
        !strategy_events
            .iter()
            .any(|event| event.description.contains("refcount_module"))
    );
    assert!(
        !strategy_events
            .iter()
            .any(|event| event.description.contains("parsed_module"))
    );
    assert!(
        !strategy_events
            .iter()
            .any(|event| event.description.contains("checked_core"))
    );
    assert!(
        !strategy_events
            .iter()
            .any(|event| event.description.contains("runtime_module"))
    );
    assert!(matches!(rc.as_ref(), MemoryModule::ReferenceCounting(_)));
    assert!(matches!(tracing.as_ref(), MemoryModule::Tracing(_)));
    assert_eq!(
        rc.runtime().staged.typed.module,
        tracing.runtime().staged.typed.module
    );
    let memory_stats = db.query_memory_stats();
    assert!(
        memory_stats
            .iter()
            .any(|stats| stats.query.contains("cielo_memory::MemoryModule"))
    );
}

#[test]
fn collector_variants_reuse_the_runtime_product() {
    let db = CieloDatabase::default();
    let source = SourceFile::new(&db, 0, "memory.cielo".to_owned(), "fn main() {}".to_owned());
    let mut profiles = CompileProfiles::default();
    profiles.memory.model = MemoryModel::Tracing;

    let mark_sweep = compile_memory(&db, source, profiles);
    let _ = db.take_query_events();

    profiles.memory.tracing.collector = TracingCollector::SemiSpace;
    let semi_space = compile_memory(&db, source, profiles);
    let events = db.take_query_events();

    assert!(matches!(
        mark_sweep.as_ref(),
        MemoryModule::Tracing(module) if module.collector == TracingCollector::MarkSweep
    ));
    assert!(matches!(
        semi_space.as_ref(),
        MemoryModule::Tracing(module) if module.collector == TracingCollector::SemiSpace
    ));
    assert!(
        events
            .iter()
            .any(|event| event.description.contains("tracing_module"))
    );
    assert!(
        !events
            .iter()
            .any(|event| event.description.contains("runtime_module"))
    );
}
