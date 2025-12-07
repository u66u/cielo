use cielo::common::diagnostics::DiagnosticBag;
use cielo::common::ids::{EffectLabelId, SourceId};
use cielo::common::span::Span;
use cielo::common::symbols::Interner;
use cielo::frontend::parser::parse_source;
use cielo::ir::core::{BinaryOp, CoreProgram, ExprKind, ExprNode, Literal};
use cielo::passes::lowering::{LowerConfig, lower_program};
use cielo::sema::effect::{CapabilityLevel, EffectFlags, is_thunkable};
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
    assert!(
        diagnostics
            .entries()
            .iter()
            .any(|d| d.code == "TYPE_EFFECT_ARG_MISMATCH")
    );
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
    assert!(
        diagnostics
            .entries()
            .iter()
            .any(|d| d.code == "TYPE_UNKNOWN_EFFECT_OP")
    );
}

#[test]
fn reports_handler_clause_arity_in_typecheck() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  let x = handle { do Console.print("x"); 7 } with Console {
    | print(a, b, c) => 0
  };
  x
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let _ = typecheck_core(&lowered.program, &mut diagnostics);
    assert!(
        diagnostics
            .entries()
            .iter()
            .any(|d| d.code == "TYPE_BAD_HANDLER_CLAUSE_ARITY")
    );
}

#[test]
fn accepts_resumptive_handler_clause_arity() {
    let src = r#"
effect LocalState { fn tick() -> Int }
fn main() -> Int {
  let x = handle { do LocalState.tick(); 7 } with LocalState {
    | tick(resume) => resume(1)
  };
  x
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let _ = typecheck_core(&lowered.program, &mut diagnostics);
    assert!(
        !diagnostics
            .entries()
            .iter()
            .any(|d| d.code == "TYPE_BAD_HANDLER_CLAUSE_ARITY")
    );
}

#[test]
fn classifies_effect_properties_and_exposes_thunkability() {
    let src = r#"
effect Console { fn print(s: String) -> () }
effect SharedState { fn get() -> Int }
effect ComptimeReadFiles { fn read(path: String) -> String }
fn main() -> Int {
  0
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let sema = typecheck_core(&lowered.program, &mut diagnostics);

    let console = sema
        .effect_properties
        .get(&EffectLabelId::from_u32(0))
        .copied()
        .expect("console effect");
    assert_eq!(console.level, CapabilityLevel::Io);
    assert!(console.flags.contains(EffectFlags::OPAQUE_FOR_STAGING));

    let shared = sema
        .effect_properties
        .get(&EffectLabelId::from_u32(1))
        .copied()
        .expect("shared-state effect");
    assert_eq!(shared.level, CapabilityLevel::SharedState);
    assert!(shared.flags.contains(EffectFlags::SHARED_STATE));

    let ct_files = sema
        .effect_properties
        .get(&EffectLabelId::from_u32(2))
        .copied()
        .expect("ct-files effect");
    assert!(ct_files.flags.contains(EffectFlags::CT_ONLY));

    assert!(
        !is_thunkable(
            &cielo::sema::effect::SortedEffectRow::singleton(EffectLabelId::from_u32(0)),
            &sema.effect_properties
        ),
        "IO effects should not be thunkable"
    );
    assert!(
        is_thunkable(
            &cielo::sema::effect::SortedEffectRow::singleton(EffectLabelId::from_u32(2)),
            &sema.effect_properties
        ),
        "ComptimeReadFiles should stay thunkable for staging decisions"
    );
}

#[test]
fn generic_function_signature_allows_distinct_call_instantiations() {
    let src = r#"
fn id(x: A) -> A {
  x
}
fn main() -> Int {
  let a = id(1);
  let b = id(true);
  a
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let _ = typecheck_core(&lowered.program, &mut diagnostics);
    assert!(
        diagnostics
            .entries()
            .iter()
            .all(|entry| entry.severity != cielo::common::diagnostics::Severity::Error),
        "generic signature should instantiate per-call without type errors: {:?}",
        diagnostics.entries()
    );
}

#[test]
fn same_generic_parameter_rejects_mismatched_call_types() {
    let src = r#"
fn pair_left(a: A, b: A) -> A {
  a
}
fn main() -> Int {
  let x = pair_left(1, true);
  x
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let _ = typecheck_core(&lowered.program, &mut diagnostics);
    assert!(
        diagnostics
            .entries()
            .iter()
            .any(|entry| entry.code == "TYPE_CALL_ARG_MISMATCH"),
        "shared generic parameter should force argument type agreement"
    );
}

#[test]
fn different_generic_parameters_do_not_unify_with_each_other() {
    let src = r#"
fn first(a: A, b: B) -> A {
  a
}
fn main() -> Int {
  let x = first(7, true);
  x
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let _ = typecheck_core(&lowered.program, &mut diagnostics);
    assert!(
        !diagnostics
            .entries()
            .iter()
            .any(|entry| entry.code == "TYPE_CALL_ARG_MISMATCH"),
        "distinct generic symbols should remain independent"
    );
}

#[test]
fn struct_constructor_field_mismatch_reports_type_error() {
    let src = r#"
struct Pair { a: Int, b: Bool }
fn main() -> Int {
  let x = Pair(1, 2);
  0
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let _ = typecheck_core(&lowered.program, &mut diagnostics);
    assert!(
        diagnostics
            .entries()
            .iter()
            .any(|entry| entry.code == "TYPE_STRUCT_FIELD_MISMATCH"),
        "struct field type mismatch should be diagnosed"
    );
}

#[test]
fn enum_constructor_field_mismatch_reports_type_error() {
    let src = r#"
enum OptionI { Some(Int), None }
fn main() -> Int {
  let x = Some(true);
  0
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let _ = typecheck_core(&lowered.program, &mut diagnostics);
    assert!(
        diagnostics
            .entries()
            .iter()
            .any(|entry| entry.code == "TYPE_ENUM_FIELD_MISMATCH"),
        "enum constructor field type mismatch should be diagnosed"
    );
}

#[test]
fn unresolved_adt_field_type_is_rejected_during_typecheck() {
    let src = r#"
struct Broken { x: MissingType }
fn main() -> Int {
  0
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let _ = typecheck_core(&lowered.program, &mut diagnostics);
    assert!(
        diagnostics
            .entries()
            .iter()
            .any(|entry| entry.code == "TYPE_UNKNOWN_ADT_FIELD_TYPE"),
        "unknown ADT field types should be diagnosed"
    );
}
