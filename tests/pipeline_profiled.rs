use cielo::common::ids::SourceId;
use cielo::common::symbols::Interner;
use cielo::{Compiler, CompilerConfig};

#[test]
fn profiled_pipeline_matches_regular_compile_shape() {
    let src = r#"
fn main() -> Int {
  let x = 1 + 2;
  x
}
"#;
    let compiler = Compiler::new(CompilerConfig::default());

    let mut plain_interner = Interner::new();
    let plain = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut plain_interner);

    let mut profiled_interner = Interner::new();
    let (profiled, timings) =
        compiler.compile_source_v0_profiled(src, SourceId::from_u32(0), &mut profiled_interner);

    assert_eq!(
        plain.program().functions().len(),
        profiled.program().functions().len(),
        "profiled compile should preserve function shape"
    );
    assert_eq!(
        plain.diagnostics().entries().len(),
        profiled.diagnostics().entries().len(),
        "profiled compile should preserve diagnostics shape"
    );
    assert_eq!(
        timings.total(),
        timings.parse
            .saturating_add(timings.lower)
            .saturating_add(timings.typecheck)
            .saturating_add(timings.monomorphize)
            .saturating_add(timings.ct_propagate)
            .saturating_add(timings.bta)
            .saturating_add(timings.residualize),
        "total timing should equal the sum of stage timings"
    );
}
