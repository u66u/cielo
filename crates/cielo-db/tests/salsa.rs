use cielo_db::{CieloDatabase, CompileProfile, SourceFile, compile, compile_memory};
use cielo_memory::GcConfig;
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
        gc: GcConfig::from_preset(cielo_memory::GcPreset::Off),
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
