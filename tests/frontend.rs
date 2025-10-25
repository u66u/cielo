use cielo::common::ids::SourceId;
use cielo::common::symbols::Interner;
use cielo::frontend::ast::{ExprKind, Item, Stmt};
use cielo::frontend::lexer::{Keyword, TokenKind, lex};
use cielo::frontend::parser::parse_source;

#[test]
fn lexes_keywords_and_symbols() {
    let mut interner = Interner::new();
    let output = lex(
        "fn add(a: i32) -> i32 { a + 1 }",
        SourceId::from_u32(0),
        &mut interner,
    );
    assert!(
        output
            .tokens
            .iter()
            .any(|token| token.kind == TokenKind::Keyword(Keyword::Fn))
    );
    assert!(
        output
            .tokens
            .iter()
            .any(|token| token.kind == TokenKind::Arrow)
    );
    assert_eq!(
        output.tokens.last().map(|token| &token.kind),
        Some(&TokenKind::Eof)
    );
}

#[test]
fn parse_minimal_function() {
    let src = r#"
fn add(a: Int, b: Int) -> Int {
  let c = a + b;
  c
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    assert_eq!(parsed.program.items.len(), 1);
    assert!(!parsed.diagnostics.has_errors());
}

#[test]
fn parse_struct_enum_effect() {
    let src = r#"
struct Point { x: Int, y: Int }
enum Option { Some(Int), None }
effect Console { fn print(s: String) -> () }
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    assert_eq!(parsed.program.items.len(), 3);
}

#[test]
fn parse_do_effect_statement() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  do Console.print("hi");
  0
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let func = parsed
        .program
        .items
        .iter()
        .find_map(|item| match item {
            Item::Function(func) => Some(func),
            _ => None,
        })
        .expect("expected function");
    assert!(
        func.body
            .statements
            .iter()
            .any(|stmt| matches!(stmt, Stmt::Perform { .. }))
    );
}

#[test]
fn parse_handle_expression() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  let x = handle 0 with Console {
    | print(s) => 1
  };
  x
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let func = parsed
        .program
        .items
        .iter()
        .find_map(|item| match item {
            Item::Function(func) => Some(func),
            _ => None,
        })
        .expect("expected function");
    let has_handle = func.body.statements.iter().any(|stmt| match stmt {
        Stmt::Let { value, .. } => matches!(value.kind, ExprKind::Handle { .. }),
        _ => false,
    });
    assert!(has_handle);
}
