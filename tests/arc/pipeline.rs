use crate::helpers::core::compile_source_v1;

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

    let residual = compile_source_v1(src);
    let stats = residual.residual().arc_stats;

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
        stats.eliminated_move_pairs >= 1,
        "copy-site retain+release pair should be optimized away in this fixture"
    );
}
