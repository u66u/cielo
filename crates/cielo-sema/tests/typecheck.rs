use cielo_base::diagnostics::{DiagnosticBag, Severity};
use cielo_base::{EffectLabelId, Interner, SourceId, Span};
use cielo_frontend::parser::parse_source;
use cielo_ir::core::{BinaryOp, CoreProgram, ExprKind, ExprNode, Literal, StmtKind};
use cielo_ir::effect::{CapabilityLevel, EffectFlags, SortedEffectRow, is_thunkable};
use cielo_ir::ownership::OwnershipClass;
use cielo_lowering::{LowerConfig, lower_program};
use cielo_sema::typecheck::typecheck_core;

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
    let sema = typecheck_core(&program, &mut diagnostics, None);
    assert!(sema.type_of_expr.iter().all(Option::is_some));
}

#[test]
fn classifies_expr_and_var_ownership_for_managed_and_trivial_values() {
    let src = r#"
enum Boxed { Wrap(Int) }
fn main() -> Int {
  let n = 1;
  let boxed = Wrap(n);
  n
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let sema = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));

    let int_literal_expr = lowered
        .program
        .exprs()
        .iter()
        .enumerate()
        .find_map(|(idx, expr)| {
            matches!(expr.kind, ExprKind::Literal(Literal::Int(_))).then_some(idx)
        })
        .expect("int literal");
    let ctor_expr = lowered
        .program
        .exprs()
        .iter()
        .enumerate()
        .find_map(|(idx, expr)| matches!(expr.kind, ExprKind::MakeEnum { .. }).then_some(idx))
        .expect("enum constructor expr");

    assert_eq!(
        sema.ownership_of_expr[int_literal_expr],
        OwnershipClass::Trivial
    );
    assert_eq!(sema.ownership_of_expr[ctor_expr], OwnershipClass::Managed);

    let mut literal_binding = None;
    let mut ctor_binding = None;
    for stmt in lowered.program.stmts() {
        if let StmtKind::Let { binding, value, .. } = &stmt.kind {
            let kind = lowered
                .program
                .expr(*value)
                .map(|expr| &expr.kind)
                .expect("let value expr");
            if matches!(kind, ExprKind::Literal(Literal::Int(_))) {
                literal_binding = Some(*binding);
            }
            if matches!(
                kind,
                ExprKind::MakeEnum { .. } | ExprKind::MakeStruct { .. }
            ) {
                ctor_binding = Some(*binding);
            }
        }
    }

    let literal_binding = literal_binding.expect("literal binding var");
    let ctor_binding = ctor_binding.expect("ctor binding var");
    assert_eq!(
        sema.ownership_of_var.get(&literal_binding).copied(),
        Some(OwnershipClass::Trivial)
    );
    assert_eq!(
        sema.ownership_of_var.get(&ctor_binding).copied(),
        Some(OwnershipClass::Managed)
    );
}

#[test]
fn ownership_table_covers_relevant_var_binders() {
    let src = r#"
effect LocalState { fn tick() -> Int }
enum Option { Some(Int), None }
fn helper(a: Int) -> Int { a }
fn main() -> Int {
  let local = handle {
    let t = do LocalState.tick();
    t
  } with LocalState {
    | tick(resume) => {
      let resumed = resume(3);
      resumed
    }
  };
  let opt = Some(local);
  let out = match opt {
    Some(x) => x,
    None => 0,
  };
  let y = helper(out);
  y
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(1), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let sema = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));

    assert_eq!(sema.ownership_of_expr.len(), lowered.program.exprs().len());

    let mut required = std::collections::HashSet::new();
    for function in lowered.program.functions() {
        for param in function.params.iter().copied() {
            required.insert(param);
        }
    }
    for handler in lowered.program.handlers() {
        for clause in &handler.clauses {
            for param in clause.params.iter().copied() {
                required.insert(param);
            }
            if let Some(resume) = clause.resume_param {
                required.insert(resume);
            }
        }
    }
    for stmt in lowered.program.stmts() {
        match &stmt.kind {
            StmtKind::Let { binding, .. } | StmtKind::Val { binding, .. } => {
                required.insert(*binding);
            }
            StmtKind::Call { result, .. } | StmtKind::Resume { result, .. } => {
                required.insert(*result);
            }
            StmtKind::Perform {
                result: Some(result),
                ..
            } => {
                required.insert(*result);
            }
            StmtKind::Perform { result: None, .. }
            | StmtKind::Return(_)
            | StmtKind::If { .. }
            | StmtKind::Match { .. }
            | StmtKind::Handle { .. }
            | StmtKind::Stage { .. }
            | StmtKind::Hole { .. }
            | StmtKind::Error(_) => {}
        }
        if let StmtKind::Match { arms, .. } = &stmt.kind {
            for arm in arms {
                for binder in arm.binders.iter().copied() {
                    required.insert(binder);
                }
            }
        }
    }

    let missing = required
        .iter()
        .copied()
        .filter(|var| sema.ownership_of_var.get(var).is_none())
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "ownership table must classify every relevant var binder, missing {:?}",
        missing
    );
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
    let sema = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));
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
    let sema = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));
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
    let sema = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));
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
    let sema = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));
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
    let _ = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));
    assert!(!diagnostics.has_errors());
    assert!(diagnostics.entries().is_empty());
}

