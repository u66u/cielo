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

/// Randomised call chains of varying depth, each with the perform at the far
/// end. Every one must be rejected: inlining cannot see through a call, and
/// before this was detected the handler frame was silently dropped.
#[test]
fn rejects_transitive_performs_at_every_call_depth() {
    for depth in 1..=6 {
        let mut source = String::from("effect St { fn note(n: Int) -> Int }\n\n");
        source.push_str("fn level0(x: Int) -> Int with St {\n  do St.note(x);\n  x + 1\n}\n\n");
        for level in 1..=depth {
            let callee = level - 1;
            source.push_str(&format!(
                "fn level{level}(x: Int) -> Int with St {{\n  let a = level{callee}(x);\n  a + {level}\n}}\n\n"
            ));
        }
        source.push_str(&format!(
            "fn main() -> Int {{\n  let out = handle {{ level{depth}(3) }} with St {{\n    | note(n) => 100\n  }};\n  out\n}}\n"
        ));

        let codes = linearize_diagnostic_codes(&source);
        assert!(
            codes.iter().any(|c| c == "LINEARIZE_HANDLED_EFFECT_LEAK"),
            "depth {depth} should be rejected, got {codes:?}"
        );
    }
}

/// A nested handler shadows the enclosing one only for its own effect. Before
/// the inliner carried a handler stack, the outer handler was dropped inside
/// the nested block and its effect escaped unrewritten.
#[test]
fn discharges_an_outer_effect_performed_inside_a_nested_handler() {
    let codes = linearize_diagnostic_codes(
        r#"
effect A { fn ping() -> Int }
effect B { fn pong() -> Int }

fn main() -> Int {
  let r = handle {
    let inner = handle {
      let a = do A.ping();
      let b = do B.pong();
      a + b
    } with A {
      | ping(resume) => resume(1)
    };
    inner
  } with B {
    | pong(resume) => resume(2)
  };
  r
}
"#,
    );
    assert!(
        !codes.iter().any(|c| c == "LINEARIZE_HANDLED_EFFECT_LEAK"),
        "the outer handler must still apply inside the nested block, got {codes:?}"
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
