use cielo_base::{Interner, SourceId, SymbolId};
use cielo_frontend::ast::{ExprKind, Item, Stmt, TypeExprKind};
use cielo_frontend::lexer::{Keyword, TokenKind, lex};
use cielo_frontend::parser::parse_source;

#[test]
fn lexes_keywords_and_symbols() {
    let mut interner = Interner::new();
    let output = lex(
        "fn add(a: i32) -> i32 { match a { | _ => 1 } }",
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
    assert!(
        output
            .tokens
            .iter()
            .any(|token| token.kind == TokenKind::Keyword(Keyword::Match))
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
fn parse_comptime_function_annotation() {
    let src = r#"
@comptime fn fold() -> Int {
  7
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let function = parsed
        .program
        .items
        .iter()
        .find_map(|item| match item {
            Item::Function(function) => Some(function),
            _ => None,
        })
        .expect("expected function");

    assert!(function.ct_only);
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

#[test]
fn parse_match_expression() {
    let src = r#"
enum Option { Some(Int), None }
fn main() -> Int {
  let x = match Some(1) {
    | Some(v) => v
    | _ => 0
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
    let has_match = func.body.statements.iter().any(|stmt| match stmt {
        Stmt::Let { value, .. } => matches!(value.kind, ExprKind::Match { .. }),
        _ => false,
    });
    assert!(has_match);
    assert!(!parsed.diagnostics.has_errors());
}

#[test]
fn parse_type_parameter_lists() {
    let src = r#"
struct Pair[A, B] { first: A, second: B }
enum Option[T] { Some(T), None }
fn identity[T](x: T) -> T { x }
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    assert!(!parsed.diagnostics.has_errors(), "{:?}", parsed.diagnostics);

    let names = |symbols: &[SymbolId]| {
        symbols
            .iter()
            .map(|symbol| interner.resolve(*symbol).unwrap_or("<missing>").to_owned())
            .collect::<Vec<_>>()
    };

    for item in &parsed.program.items {
        match item {
            Item::Struct(decl) => assert_eq!(names(&decl.type_params), vec!["A", "B"]),
            Item::Enum(decl) => assert_eq!(names(&decl.type_params), vec!["T"]),
            Item::Function(decl) => assert_eq!(names(&decl.type_params), vec!["T"]),
            _ => panic!("unexpected item"),
        }
    }
}

#[test]
fn parse_keeps_type_arguments_at_use_sites() {
    let src = r#"
enum Option[T] { Some(T), None }
fn unwrap_or[T](opt: Option[T], fallback: T) -> T { fallback }
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    assert!(!parsed.diagnostics.has_errors(), "{:?}", parsed.diagnostics);

    let function = parsed
        .program
        .items
        .iter()
        .find_map(|item| match item {
            Item::Function(function) => Some(function),
            _ => None,
        })
        .expect("expected function");
    let TypeExprKind::Path { name, args } = &function.params[0].ty.kind else {
        panic!("expected a path type for the first parameter");
    };
    assert_eq!(interner.resolve(*name), Some("Option"));
    assert_eq!(args.len(), 1);
    assert!(matches!(args[0].kind, TypeExprKind::Path { .. }));
}

#[test]
fn reject_duplicate_type_parameter() {
    let src = "fn f[T, T](x: T) -> T { x }";
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    assert!(
        parsed
            .diagnostics
            .entries()
            .iter()
            .any(|entry| entry.code == "PARSE_DUP_TYPE_PARAM")
    );
}

#[test]
fn reject_generic_effect_declarations_and_rows() {
    let src = r#"
effect State[S] { fn get() -> Int }
fn f(x: Int) -> Int with State[Int] { x }
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    assert_eq!(
        parsed
            .diagnostics
            .entries()
            .iter()
            .filter(|entry| entry.code == "PARSE_GENERIC_EFFECT_ARGS")
            .count(),
        2
    );
}

#[test]
fn reject_generic_handler_declarations() {
    let src = r#"
effect Console { fn print(s: String) -> () }
handler quiet[T] with Console { | print(s) => 0 }
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    assert!(
        parsed
            .diagnostics
            .entries()
            .iter()
            .any(|entry| entry.code == "PARSE_GENERIC_HANDLER")
    );
}

#[test]
fn reject_type_arguments_in_expression_position() {
    let src = r#"
fn identity[T](x: T) -> T { x }
fn main() -> Int { identity[Int](1) }
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    assert!(
        parsed
            .diagnostics
            .entries()
            .iter()
            .any(|entry| entry.code == "PARSE_TYPE_ARGS_IN_EXPR")
    );
}