#[test]
fn types_struct_field_access_by_name() {
    let src = r#"
struct Pair { a: Int, b: Int }
fn second(p: Pair) -> Int {
  p.b
}
fn main() -> Int {
  let p = Pair(10, 32);
  let x = p.a;
  x + second(p)
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let sema = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));
    assert!(
        !diagnostics.has_errors(),
        "{:?}",
        diagnostics
            .entries()
            .iter()
            .map(|d| d.code.to_owned())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        sema.field_index_of_expr.len(),
        2,
        "both projections should resolve to a positional index"
    );
}

#[test]
fn reports_access_to_an_unknown_field() {
    let src = r#"
struct Pair { a: Int, b: Int }
fn main() -> Int {
  let p = Pair(1, 2);
  p.c
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let _ = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));
    assert!(
        diagnostics
            .entries()
            .iter()
            .any(|d| d.code == "TYPE_UNKNOWN_FIELD")
    );
}

#[test]
fn reports_match_missing_a_variant_without_a_default() {
    let src = r#"
enum Shape { Circle(Int), Square(Int), Tri(Int) }
fn area(s: Shape) -> Int {
  match s {
    Circle(r) => r,
    Square(w) => w
  }
}
fn main() -> Int {
  area(Tri(7))
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let _ = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));
    assert!(
        diagnostics
            .entries()
            .iter()
            .any(|d| d.code == "TYPE_MATCH_NOT_EXHAUSTIVE"),
        "an uncovered variant falls through to a unit block at runtime"
    );
}

#[test]
fn accepts_match_covering_every_variant() {
    let src = r#"
enum Shape { Circle(Int), Square(Int), Tri(Int) }
fn area(s: Shape) -> Int {
  match s {
    Circle(r) => r,
    Square(w) => w,
    Tri(b) => b
  }
}
fn main() -> Int {
  area(Tri(7))
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let _ = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));
    assert!(
        !diagnostics
            .entries()
            .iter()
            .any(|d| d.code == "TYPE_MATCH_NOT_EXHAUSTIVE"),
        "got {:?}",
        diagnostics
            .entries()
            .iter()
            .map(|d| d.code.to_owned())
            .collect::<Vec<_>>()
    );
}

#[test]
fn accepts_two_enums_sharing_a_variant_name() {
    let src = r#"
enum Shape { Empty, Circle(Int) }
enum Buffer { Empty, Full(Int) }
fn shape_code(s: Shape) -> Int {
  match s {
    Empty => 1,
    Circle(r) => r
  }
}
fn buffer_code(b: Buffer) -> Int {
  match b {
    Empty => 10,
    Full(n) => n
  }
}
fn main() -> Int {
  let s: Shape = Empty;
  let b: Buffer = Empty;
  shape_code(s) + buffer_code(b)
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let _ = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));
    assert!(
        !diagnostics.has_errors(),
        "{:?}",
        diagnostics
            .entries()
            .iter()
            .map(|d| d.code.to_owned())
            .collect::<Vec<_>>()
    );
}

#[test]
fn reports_a_match_arm_from_another_enum_against_the_scrutinee() {
    let src = r#"
enum Shape { Empty, Circle(Int) }
enum Buffer { Empty, Full(Int) }
fn shape_code(s: Shape) -> Int {
  match s {
    Empty => 1,
    Full(n) => n
  }
}
fn main() -> Int {
  shape_code(Empty)
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let _ = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));
    assert!(
        diagnostics
            .entries()
            .iter()
            .any(|d| d.code == "TYPE_MATCH_SCRUTINEE_MISMATCH"),
        "{:?}",
        diagnostics
            .entries()
            .iter()
            .map(|d| d.code.to_owned())
            .collect::<Vec<_>>()
    );
}

