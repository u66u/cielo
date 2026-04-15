use cielo_db::{
    CieloDatabase, CompileProfile, SourceFile, compile, compile_memory, linear_file, staged_file,
};
use cielo_memory::MemoryProfile;
use salsa::Setter;

#[test]
fn source_changes_flow_through_the_real_pipeline() {
    let mut db = CieloDatabase::default();
    let source = SourceFile::new(&db, 0, "memory.cielo".to_owned(), "fn main() {}".to_owned());
    let profile = CompileProfile::default();

    let first = compile(&db, source, profile);
    assert!(!first.c_source.is_empty());
    assert!(!db.take_query_events().is_empty());

    let _cached = compile(&db, source, profile);
    assert!(db.take_query_events().is_empty());

    source
        .set_text(&mut db)
        .to("fn main() {}\nfn helper() {}".to_owned());
    let _second = compile(&db, source, profile);
    assert!(
        db.take_query_events()
            .iter()
            .any(|event| event.description.contains("parsed_file"))
    );
}

#[test]
fn changing_memory_config_does_not_rerun_runtime_queries() {
    let db = CieloDatabase::default();
    let source = SourceFile::new(&db, 0, "memory.cielo".to_owned(), "fn main() {}".to_owned());
    let profile = CompileProfile::default();
    let _arc = compile_memory(&db, source, profile);
    let _ = db.take_query_events();

    let off = CompileProfile {
        memory: MemoryProfile::from_preset(cielo_memory::MemoryPreset::Unmanaged),
        ..profile
    };
    let _unmanaged = compile_memory(&db, source, off);
    let events = db.take_query_events();
    assert!(
        events
            .iter()
            .any(|event| event.description.contains("memory_file"))
    );
    assert!(
        !events
            .iter()
            .any(|event| event.description.contains("runtime_file"))
    );
}

#[test]
fn query_memory_stats_include_real_products() {
    let db = CieloDatabase::default();
    let source = SourceFile::new(&db, 0, "memory.cielo".to_owned(), "fn main() {}".to_owned());
    let _ = compile(&db, source, CompileProfile::default());
    assert!(
        db.query_memory_stats()
            .iter()
            .any(|stats| stats.query.contains("EmittedFile"))
    );
}

#[test]
fn staging_and_runtime_have_separate_coarse_queries() {
    let db = CieloDatabase::default();
    let source = SourceFile::new(&db, 0, "memory.cielo".to_owned(), "fn main() {}".to_owned());

    let _staged = staged_file(&db, source, CompileProfile::default().target);
    let staged_events = db.take_query_events();
    assert!(
        staged_events
            .iter()
            .any(|event| event.description.contains("monomorphized_file"))
    );
    assert!(
        staged_events
            .iter()
            .any(|event| event.description.contains("classified_file"))
    );

    let _linear = linear_file(&db, source, CompileProfile::default().target);
    let linear_events = db.take_query_events();
    assert!(
        linear_events
            .iter()
            .any(|event| event.description.contains("linear_file"))
    );
    assert!(
        !linear_events
            .iter()
            .any(|event| event.description.contains("staged_file"))
    );
}
