use cielo_base::SourceId;
use cielo_base::Interner;
use cielo_staging::pipeline::staging_diagnostics::{render_stage_b_counter_summary, staging_pass_counters};
use cielo::{Compiler, CompilerConfig};

#[test]
fn staging_diagnostics_surface_residual_and_specialization_counters() {
    let src = r#"
effect Console { fn print(s: String) -> () }

fn io() -> Int with Console {
  do Console.print("x");
  1
}

fn main() -> Int {
  let a = handle { io() } with Console {
    | print(s) => 0
  };
  let b = handle { io() } with Console {
    | print(s) => 0
  };
  a + b
}
"#;

    let compiler = Compiler::new(CompilerConfig::default());
    let mut interner = Interner::new();
    let core = compiler.parse_and_lower_to_core(src, SourceId::from_u32(0), &mut interner);
    let staged = compiler.run_v1_evaluate_classify(core);
    let residual = compiler.run_v1_residualize_specialize(staged);

    let counters = staging_pass_counters(&residual);
    assert_eq!(
        counters.residualize,
        residual.residual().residualize_stats,
        "staging diagnostics should surface residualization counters from residual tables"
    );
    assert_eq!(
        counters.specialize,
        residual.residual().specialization_stats,
        "staging diagnostics should surface specialization counters from residual tables"
    );
    assert!(
        counters.specialize.candidates_seen > 0
            && counters.specialize.created > 0
            && counters.specialize.rewrites > 0,
        "fixture should exercise handler specialization so diagnostics include non-zero specialization counters"
    );

    let rendered = render_stage_b_counter_summary(counters);
    assert!(
        rendered.contains("residualize embedded_literals=")
            && rendered.contains("specialize candidates="),
        "rendered staging diagnostics should include residualize/specialize counter headings"
    );
    assert!(
        rendered.contains(format!("candidates={}", counters.specialize.candidates_seen).as_str())
            && rendered.contains(format!("rewrites={}", counters.specialize.rewrites).as_str()),
        "rendered staging diagnostics should include concrete specialization counter values"
    );
}