#[test]
fn reports_effect_performed_without_declaration() {
    let src = r#"
effect Console { fn print(s: String) -> Int }
fn helper() -> Int {
  do Console.print("side effect");
  1
}
fn main() -> Int {
  helper()
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let _ = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));
    assert!(
        diagnostics
            .entries()
            .iter()
            .any(|d| d.code == "SEMA_UNDECLARED_EFFECT"),
        "performing an effect outside the declared row must be an error, \
         otherwise dead-code elimination silently drops the perform"
    );
}

#[test]
fn accepts_effect_listed_in_the_declared_row() {
    let src = r#"
effect Console { fn print(s: String) -> Int }
fn helper() -> Int with Console {
  do Console.print("side effect");
  1
}
fn main() -> Int with Console {
  helper()
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let _ = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));
    assert!(
        !diagnostics
            .entries()
            .iter()
            .any(|d| d.code == "SEMA_UNDECLARED_EFFECT"),
        "a declared effect must not be reported as undeclared"
    );
}

#[test]
fn accepts_effect_discharged_by_an_enclosing_handler() {
    let src = r#"
effect Console { fn print(s: String) -> Int }
fn helper() -> Int with Console {
  do Console.print("side effect");
  1
}
fn main() -> Int {
  let out = handle { helper() } with Console {
    | print(s) => 100
  };
  out
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let _ = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));
    assert!(
        !diagnostics
            .entries()
            .iter()
            .any(|d| d.code == "SEMA_UNDECLARED_EFFECT"),
        "`main` does not declare Console because the handler discharges it"
    );
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
    let _ = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));
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
    let _ = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));
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
    let _ = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));
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
    let _ = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));
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
    let sema = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));

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
            &SortedEffectRow::singleton(EffectLabelId::from_u32(0)),
            &sema.effect_properties
        ),
        "IO effects should not be thunkable"
    );
    assert!(
        is_thunkable(
            &SortedEffectRow::singleton(EffectLabelId::from_u32(2)),
            &sema.effect_properties
        ),
        "ComptimeReadFiles should stay thunkable for staging decisions"
    );
}

#[test]
fn generic_function_signature_allows_distinct_call_instantiations() {
    let src = r#"
fn id[A](x: A) -> A {
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
    let _ = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));
    assert!(
        diagnostics
            .entries()
            .iter()
            .all(|entry| entry.severity != Severity::Error),
        "generic signature should instantiate per-call without type errors: {:?}",
        diagnostics.entries()
    );
}

#[test]
fn same_generic_parameter_rejects_mismatched_call_types() {
    let src = r#"
fn pair_left[A](a: A, b: A) -> A {
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
    let _ = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));
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
fn first[A, B](a: A, b: B) -> A {
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
    let _ = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));
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
    let _ = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));
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
    let _ = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));
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
    let _ = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));
    assert!(
        diagnostics
            .entries()
            .iter()
            .any(|entry| entry.code == "TYPE_UNKNOWN_ADT_FIELD_TYPE"),
        "unknown ADT field types should be diagnosed"
    );
}

fn diagnose(src: &str) -> DiagnosticBag {
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let mut diagnostics = DiagnosticBag::default();
    let _ = typecheck_core(&lowered.program, &mut diagnostics, Some(&interner));
    diagnostics
}

fn has_code(diagnostics: &DiagnosticBag, code: &str) -> bool {
    diagnostics.entries().iter().any(|entry| entry.code == code)
}

#[test]
fn undeclared_type_name_is_an_error_not_an_implicit_type_variable() {
    let diagnostics = diagnose(
        r#"
fn f(x: Itn) -> Int {
  0
}
"#,
    );
    assert!(
        has_code(&diagnostics, "TYPE_UNKNOWN_TYPE_NAME"),
        "a capitalized name that is neither declared nor a type parameter must be rejected: {:?}",
        diagnostics.entries()
    );
}

#[test]
fn generic_enum_instantiates_per_type_argument() {
    let diagnostics = diagnose(
        r#"
enum Option[T] { Some(T), None }
fn unwrap_or[T](opt: Option[T], fallback: T) -> T {
  match opt {
    | Some(v) => v
    | None => fallback
  }
}
fn main() -> Int {
  unwrap_or(Some(7), 0)
}
"#,
    );
    assert!(
        !diagnostics.has_errors(),
        "a generic enum used at one type argument should typecheck: {:?}",
        diagnostics.entries()
    );
}

#[test]
fn generic_enum_rejects_mismatched_type_arguments() {
    let diagnostics = diagnose(
        r#"
enum Option[T] { Some(T), None }
fn unwrap_or[T](opt: Option[T], fallback: T) -> T {
  match opt {
    | Some(v) => v
    | None => fallback
  }
}
fn main() -> Int {
  unwrap_or(Some(true), 0)
}
"#,
    );
    assert!(
        has_code(&diagnostics, "TYPE_CALL_ARG_MISMATCH"),
        "Option[Bool] must not satisfy Option[Int]: {:?}",
        diagnostics.entries()
    );
}

