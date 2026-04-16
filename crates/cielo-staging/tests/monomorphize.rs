use cielo_base::Interner;
use cielo_base::SourceId;
use cielo_test_support::{PassConfig, PassHarness};

#[test]
fn monomorphize_summary_tracks_identity_in_v0() {
    let src = r#"
fn add(a: Int, b: Int) -> Int {
  a + b
}

fn main() -> Int {
  add(1, 2)
}
"#;
    let mut interner = Interner::new();
    let compiler = PassHarness::new(PassConfig::default());
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);

    assert_eq!(residual.program().functions().len(), 2);
    assert_eq!(residual.mono().source_to_mono.len(), 2);
}

#[test]
fn v1_pipeline_compacts_monomorphization_summary_after_pruning() {
    let src = r#"
fn add(a: Int, b: Int) -> Int {
  a + b
}

fn main() -> Int {
  add(1, 2)
}
"#;
    let mut interner = Interner::new();
    let compiler = PassHarness::new(PassConfig::default());
    let residual = compiler.compile_source(src, SourceId::from_u32(1), &mut interner);
    let func_count = residual.program().functions().len();
    assert!(
        residual
            .mono()
            .source_to_mono
            .iter()
            .all(|(source, monos)| {
                source.index() < func_count && monos.iter().all(|mono| mono.index() < func_count)
            }),
        "v1 residualization/specialization must keep monomorphization ids within compacted function bounds"
    );
    assert_eq!(
        residual.program().functions().len(),
        1,
        "v1 may fold/prune unused helpers from the final residual function set"
    );
}

#[test]
fn v1_pipeline_keeps_runtime_reachable_helpers() {
    let src = r#"
fn add(a: Int, b: Int) -> Int {
  a + b
}

fn main() -> Int {
  let x = @runtime { 1 };
  add(x, 2)
}
"#;
    let mut interner = Interner::new();
    let compiler = PassHarness::new(PassConfig::default());
    let residual = compiler.compile_source(src, SourceId::from_u32(2), &mut interner);
    let names = residual
        .program()
        .functions()
        .iter()
        .map(|f| interner.resolve(f.name).unwrap_or("<missing>").to_owned())
        .collect::<Vec<_>>();
    assert!(
        names.iter().any(|name| name == "add"),
        "runtime-dependent calls must keep helper function bodies reachable in v1"
    );
    assert!(
        names.iter().any(|name| name == "main"),
        "entrypoint must remain reachable in v1"
    );
    assert!(
        residual.mono().source_to_mono.iter().all(|(_, monos)| monos
            .iter()
            .all(|mono| mono.index() < residual.program().functions().len())),
        "monomorphization summary must remain in-bounds after v1 pruning"
    );
}
