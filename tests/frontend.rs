use cielo::common::ids::SourceId;
use cielo::common::symbols::Interner;
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
