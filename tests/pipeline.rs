use cielo::common::ids::SourceId;
use cielo::common::symbols::Interner;
use cielo::{Compiler, CompilerConfig};

#[test]
fn compiles_source_through_v0_skeleton_pipeline() {
    let src = r#"
fn add(a: Int, b: Int) -> Int {
  a + b
}
fn main() -> Int {
  add(1, 2)
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);
    assert_eq!(residual.program.functions().len(), 2);
    assert_eq!(residual.program.entrypoints().len(), 1);
}
