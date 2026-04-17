#[path = "helpers/mod.rs"]
mod helpers;

use cielo_base::Interner;
use cielo_base::SourceId;
use cielo_staging::pipeline::phases::Reason;
use cielo_staging::pipeline::provenance::{runtime_provenance_lines, staging_root_causes};
use cielo_test_support::{PassConfig, PassHarness};
use helpers::ir::first_return_expr_linear;

#[test]
fn runtime_provenance_tracks_forced_runtime_dependency_chain() {
    let src = r#"
fn main() -> Int {
  let y = @runtime { 1 + 2 };
  let z = y + 1;
  z
}
"#;
    let mut interner = Interner::new();
    let compiler = PassHarness::new(PassConfig::default());
    let residual = compiler.compile_source(src, SourceId::from_u32(0), &mut interner);

    let main = residual
        .program()
        .functions()
        .first()
        .expect("main function");
    let ret_expr =
        first_return_expr_linear(residual.program(), main.body).expect("main return expr");
    let lines = runtime_provenance_lines(residual.program(), residual.bta(), ret_expr, 6);

    assert!(
        lines
            .iter()
            .any(|line| line.contains("explicitly marked @runtime")),
        "expected forced runtime root cause in provenance chain: {lines:?}"
    );
}

#[test]
fn runtime_provenance_reports_non_thunkable_effect_blocker() {
    let src = r#"
effect Console { fn print(s: String) -> () }

fn io() -> Int with Console {
  do Console.print("x");
  1
}

fn main() -> Int {
  let x = io();
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = PassHarness::new(PassConfig::default());
    let residual = compiler.compile_source(src, SourceId::from_u32(0), &mut interner);

    let main = residual
        .program()
        .functions()
        .iter()
        .find(|function| interner.resolve(function.name) == Some("main"))
        .expect("main function");
    let ret_expr =
        first_return_expr_linear(residual.program(), main.body).expect("main return expr");
    let lines = runtime_provenance_lines(residual.program(), residual.bta(), ret_expr, 6);

    assert!(
        lines
            .iter()
            .any(|line| line.contains("not thunkable/discharged")),
        "expected effect blocker root cause in provenance chain: {lines:?}"
    );
}

#[test]
fn test_root_cause_rollup_parameter_taint() {
    let compiler = PassHarness::new(PassConfig::default());
    let mut interner = Interner::new();

    // `input` is RT. It taints `a`, `b`, and all the binary operations.
    let source = r#"
    fn main(input: Int) -> Int {
        let a = input + 1;
        let b = a * 2;
        b
    }
    "#;

    // Stop at the BTA phase so we can inspect the analytical tables
    let core = compiler.parse_and_lower_to_core(source, SourceId::new(0), &mut interner);
    let bta = compiler.evaluate_classify(core);

    let rollups = staging_root_causes(bta.program(), bta.bta());

    let root = rollups
        .iter()
        .find(|root| matches!(root.terminal_reason, Reason::Parameter { .. }))
        .expect("Expected at least one parameter root cause");
    assert!(
        matches!(root.terminal_reason, Reason::Parameter { .. }),
        "Root cause should be a parameter, got {:?}",
        root.terminal_reason
    );

    // It should taint at least more than the parameter expression itself.
    assert!(
        root.taint_count >= 2,
        "Root cause should taint multiple dependent expressions, got {}",
        root.taint_count
    );
}
