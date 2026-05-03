use cielo_base::{Interner, SourceId};
use cielo_frontend::ast::{Expr, ExprKind, HandlerDecl, HandlerRef, Item, Stmt};
use cielo_frontend::parser::parse_source;

struct Parsed {
    program: cielo_frontend::ast::Program,
    interner: Interner,
}

fn parse(src: &str) -> Parsed {
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    assert!(
        !parsed.diagnostics.has_errors(),
        "{:?}",
        parsed
            .diagnostics
            .entries()
            .iter()
            .map(|d| d.code.to_owned())
            .collect::<Vec<_>>()
    );
    Parsed {
        program: parsed.program,
        interner,
    }
}

fn handler_decl(parsed: &Parsed) -> &HandlerDecl {
    parsed
        .program
        .items
        .iter()
        .find_map(|item| match item {
            Item::Handler(decl) => Some(decl),
            _ => None,
        })
        .expect("a handler item")
}

/// The `handle` expression bound by the first `let` in the first function.
fn handle_expr(parsed: &Parsed) -> &Expr {
    parsed
        .program
        .items
        .iter()
        .find_map(|item| match item {
            Item::Function(decl) => Some(&decl.body),
            _ => None,
        })
        .expect("a function item")
        .statements
        .iter()
        .find_map(|stmt| match stmt {
            Stmt::Let { value, .. } if matches!(value.kind, ExprKind::Handle { .. }) => Some(value),
            _ => None,
        })
        .expect("a `handle` expression")
}

const NAMED: &str = r#"
effect St { fn next() -> Int }

handler counter with St {
  | next(resume) => resume(1)
}

fn main() -> Int {
  let x = handle { do St.next(); 2 } with counter;
  x
}
"#;

#[test]
fn parses_a_named_handler_item() {
    let parsed = parse(NAMED);
    let decl = handler_decl(&parsed);
    assert_eq!(parsed.interner.resolve(decl.name), Some("counter"));
    assert_eq!(parsed.interner.resolve(decl.effect), Some("St"));
    assert_eq!(decl.clauses.len(), 1);
    assert_eq!(
        parsed.interner.resolve(decl.clauses[0].operation),
        Some("next")
    );
}

#[test]
fn handle_with_a_declared_handler_takes_no_clause_block() {
    let parsed = parse(NAMED);
    let ExprKind::Handle { handler, .. } = &handle_expr(&parsed).kind else {
        unreachable!("filtered to `handle` above");
    };
    match handler {
        HandlerRef::Named(name) => {
            assert_eq!(parsed.interner.resolve(*name), Some("counter"));
        }
        HandlerRef::Inline { .. } => panic!("`with counter` must resolve to a named handler"),
    }
}

#[test]
fn handle_with_an_effect_name_still_parses_inline_clauses() {
    let parsed = parse(
        r#"
effect St { fn next() -> Int }

fn main() -> Int {
  let x = handle { do St.next(); 2 } with St {
    | next(resume) => resume(1)
  };
  x
}
"#,
    );
    let ExprKind::Handle { handler, .. } = &handle_expr(&parsed).kind else {
        unreachable!("filtered to `handle` above");
    };
    match handler {
        HandlerRef::Inline { effect, clauses } => {
            assert_eq!(parsed.interner.resolve(*effect), Some("St"));
            assert_eq!(clauses.len(), 1);
        }
        HandlerRef::Named(_) => panic!("an effect name with a clause block must stay inline"),
    }
}
