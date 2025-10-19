use cielo::common::ids::SourceId;
use cielo::common::symbols::Interner;
use cielo::frontend::parser::parse_source;
use cielo::passes::lowering::lower_program;

#[test]
fn lowers_simple_program() {
    let src = r#"
fn main() -> Int {
  let a = 1;
  let b = 2;
  add(a, b)
}

fn add(x: Int, y: Int) -> Int {
  x + y
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program);
    assert_eq!(lowered.program.functions().len(), 2);
    assert!(!lowered.diagnostics.has_errors());
}
