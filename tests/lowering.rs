use cielo::common::ids::SourceId;
use cielo::common::symbols::Interner;
use cielo::frontend::parser::parse_source;
use cielo::ir::core::StmtKind;
use cielo::passes::lowering::{LowerConfig, lower_program};

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
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    assert_eq!(lowered.program.functions().len(), 2);
    assert!(!lowered.diagnostics.has_errors());
}

#[test]
fn lowers_do_statement_to_perform_stmt() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  do Console.print("x");
  0
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let main = lowered.program.functions().first().expect("function");
    let mut cursor = main.body;
    let mut seen_perform = false;
    while let Some(stmt) = lowered.program.stmt(cursor) {
        match &stmt.kind {
            StmtKind::Perform { next, .. } => {
                seen_perform = true;
                cursor = *next;
            }
            StmtKind::Let { next, .. } => cursor = *next,
            StmtKind::Return(_) => break,
            _ => break,
        }
    }
    assert!(seen_perform);
}
