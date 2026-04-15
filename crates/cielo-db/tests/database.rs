use cielo_base::SourceId;
use cielo_db::{
    CieloDatabase, CompileProfile, SourceFile, TargetProfile, compile, parsed_file, typed_file,
};
use cielo_memory::{MemoryPreset, MemoryProfile};

fn source(db: &CieloDatabase, text: &str) -> SourceFile {
    SourceFile::new(
        db,
        SourceId::from_u32(0).as_u32(),
        "<test>".to_owned(),
        text.to_owned(),
    )
}

#[test]
fn parses_through_the_database_boundary() {
    let db = CieloDatabase::default();
    let parsed = parsed_file(&db, source(&db, "fn main() {}"));
    assert_eq!(parsed.ast.items.len(), 1);
    assert!(!parsed.diagnostics.has_errors());
}

#[test]
fn typechecks_through_the_database_boundary() {
    let db = CieloDatabase::default();
    let typed = typed_file(&db, source(&db, "fn main() {}"), TargetProfile::default());
    assert_eq!(typed.typed.program().functions().len(), 1);
    assert!(!typed.typed.diagnostics().has_errors());
    assert_eq!(
        typed.typed.facts().type_of_expr.len(),
        typed.typed.program().exprs().len()
    );
}

#[test]
fn emits_through_the_database_boundary() {
    let db = CieloDatabase::default();
    let profile = CompileProfile {
        target: TargetProfile::default(),
        memory: MemoryProfile::from_preset(MemoryPreset::Unmanaged),
    };
    let emitted = compile(&db, source(&db, "fn main() {}"), profile);
    assert!(emitted.c_source.contains("int main(void)"));
}
