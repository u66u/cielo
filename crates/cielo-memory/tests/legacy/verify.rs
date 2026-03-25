use cielo::common::ids::SourceId;
use cielo::common::symbols::Interner;
use cielo::{Compiler, CompilerConfig};

#[test]
fn arc_verify_rejects_duplicate_managed_call_args() {
    let src = r#"
enum Boxed { Wrap(Int) }
fn sum(a: Boxed, b: Boxed) -> Int {
  match a {
    Wrap(x) => match b {
      Wrap(y) => x + y,
    },
  }
}
fn main() -> Int {
  let x = Wrap(1);
  sum(x, x)
}
"#;

    let compiler = Compiler::new(CompilerConfig::default());
    let mut interner = Interner::new();
    let compiled = compiler.compile_source_to_c(src, SourceId::from_u32(0), &mut interner);
    assert!(
        compiled.residual.diagnostics().has_errors(),
        "duplicate managed call args should fail ARC verifier"
    );
    assert!(
        compiled
            .residual
            .diagnostics()
            .entries()
            .iter()
            .any(|entry| entry.code == "ARC_VERIFY_DUP_MANAGED_CALL_ARG"),
        "expected ARC verifier duplicate managed arg diagnostic"
    );
}
