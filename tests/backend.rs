use cielo::common::ids::SourceId;
use cielo::common::symbols::Interner;
use cielo::{Compiler, CompilerConfig};

#[test]
fn emits_c_for_basic_arithmetic_program() {
    let src = r#"
fn main() -> Int {
  let x = 1 + 2;
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    assert_eq!(compiled.linear.functions.len(), 1);
    assert!(compiled.c_source.contains("cv_add("));
    assert!(compiled.c_source.contains("int main(void)"));
    assert!(compiled.residual.diagnostics.entries().is_empty());
}

#[test]
fn emits_runtime_stub_calls_for_effect_operations() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  do Console.print("hello");
  0
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    assert!(compiled.c_source.contains("cielo_perform("));
    assert!(compiled.c_source.contains("print"));
}
