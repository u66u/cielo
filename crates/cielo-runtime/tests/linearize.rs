use cielo_base::{Interner, SourceId};
use cielo_runtime::linearize;
use cielo_test_support::{PassConfig, PassHarness};

fn linearize_diagnostic_codes(source: &str) -> Vec<String> {
    let harness = PassHarness::new(PassConfig::default());
    let mut interner = Interner::new();
    let mut residual = harness.compile_source(source, SourceId::from_u32(0), &mut interner);
    assert!(
        !residual.diagnostics().has_errors(),
        "source must reach linearize without earlier errors: {:?}",
        residual
            .diagnostics()
            .entries()
            .iter()
            .map(|d| d.code.to_owned())
            .collect::<Vec<_>>()
    );

    let sema = residual.sema().clone();
    let before = residual.diagnostics().entries().len();
    {
        let (program, diagnostics) = residual.program_and_diagnostics_mut();
        let _ = linearize::run(program, &sema, diagnostics);
    }
    residual
        .diagnostics()
        .entries()
        .iter()
        .skip(before)
        .map(|d| d.code.to_owned())
        .collect()
}

/// A perform reachable only through a call cannot be inlined away, and before
/// this was detected the handler frame was dropped and the program returned a
/// wrong value with no diagnostics.
#[test]
fn reports_handled_effect_performed_inside_a_callee() {
    let codes = linearize_diagnostic_codes(
        r#"
effect St { fn note(n: Int) -> Int }

fn deep(x: Int) -> Int with St {
  do St.note(x);
  x + 1
}

fn shallow(x: Int) -> Int with St {
  let a = deep(x);
  a + 1
}

fn main() -> Int {
  let out = handle { shallow(3) } with St {
    | note(n) => 100
  };
  out
}
"#,
    );
    assert!(
        codes.iter().any(|c| c == "LINEARIZE_HANDLED_EFFECT_LEAK"),
        "expected a leak diagnostic, got {codes:?}"
    );
}

#[test]
fn accepts_handled_effect_performed_in_the_handled_function() {
    let codes = linearize_diagnostic_codes(
        r#"
effect St { fn note(n: Int) -> Int }

fn deep(x: Int) -> Int with St {
  do St.note(x);
  x + 1
}

fn main() -> Int {
  let out = handle { deep(3) } with St {
    | note(n) => 100
  };
  out
}
"#,
    );
    assert!(
        !codes.iter().any(|c| c == "LINEARIZE_HANDLED_EFFECT_LEAK"),
        "a lexically visible perform is discharged by inlining, got {codes:?}"
    );
}