#[test]
fn generic_struct_field_access_resolves_through_the_instantiation() {
    let diagnostics = diagnose(
        r#"
struct Pair[A, B] { first: A, second: B }
fn left[A, B](p: Pair[A, B]) -> A {
  p.first
}
fn main() -> Int {
  left(Pair(3, true))
}
"#,
    );
    assert!(
        !diagnostics.has_errors(),
        "field access on a generic struct should resolve: {:?}",
        diagnostics.entries()
    );
}

#[test]
fn a_let_annotation_supplies_an_otherwise_undetermined_type_argument() {
    let diagnostics = diagnose(
        r#"
enum Option[T] { Some(T), None }
fn none[T]() -> Option[T] {
  None()
}
fn main() -> Int {
  let x: Option[Int] = none();
  0
}
"#,
    );
    assert!(
        !diagnostics.has_errors(),
        "the annotation is the only thing that determines T: {:?}",
        diagnostics.entries()
    );
}

#[test]
fn a_let_annotation_that_contradicts_the_value_is_rejected() {
    let diagnostics = diagnose(
        r#"
fn main() -> Int {
  let x: Bool = 1;
  0
}
"#,
    );
    assert!(
        has_code(&diagnostics, "TYPE_LET_ANNOTATION_MISMATCH"),
        "declared and inferred types must agree: {:?}",
        diagnostics.entries()
    );
}

#[test]
fn a_let_annotation_naming_an_enclosing_type_parameter_resolves() {
    let diagnostics = diagnose(
        r#"
fn identity[T](x: T) -> T {
  let y: T = x;
  y
}
fn main() -> Int {
  identity(3)
}
"#,
    );
    assert!(
        !diagnostics.has_errors(),
        "`T` in a body annotation is the function's own parameter: {:?}",
        diagnostics.entries()
    );
}

#[test]
fn an_unknown_let_annotation_type_name_is_rejected() {
    let diagnostics = diagnose(
        r#"
fn main() -> Int {
  let x: Itn = 1;
  0
}
"#,
    );
    assert!(
        has_code(&diagnostics, "TYPE_UNKNOWN_TYPE_NAME"),
        "an annotation names a type that must exist: {:?}",
        diagnostics.entries()
    );
}

#[test]
fn wrong_type_argument_count_is_rejected() {
    let diagnostics = diagnose(
        r#"
enum Option[T] { Some(T), None }
fn f(opt: Option[Int, Bool]) -> Int {
  0
}
"#,
    );
    assert!(
        has_code(&diagnostics, "TYPE_BAD_TYPE_ARG_COUNT"),
        "arity of a type application must be checked: {:?}",
        diagnostics.entries()
    );
}

#[test]
fn a_type_mismatch_names_the_declarations_it_compared() {
    let diagnostics = diagnose(
        r#"
struct Meters { v: Int }
struct Feet { v: Int }
fn walk(d: Meters) -> Int {
  0
}
fn main() -> Int {
  walk(Feet(1))
}
"#,
    );
    let mismatch = message_for(&diagnostics, "TYPE_CALL_ARG_MISMATCH");
    assert!(
        mismatch.contains("Meters") && mismatch.contains("Feet"),
        "a mismatch between two ADTs must name them: {mismatch}"
    );
}

#[test]
fn an_ambiguous_match_variant_names_its_candidate_enums() {
    let diagnostics = diagnose(
        r#"
enum Shape { Empty, Circle(Int) }
enum Buffer { Empty, Full(Int) }
fn pick[T](x: T) -> Int {
  match x {
    | Empty => 1
    | _ => 0
  }
}
fn main() -> Int {
  0
}
"#,
    );
    let ambiguity = message_for(&diagnostics, "TYPE_AMBIGUOUS_MATCH_VARIANT");
    assert!(
        ambiguity.contains("Shape") && ambiguity.contains("Buffer"),
        "the candidates are the whole point of this diagnostic: {ambiguity}"
    );
}

/// A row is a set, so writing one in the other order has to produce the same
/// type. Both occurrences must intern to a single `TypeId` or the closure never
/// matches the parameter it is passed to.
#[test]
fn an_effect_row_unifies_regardless_of_the_order_it_is_written_in() {
    let diagnostics = diagnose(
        r#"
effect St { fn get() -> Int }
effect Log { fn note(s: String) -> Int }
fn apply(f: Fn(Int) -> Int with St + Log, x: Int) -> Int with St, Log {
  f(x)
}
fn main() -> Int with St, Log {
  let g: Fn(Int) -> Int with Log + St = |n| n;
  apply(g, 1)
}
"#,
    );
    assert!(
        !diagnostics.has_errors(),
        "`with St + Log` and `with Log + St` are one type: {:?}",
        diagnostics.entries()
    );
}

