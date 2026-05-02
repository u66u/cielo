use cielo_base::{Interner, SourceId};
use cielo_runtime::linearize;
use cielo_test_support::{PassConfig, PassHarness};

struct Linearized {
    stmts: usize,
    codes: Vec<String>,
}

fn linearize_source(source: &str) -> Linearized {
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
    let stmts = {
        let (program, diagnostics) = residual.program_and_diagnostics_mut();
        linearize::run(program, &sema, diagnostics).stmts().len()
    };
    Linearized {
        stmts,
        codes: residual
            .diagnostics()
            .entries()
            .iter()
            .skip(before)
            .map(|d| d.code.to_owned())
            .collect(),
    }
}

fn linearize_diagnostic_codes(source: &str) -> Vec<String> {
    linearize_source(source).codes
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

/// `performs` sequential performs of one operation, all discharged by `clause`.
fn handled_perform_chain(performs: usize, clause: &str) -> String {
    let body: String = (1..=performs)
        .map(|nth| format!("  do St.tick({nth});\n"))
        .collect();
    format!(
        "effect St {{ fn tick(n: Int) -> Int }}\n\n\
         fn body(x: Int) -> Int with St {{\n{body}  x\n}}\n\n\
         fn main() -> Int {{\n  \
         let r = handle {{ body(7) }} with St {{\n    {clause}\n  }};\n  r\n}}\n"
    )
}

fn growth_steps(counts: &[usize]) -> Vec<usize> {
    counts.windows(2).map(|pair| pair[1] - pair[0]).collect()
}

/// Re-lowering the continuation at every `resume` made a two-site clause expand
/// as 2^performs: 12 performs emitted 148k lines of C and 16 blew the inline
/// budget. Merging the sites into one join point makes growth linear.
#[test]
fn merges_tail_resume_sites_into_one_join() {
    let counts: Vec<usize> = [4, 8, 12, 16]
        .into_iter()
        .map(|performs| {
            let lowered = linearize_source(&handled_perform_chain(
                performs,
                "| tick(n, resume) => if n > 0 { resume(1) } else { resume(2) }",
            ));
            assert!(
                !lowered
                    .codes
                    .iter()
                    .any(|code| code == "LINEARIZE_INLINE_BUDGET_EXCEEDED"),
                "{performs} performs must stay inside the inline budget, got {:?}",
                lowered.codes
            );
            lowered.stmts
        })
        .collect();

    let steps = growth_steps(&counts);
    assert!(
        steps.windows(2).all(|pair| pair[0] == pair[1]),
        "statement count must grow linearly in the perform count, got {counts:?}"
    );
}

/// Merging is unsound when an arm performs before it resumes: the join would
/// hoist that perform past the continuation. Such a clause keeps the inlining
/// path, and stays super-linear, which is what the budget exists to catch.
#[test]
fn leaves_a_clause_that_performs_before_resuming_alone() {
    let counts: Vec<usize> = [2, 3, 4]
        .into_iter()
        .map(|performs| {
            let ticks: String = (1..=performs)
                .map(|nth| format!("      do St.tick({nth});\n"))
                .collect();
            linearize_source(&format!(
                r#"
effect St {{ fn tick(n: Int) -> Int }}
effect Log {{ fn emit(n: Int) -> Int }}

fn main() -> Int {{
  let r = handle {{
    let inner = handle {{
{ticks}      7
    }} with St {{
      | tick(n, resume) => if n > 0 {{ let e = do Log.emit(n); resume(e) }} else {{ resume(2) }}
    }};
    inner
  }} with Log {{
    | emit(m, resume) => resume(m)
  }};
  r
}}
"#
            ))
            .stmts
        })
        .collect();

    let steps = growth_steps(&counts);
    assert!(
        steps[1] > steps[0],
        "a non-tail clause must keep re-lowering the continuation, got {counts:?}"
    );
}
