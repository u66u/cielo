use cielo::common::diagnostics::DiagnosticBag;
use cielo::common::ids::{EffectLabelId, SourceId};
use cielo::common::span::Span;
use cielo::common::symbols::Interner;
use cielo::frontend::parser::parse_source;
use cielo::ir::core::{BinaryOp, CoreProgram, ExprKind, ExprNode, Literal};
use cielo::passes::lowering::{LowerConfig, lower_program};
use cielo::sema::typecheck::typecheck_core;

#[test]
fn infers_simple_binary_types() {
    let mut program = CoreProgram::new();
    let lhs = program.push_expr(ExprNode {
        span: Span::synthetic(),
        kind: ExprKind::Literal(Literal::Int(1)),
    });
    let rhs = program.push_expr(ExprNode {
        span: Span::synthetic(),
        kind: ExprKind::Literal(Literal::Int(2)),
    });
    program.push_expr(ExprNode {
        span: Span::synthetic(),
        kind: ExprKind::Binary {
            op: BinaryOp::Add,
            lhs,
            rhs,
        },
    });

    let mut diagnostics = DiagnosticBag::default();
    let sema = typecheck_core(&program, &mut diagnostics);
    assert!(sema.type_of_expr.iter().all(Option::is_some));
}

#[test]
fn computes_stmt_effect_rows_for_perform() {
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
    let mut diagnostics = DiagnosticBag::default();
    let sema = typecheck_core(&lowered.program, &mut diagnostics);
    assert!(
        sema.effects_of_stmt
            .iter()
            .any(|row| row.contains(EffectLabelId::from_u32(0)))
    );
}

#[test]
fn handle_stmt_discharge_removes_handled_effect_from_root_flow() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  let x = handle { do Console.print("x"); 7 } with Console {
    | print(s) => 0
  };
  x
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let sema = typecheck_core(&lowered.program, &mut diagnostics);
    let main_body = lowered.program.functions()[0].body;
    let root_row = &sema.effects_of_stmt[main_body.index()];
    assert!(!root_row.contains(EffectLabelId::from_u32(0)));
}

#[test]
fn call_stmt_effects_include_declared_function_effects() {
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
    let mut diagnostics = DiagnosticBag::default();
    let sema = typecheck_core(&lowered.program, &mut diagnostics);
    let main_body = lowered
        .program
        .functions()
        .iter()
        .find(|f| interner.resolve(f.name) == Some("main"))
        .expect("main")
        .body;
    assert!(sema.effects_of_stmt[main_body.index()].contains(EffectLabelId::from_u32(0)));
}

#[test]
fn infers_types_for_constructor_expressions() {
    let src = r#"
enum Option { Some(Int), None }
struct Pair { a: Int, b: Int }
fn main() -> Int {
  let x = Some(1);
  let y = Pair(1, 2);
  0
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let sema = typecheck_core(&lowered.program, &mut diagnostics);
    let all_ctors_typed = lowered
        .program
        .exprs()
        .iter()
        .enumerate()
        .filter(|(_, expr)| {
            matches!(
                expr.kind,
                ExprKind::MakeEnum { .. } | ExprKind::MakeStruct { .. }
            )
        })
        .all(|(idx, _)| sema.type_of_expr[idx].is_some());
    assert!(all_ctors_typed);
}

#[test]
fn infers_simple_function_call_chain_without_warnings() {
    let src = r#"
fn add(a: Int, b: Int) -> Int {
  a + b
}
fn main() -> Int {
  let z = add(1, 2);
  z
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let _ = typecheck_core(&lowered.program, &mut diagnostics);
    assert!(!diagnostics.has_errors());
    assert!(diagnostics.entries().is_empty());
}

#[test]
fn reports_effect_argument_type_mismatch() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  do Console.print(42);
  0
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let _ = typecheck_core(&lowered.program, &mut diagnostics);
    assert!(diagnostics
        .entries()
        .iter()
        .any(|d| d.code == "TYPE_EFFECT_ARG_MISMATCH"));
}

#[test]
fn reports_unknown_effect_operation_in_typecheck() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  do Console.missing("x");
  0
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let _ = typecheck_core(&lowered.program, &mut diagnostics);
    assert!(diagnostics
        .entries()
        .iter()
        .any(|d| d.code == "TYPE_UNKNOWN_EFFECT_OP"));
}

#[test]
fn reports_handler_clause_arity_in_typecheck() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  let x = handle { do Console.print("x"); 7 } with Console {
    | print(a, b) => 0
  };
  x
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let _ = typecheck_core(&lowered.program, &mut diagnostics);
    assert!(diagnostics
        .entries()
        .iter()
        .any(|d| d.code == "TYPE_BAD_HANDLER_CLAUSE_ARITY"));
}
