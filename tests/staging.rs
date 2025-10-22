use cielo::common::ids::SourceId;
use cielo::common::symbols::Interner;
use cielo::pipeline::phases::Stage;
use cielo::{Compiler, CompilerConfig};

#[test]
fn ct_and_bta_tables_are_populated() {
    let src = r#"
fn main() -> Int {
  let x = 1 + 2;
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);

    assert!(!residual.ct.ct_cache.is_empty());
    assert!(
        residual
            .bta
            .stage_of_expr
            .values()
            .any(|stage| matches!(stage, Stage::Ct))
    );
}
