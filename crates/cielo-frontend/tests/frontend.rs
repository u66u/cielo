use cielo_base::{Interner, SourceId, SymbolId};
use cielo_frontend::ast::{BuiltinType, ExprKind, Item, Stmt, TypeExprKind};
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

fn float_tokens(src: &str) -> (Vec<f64>, Vec<String>) {
    let mut interner = Interner::new();
    let output = lex(src, SourceId::from_u32(0), &mut interner);
    let floats = output
        .tokens
        .iter()
        .filter_map(|token| match token.kind {
            TokenKind::Float(value) => Some(value),
            _ => None,
        })
        .collect();
    let codes = output
        .diagnostics
        .entries()
        .iter()
        .map(|entry| entry.code.to_owned())
        .collect();
    (floats, codes)
}

#[test]
fn lexes_accepted_float_literal_forms() {
    let (floats, codes) = float_tokens("1.5 1e9 1.5e-3 2E+2 0.0");
    assert_eq!(floats, vec![1.5, 1e9, 1.5e-3, 2E2, 0.0]);
    assert!(codes.is_empty(), "unexpected diagnostics: {codes:?}");
}

#[test]
fn rejects_malformed_numeric_literals() {
    for src in ["1.", "1.foo", "1e", "1.5.5", "1e400"] {
        let (floats, codes) = float_tokens(src);
        assert!(floats.is_empty(), "{src} should not lex as a float");
        assert!(
            codes
                .iter()
                .any(|code| code == "LEX_BAD_NUMBER" || code == "LEX_BAD_FLOAT"),
            "{src} should be a lex error, got {codes:?}"
        );
    }
}

/// The float point rule must not eat the `.` that projects a field or names an
/// effect operation, which is the only reason it demands a digit after it.
#[test]
fn field_access_and_perform_survive_the_float_rule() {
    let src = r#"
struct P { a: Int }
effect St { fn tick(n: Int) -> Int }
fn main() -> Int {
  let p = P(3);
  let v = do St.tick(p.a);
  v
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    assert!(!parsed.diagnostics.has_errors());
}

#[test]
fn parses_a_float_literal_expression() {
    let src = "fn main() -> Float { -1.25 }";
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    assert!(!parsed.diagnostics.has_errors());
    let Some(Item::Function(function)) = parsed.program.items.first() else {
        panic!("expected a function item");
    };
    assert!(matches!(
        function.return_type.as_ref().map(|ty| &ty.kind),
        Some(TypeExprKind::Builtin(BuiltinType::Float))
    ));
    let tail = function.body.tail.as_ref().expect("expected a tail expr");
    let ExprKind::Unary { expr, .. } = &tail.kind else {
        panic!("expected unary negation");
    };
    assert!(matches!(expr.kind, ExprKind::Float(value) if value == 1.25));
}