#[test]
fn a_closure_with_a_wider_row_than_the_parameter_is_rejected() {
    let diagnostics = diagnose(
        r#"
effect St { fn get() -> Int }
fn apply(f: Fn(Int) -> Int, x: Int) -> Int {
  f(x)
}
fn main() -> Int with St {
  let g: Fn(Int) -> Int with St = |n| n;
  apply(g, 1)
}
"#,
    );
    let mismatch = message_for(&diagnostics, "TYPE_CALL_ARG_MISMATCH");
    assert!(
        mismatch.ends_with("Fn(Int) -> Int with St vs Fn(Int) -> Int"),
        "a row mismatch must print both rows, not `Function vs Function`: {mismatch}"
    );
}

/// The row on the callee's type is the only record of what a call through a
/// value may perform; the caller's own body row says nothing about it.
#[test]
fn calling_a_value_whose_row_the_caller_does_not_declare_is_rejected() {
    let diagnostics = diagnose(
        r#"
effect St { fn get() -> Int }
fn main() -> Int {
  let g: Fn(Int) -> Int with St = |n| n;
  g(1)
}
"#,
    );
    let message = message_for(&diagnostics, "SEMA_UNDECLARED_CALL_EFFECT");
    assert!(
        message.contains("St") && message.contains("main"),
        "the diagnostic must name the effect and the function missing it: {message}"
    );
}

/// `cv_ordering` and `ct_common` both order chars by scalar value. Arithmetic
/// stays numeric-only, and a mixed pair is still a mismatch.
#[test]
fn chars_are_ordered_but_not_arithmetic() {
    let ordered = diagnose(
        r#"
fn main() -> Int {
  if 'a' < 'b' { 1 } else { 0 }
}
"#,
    );
    assert!(
        !has_code(&ordered, "TYPE_NUMERIC_REQUIRED"),
        "char ordering must typecheck: {ordered:?}"
    );

    for source in [
        "fn main() -> Int { let x = 'a' + 'b'; 0 }",
        "fn main() -> Int { if 'a' < 1 { 1 } else { 0 } }",
    ] {
        let rejected = diagnose(source);
        assert!(
            has_code(&rejected, "TYPE_NUMERIC_REQUIRED"),
            "expected a numeric-required error for `{source}`: {rejected:?}"
        );
    }
}

/// A handler discharges the callee's row the same way it discharges a direct
/// perform, so only what escapes it belongs in the caller's own row.
#[test]
fn calling_a_value_under_a_handler_for_its_row_is_accepted() {
    let diagnostics = diagnose(
        r#"
effect St { fn get() -> Int }
fn main() -> Int {
  let g: Fn(Int) -> Int with St = |n| n;
  handle { g(1) } with St {
    | get(resume) => resume(41)
  }
}
"#,
    );
    assert!(
        !has_code(&diagnostics, "SEMA_UNDECLARED_CALL_EFFECT"),
        "a call under a handler for its effect must not require the row: {diagnostics:?}"
    );
}

#[test]
fn an_unknown_effect_name_in_a_row_is_rejected() {
    let diagnostics = diagnose(
        r#"
fn apply(f: Fn(Int) -> Int with Nope, x: Int) -> Int {
  f(x)
}
"#,
    );
    let message = message_for(&diagnostics, "TYPE_UNKNOWN_EFFECT_IN_ROW");
    assert!(
        message.contains("Nope"),
        "stage 1 has no row variables, so an unknown name is a typo and must be named: {message}"
    );
}

#[test]
fn a_function_type_without_a_row_still_checks() {
    let diagnostics = diagnose(
        r#"
fn apply(f: Fn(Int) -> Int, x: Int) -> Int {
  f(x)
}
fn make_adder(n: Int) -> Fn(Int) -> Int {
  |x| x + n
}
fn main() -> Int {
  apply(make_adder(2), 3)
}
"#,
    );
    assert!(
        !diagnostics.has_errors(),
        "an effect-free function type must be unaffected: {:?}",
        diagnostics.entries()
    );
}

fn message_for(diagnostics: &DiagnosticBag, code: &str) -> String {
    diagnostics
        .entries()
        .iter()
        .find(|entry| entry.code == code)
        .unwrap_or_else(|| panic!("expected {code}, got {:?}", diagnostics.entries()))
        .message
        .clone()
}
