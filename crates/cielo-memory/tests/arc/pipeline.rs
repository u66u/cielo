use cielo_base::Interner;
use cielo_base::SourceId;
use cielo_test_support::{PassConfig, PassHarness};

#[test]
fn v1_pipeline_tracks_arc_accounting_consistently() {
    let src = r#"
enum Boxed { Wrap(Int) }
fn main() -> Int {
  let a = Wrap(1);
  let b = a;
  b
}
"#;

    let mut interner = Interner::new();
    let compiler = PassHarness::new(PassConfig::default());
    let compiled = compiler.compile_source_to_c(src, SourceId::from_u32(0), &mut interner);
    let stats = compiled
        .memory
        .reference_counting()
        .expect("ARC test must select reference counting")
        .arc;

    assert!(
        stats.planned_retain_ops >= stats.final_retain_ops,
        "final retain ops must not exceed planned retain ops"
    );
    assert!(
        stats.planned_release_ops >= stats.final_release_ops,
        "final release ops must not exceed planned release ops"
    );
    assert_eq!(
        stats
            .planned_retain_ops
            .saturating_sub(stats.removed_retain_ops),
        stats.final_retain_ops,
        "retain accounting should be internally consistent"
    );
    assert_eq!(
        stats
            .planned_release_ops
            .saturating_sub(stats.removed_release_ops),
        stats.final_release_ops,
        "release accounting should be internally consistent"
    );
    assert!(
        stats.eliminated_move_pairs <= stats.planned_retain_ops + stats.planned_release_ops,
        "eliminated ARC pair count should never exceed total planned ARC operations"
    );
}
