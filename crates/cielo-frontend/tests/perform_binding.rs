use cielo_base::{Interner, SourceId};
use cielo_frontend::ast::{Item, Stmt};
use cielo_frontend::parser::parse_source;

fn function_body_statements(src: &str) -> Vec<Stmt> {
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
    parsed
        .program
        .items
        .into_iter()
        .find_map(|item| match item {
            Item::Function(decl) => Some(decl.body.statements),
            _ => None,
        })
        .expect("a function item")
}

#[test]
fn binds_the_result_of_a_performed_operation() {
    let statements = function_body_statements(
        r#"
effect St { fn next(seed: Int) -> Int }
fn main() -> Int {
  let a = do St.next(1);
  a
}
"#,
    );
    let binding = statements.iter().find_map(|stmt| match stmt {
        Stmt::Perform { binding, .. } => Some(*binding),
        _ => None,
    });
    assert!(
        matches!(binding, Some(Some(_))),
        "`let a = do ...` must bind the operation result, got {binding:?}"
    );
}

#[test]
fn bare_perform_discards_its_result() {
    let statements = function_body_statements(
        r#"
effect St { fn next(seed: Int) -> Int }
fn main() -> Int {
  do St.next(1);
  0
}
"#,
    );
    let binding = statements.iter().find_map(|stmt| match stmt {
        Stmt::Perform { binding, .. } => Some(*binding),
        _ => None,
    });
    assert_eq!(binding, Some(None));
}
