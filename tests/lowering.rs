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

#[test]
fn lowers_handle_expression_into_handler_and_val_flow() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  let x = handle 7 with Console {
    | print(s) => 0
  };
  x
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    assert_eq!(lowered.program.handlers().len(), 1);

    let main = lowered.program.functions().first().expect("function");
    let mut cursor = main.body;
    let mut seen_val_with_handle = false;
    while let Some(stmt) = lowered.program.stmt(cursor) {
        match &stmt.kind {
            StmtKind::Val { value, next, .. } => {
                if matches!(
                    lowered.program.stmt(*value).map(|node| &node.kind),
                    Some(StmtKind::Handle { .. })
                ) {
                    seen_val_with_handle = true;
                }
                cursor = *next;
            }
            StmtKind::Let { next, .. } => cursor = *next,
            StmtKind::Return(_) => break,
            _ => break,
        }
    }
    assert!(seen_val_with_handle);
}

#[test]
fn lowers_effectful_function_call_to_call_stmt() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn ping() -> Int with Console {
  do Console.print("x");
  7
}
fn main() -> Int {
  let y = ping();
  y
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let main = lowered
        .program
        .functions()
        .iter()
        .find(|f| interner.resolve(f.name) == Some("main"))
        .expect("main");

    let mut cursor = main.body;
    let mut saw_call_stmt = false;
    while let Some(stmt) = lowered.program.stmt(cursor) {
        match &stmt.kind {
            StmtKind::Val { value, next, .. } => {
                if matches!(
                    lowered.program.stmt(*value).map(|node| &node.kind),
                    Some(StmtKind::Call { .. })
                ) {
                    saw_call_stmt = true;
                }
                cursor = *next;
            }
            StmtKind::Let { next, .. } => cursor = *next,
            StmtKind::Return(_) => break,
            _ => break,
        }
    }
    assert!(saw_call_stmt);
}

#[test]
fn lowers_stage_block_into_stage_stmt() {
    let src = r#"
fn main() -> Int {
  let y = @runtime { 1 + 2 };
  y
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let main = lowered.program.functions().first().expect("function");

    let mut cursor = main.body;
    let mut saw_stage_stmt = false;
    while let Some(stmt) = lowered.program.stmt(cursor) {
        match &stmt.kind {
            StmtKind::Val { value, next, .. } => {
                if matches!(
                    lowered.program.stmt(*value).map(|node| &node.kind),
                    Some(StmtKind::Stage { .. })
                ) {
                    saw_stage_stmt = true;
                }
                cursor = *next;
            }
            StmtKind::Let { next, .. } => cursor = *next,
            StmtKind::Return(_) => break,
            _ => break,
        }
    }
    assert!(saw_stage_stmt);
}

#[test]
fn reports_unknown_effect_operation_in_do_statement() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  do Console.read();
  0
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    assert!(
        lowered
            .diagnostics
            .entries()
            .iter()
            .any(|diag| diag.code == "LOWER_UNKNOWN_EFFECT_OP")
    );
}

#[test]
fn reports_handler_clause_arity_mismatch() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  let x = handle 7 with Console {
    | print() => 0
  };
  x
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    assert!(
        lowered
            .diagnostics
            .entries()
            .iter()
            .any(|diag| diag.code == "LOWER_BAD_HANDLER_CLAUSE_ARITY")
    );
}

#[test]
fn reports_duplicate_handler_clause() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  let x = handle 7 with Console {
    | print(s) => 0
    | print(s) => 1
  };
  x
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    assert!(
        lowered
            .diagnostics
            .entries()
            .iter()
            .any(|diag| diag.code == "LOWER_DUP_HANDLER_CLAUSE")
    );
}
