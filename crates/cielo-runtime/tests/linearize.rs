use cielo_base::{Interner, SourceId};
use cielo_ir::linear::{HandlerOutcome, LinearStmt};
use cielo_runtime::linearize;
use cielo_test_support::{PassConfig, PassHarness};

struct Linearized {
    stmts: usize,
    codes: Vec<String>,
    outcomes: Vec<HandlerOutcome>,
    /// Clauses across every residual `Handle`, so a test can tell a table that
    /// was built from one that was merely recorded as residual.
    residual_clauses: usize,
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
    let (stmts, outcomes, residual_clauses) = {
        let (program, diagnostics) = residual.program_and_diagnostics_mut();
        let linear = linearize::run(program, &sema, diagnostics);
        let clauses = linear
            .stmts()
            .iter()
            .filter_map(|stmt| match &stmt.kind {
                LinearStmt::Handle { clauses, .. } => Some(clauses.len()),
                _ => None,
            })
            .sum();
        (
            linear.stmts().len(),
            linear
                .handler_sites
                .iter()
                .map(|site| site.outcome)
                .collect(),
            clauses,
        )
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
        outcomes,
        residual_clauses,
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

/// Merging is unsound when an arm consumes its resume's result: the join would
/// run the continuation past the code that needs its value. Defunctionalising
/// shares one copy of the continuation without moving anything past it, so this
/// clause is linear too, through the integer dispatch rather than a join.
#[test]
fn defunctionalises_a_clause_that_consumes_its_resume_result() {
    let counts: Vec<usize> = [2, 3, 4]
        .into_iter()
        .map(|performs| {
            let ticks: String = (1..=performs)
                .map(|nth| format!("    do St.tick({nth});\n"))
                .collect();
            linearize_source(&format!(
                r#"
effect St {{ fn tick(n: Int) -> Int }}

fn main() -> Int {{
  let r = handle {{
{ticks}    7
  }} with St {{
    | tick(n, resume) => if n > 0 {{ let y = resume(n); y + 1 }} else {{ let z = resume(2); z + 2 }}
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
        steps.windows(2).all(|pair| pair[0] == pair[1]),
        "one shared continuation makes growth linear in the perform count, got {counts:?}"
    );
}

/// The half of the sharing condition that has no cheap fix: `g` is bound on one
/// site's path only, and every site reaches every arm through the shared
/// continuation, so reading it in the other arm would read it undefined. Such a
/// clause keeps the inlining path and stays super-linear.
#[test]
fn leaves_a_clause_that_carries_a_branch_local_past_its_resume_alone() {
    let counts: Vec<usize> = [2, 3, 4]
        .into_iter()
        .map(|performs| {
            let ticks: String = (1..=performs)
                .map(|nth| format!("    do St.tick({nth});\n"))
                .collect();
            linearize_source(&format!(
                r#"
effect St {{ fn tick(n: Int) -> Int }}

fn main() -> Int {{
  let r = handle {{
{ticks}    7
  }} with St {{
    | tick(n, resume) => if n > 0 {{ let g = n * 3; let y = resume(g); y + g }} else {{ let z = resume(2); z + 2 }}
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
        "a site that carries a branch-local past its resume must keep re-lowering, got {counts:?}"
    );
}

/// The other half of the merge blocker, and the only shape that reaches
/// `is_tail_resumptive_stmt`'s `Perform` arm: an arm that performs an outer
/// effect before resuming. Joining would hoist that perform past the
/// continuation; entering one shared copy of it does not, so this defunctionalises.
/// Until CIELO-54 this could not be asserted end to end, because the outer
/// clause's continuation was spliced in ahead of the inner `resume` and lost the
/// context naming it.
#[test]
fn defunctionalises_a_clause_that_performs_before_resuming() {
    let counts: Vec<usize> = [2, 3, 4]
        .into_iter()
        .map(|performs| {
            let ticks: String = (1..=performs)
                .map(|nth| format!("      do St.tick({nth});\n"))
                .collect();
            let lowered = linearize_source(&format!(
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
            ));
            assert!(
                lowered
                    .codes
                    .iter()
                    .all(|code| !code.starts_with("LINEARIZE_RESUME_OUTSIDE")),
                "performing before resuming must still lower, got {:?}",
                lowered.codes
            );
            lowered.stmts
        })
        .collect();

    let steps = growth_steps(&counts);
    assert!(
        steps.windows(2).all(|pair| pair[0] == pair[1]),
        "one shared continuation makes growth linear in the perform count, got {counts:?}"
    );
}

/// Naming a handler is a frontend affordance only: by linearize it is an
/// ordinary handler frame, discharged exactly like the inline form above.
#[test]
fn discharges_a_named_handler_like_an_inline_one() {
    let codes = linearize_diagnostic_codes(
        r#"
effect St { fn note(n: Int) -> Int }

handler noted with St {
  | note(n) => 100
}

fn deep(x: Int) -> Int with St {
  do St.note(x);
  x + 1
}

fn main() -> Int {
  let out = handle { deep(3) } with noted;
  out
}
"#,
    );
    assert!(
        !codes.iter().any(|c| c == "LINEARIZE_HANDLED_EFFECT_LEAK"),
        "a named handler must discharge the same perform, got {codes:?}"
    );
}

/// `clause` handles `St.note`, performed one call deeper than inlining reaches.
fn callee_perform_with_clause(clause: &str) -> String {
    format!(
        r#"
effect St {{ fn note(n: Int) -> Int }}

fn deep(x: Int) -> Int with St {{
  let seen = do St.note(x);
  seen
}}

fn shallow(x: Int) -> Int with St {{
  let a = deep(x);
  a + 1
}}

fn main() -> Int {{
  let out = handle {{ shallow(3) }} with St {{
    {clause}
  }};
  out
}}
"#
    )
}

/// The case CIELO-1 reported: a perform inlining cannot reach used to stop the
/// build. A tail-resumptive clause now becomes a runtime clause table instead.
#[test]
fn residualizes_a_tail_resumptive_clause_performed_in_a_callee() {
    let lowered = linearize_source(&callee_perform_with_clause(
        "| note(n, resume) => resume(n)",
    ));
    assert!(
        !lowered
            .codes
            .iter()
            .any(|code| code == "LINEARIZE_HANDLED_EFFECT_LEAK"),
        "a tail-resumptive clause can stand in for inlining, got {:?}",
        lowered.codes
    );
    assert!(
        lowered.outcomes.contains(&HandlerOutcome::Residual),
        "the handle site should report a residual handler, got {:?}",
        lowered.outcomes
    );
    assert_eq!(
        lowered.residual_clauses, 1,
        "the residual handler needs a clause, or every perform reaching it traps"
    );
}

/// Each of these needs machinery the residual path deliberately does not have,
/// so the site must keep failing loudly rather than trapping at runtime.
#[test]
fn refuses_clauses_the_dispatcher_cannot_express() {
    for clause in [
        // Abortive: nothing here can unwind out of the callee.
        "| note(n) => 100",
        // The clause's value is not the resumption's value.
        "| note(n, resume) => { let y = resume(n); y + 1 }",
        // Multi-shot needs a cloned environment (CIELO-42).
        "| note(n, resume) => { let a = resume(n); let b = resume(n); a + b }",
    ] {
        let lowered = linearize_source(&callee_perform_with_clause(clause));
        assert!(
            lowered
                .codes
                .iter()
                .any(|code| code == "LINEARIZE_HANDLED_EFFECT_LEAK"),
            "`{clause}` must still be rejected, got {:?}",
            lowered.codes
        );
        assert_eq!(
            lowered.residual_clauses, 0,
            "`{clause}` must not reach the clause table"
        );
    }
}

/// A dispatched clause runs in its own frame with only its arguments, so a read
/// of an enclosing binding has nowhere to come from until closures land.
#[test]
fn refuses_a_clause_that_reads_an_enclosing_binding() {
    let lowered = linearize_source(
        r#"
effect St { fn note(n: Int) -> Int }

fn deep(x: Int) -> Int with St {
  let seen = do St.note(x);
  seen
}

fn shallow(x: Int) -> Int with St {
  let a = deep(x);
  a + 1
}

fn main() -> Int {
  let bias = 41;
  let out = handle { shallow(3) } with St {
    | note(n, resume) => resume(bias)
  };
  out
}
"#,
    );
    assert!(
        lowered
            .codes
            .iter()
            .any(|code| code == "LINEARIZE_HANDLED_EFFECT_LEAK"),
        "a captured binding has no environment yet, got {:?}",
        lowered.codes
    );
}

/// The clause table is the fallback, not a replacement: a perform inlining can
/// see must still be erased, leaving no table to dispatch through.
#[test]
fn keeps_erasing_a_lexically_visible_perform() {
    let lowered = linearize_source(
        r#"
effect St { fn note(n: Int) -> Int }

fn main() -> Int {
  let out = handle {
    let seen = do St.note(1);
    seen
  } with St {
    | note(n, resume) => resume(n)
  };
  out
}
"#,
    );
    assert_eq!(
        lowered.outcomes,
        vec![HandlerOutcome::Inlined],
        "erasure must still win when the perform is visible"
    );
    assert_eq!(
        lowered.residual_clauses, 0,
        "an erased handler needs no clause table"
    );
}
