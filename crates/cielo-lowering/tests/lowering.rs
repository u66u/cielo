use cielo_base::{Interner, SourceId};
use cielo_frontend::parser::parse_source;
use cielo_ir::core::{BinaryOp, CoreTypeRef, ExprKind, Literal, StmtKind};
use cielo_lowering::{LowerConfig, lower_program};

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
fn lowers_resumptive_handler_clause_with_explicit_resume_stmt() {
    let src = r#"
effect LocalState { fn tick() -> Int }
fn main() -> Int {
  let x = handle { do LocalState.tick(); 7 } with LocalState {
    | tick(resume) => resume(0)
  };
  x
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    assert!(!lowered.diagnostics.has_errors());
    assert_eq!(lowered.program.handlers().len(), 1);

    let clause = &lowered.program.handlers()[0].clauses[0];
    assert_eq!(clause.params.len(), 0, "tick has no value parameters");
    assert!(
        clause.resume_param.is_some(),
        "resume binder should be captured separately"
    );
    assert!(
        stmt_graph_contains_resume(&lowered.program, clause.body),
        "resume call should lower into explicit Core resume stmt"
    );
}

fn stmt_graph_contains_resume(
    program: &cielo_ir::core::CoreProgram,
    root: cielo_base::StmtId,
) -> bool {
    let mut stack = vec![root];
    let mut seen = std::collections::HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if matches!(stmt.kind, StmtKind::Resume { .. }) {
            return true;
        }
        for child in stmt.child_stmts() {
            stack.push(child);
        }
    }
    false
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
fn lowers_if_expression_into_core_if_stmt() {
    let src = r#"
fn main() -> Int {
  let x = if true { 1 } else { 2 };
  x
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    assert!(!lowered.diagnostics.has_errors());
    let main = lowered
        .program
        .functions()
        .iter()
        .find(|f| interner.resolve(f.name) == Some("main"))
        .expect("main");

    let mut cursor = main.body;
    let mut saw_if_stmt = false;
    while let Some(stmt) = lowered.program.stmt(cursor) {
        match &stmt.kind {
            StmtKind::Val { value, next, .. } => {
                if matches!(
                    lowered.program.stmt(*value).map(|node| &node.kind),
                    Some(StmtKind::If { .. })
                ) {
                    saw_if_stmt = true;
                }
                cursor = *next;
            }
            StmtKind::Let { next, .. } => cursor = *next,
            StmtKind::Return(_) => break,
            _ => break,
        }
    }
    assert!(saw_if_stmt);
}

#[test]
fn lowers_match_expression_into_core_match_stmt() {
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
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    assert!(!lowered.diagnostics.has_errors());
    let main = lowered
        .program
        .functions()
        .iter()
        .find(|f| interner.resolve(f.name) == Some("main"))
        .expect("main");

    let mut cursor = main.body;
    let mut saw_match_stmt = false;
    while let Some(stmt) = lowered.program.stmt(cursor) {
        match &stmt.kind {
            StmtKind::Val { value, next, .. } => {
                if matches!(
                    lowered.program.stmt(*value).map(|node| &node.kind),
                    Some(StmtKind::Match { .. })
                ) {
                    saw_match_stmt = true;
                }
                cursor = *next;
            }
            StmtKind::Let { next, .. } => cursor = *next,
            StmtKind::Return(_) => break,
            _ => break,
        }
    }
    assert!(saw_match_stmt);
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
fn reports_resume_capture_as_value_in_handler_clause() {
    let src = r#"
effect LocalState { fn tick() -> Int }
fn main() -> Int {
  let x = handle { do LocalState.tick(); 7 } with LocalState {
    | tick(resume) => {
      let k = resume;
      0
    }
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
            .any(|diag| diag.code == "LOWER_RESUME_VALUE_ESCAPE"),
        "capturing resume as a value should be rejected explicitly in v1"
    );
}

#[test]
fn reports_resume_passing_as_function_argument() {
    let src = r#"
effect LocalState { fn tick() -> Int }
fn sink(x: Int) -> Int {
  x
}
fn main() -> Int {
  let x = handle { do LocalState.tick(); 7 } with LocalState {
    | tick(resume) => sink(resume)
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
            .any(|diag| diag.code == "LOWER_RESUME_VALUE_ESCAPE"),
        "passing resume as a value should be rejected explicitly in v1"
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

#[test]
fn lowers_enum_variant_constructor_call() {
    let src = r#"
enum Option { Some(Int), None }
fn main() -> Int {
  let v = Some(1);
  0
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    assert_eq!(lowered.program.enums().len(), 1);

    let has_ctor = lowered
        .program
        .exprs()
        .iter()
        .any(|expr| matches!(expr.kind, cielo_ir::core::ExprKind::MakeEnum { .. }));
    assert!(has_ctor);
}

#[test]
fn lowers_a_bare_nullary_enum_variant() {
    let src = r#"
enum OptInt { SomeI(Int), NoneI }
fn main() -> Int {
  let a = NoneI;
  0
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    assert!(!lowered.diagnostics.has_errors());

    let variant = interner.intern("NoneI");
    assert!(lowered.program.exprs().iter().any(|expr| matches!(
        &expr.kind,
        cielo_ir::core::ExprKind::MakeEnum { variant: v, fields, .. }
            if *v == variant && fields.is_empty()
    )));
}

#[test]
fn a_local_binding_shadows_a_nullary_variant_name() {
    let src = r#"
enum OptInt { SomeI(Int), NoneI }
fn main() -> Int {
  let NoneI = 7;
  NoneI
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    assert!(!lowered.diagnostics.has_errors());
    assert!(
        !lowered
            .program
            .exprs()
            .iter()
            .any(|expr| matches!(expr.kind, cielo_ir::core::ExprKind::MakeEnum { .. })),
        "the local binding must win over the variant of the same name"
    );
}

#[test]
fn reports_arity_for_a_bare_non_nullary_variant() {
    let src = r#"
enum OptInt { SomeI(Int), NoneI }
fn main() -> Int {
  let a = SomeI;
  0
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let codes = diagnostic_codes(&lowered);
    assert!(
        codes.contains(&"LOWER_BAD_ENUM_CTOR_ARITY".to_owned()),
        "got {codes:?}"
    );
    assert!(!codes.contains(&"LOWER_UNKNOWN_VAR".to_owned()));
}

#[test]
fn resolves_a_shared_variant_name_through_the_expected_type() {
    let src = r#"
enum Shape { Empty, Circle(Int) }
enum Buffer { Empty, Full(Int) }
fn shape_code(s: Shape) -> Int { match s { Empty => 1, Circle(r) => r } }
fn buffer_of() -> Buffer { Empty }
fn main() -> Int {
  let b: Buffer = Empty;
  shape_code(Empty)
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    assert!(
        !lowered.diagnostics.has_errors(),
        "{:?}",
        diagnostic_codes(&lowered)
    );

    let shape = interner.intern("Shape");
    let buffer = interner.intern("Buffer");
    let empty = interner.intern("Empty");
    let owners = lowered
        .program
        .exprs()
        .iter()
        .filter_map(|expr| match &expr.kind {
            cielo_ir::core::ExprKind::MakeEnum { ty, variant, .. } if *variant == empty => {
                Some(*ty)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(owners, vec![buffer, buffer, shape]);
}

#[test]
fn reports_an_ambiguous_shared_variant_name_with_its_candidates() {
    let src = r#"
enum Shape { Empty, Circle(Int) }
enum Buffer { Empty, Full(Int) }
fn main() -> Int {
  let a = Empty;
  0
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let names = std::sync::Arc::new(interner);
    let lowered = lower_program(
        &parsed.program,
        LowerConfig::default().with_names(names.clone()),
    );
    let ambiguity = lowered
        .diagnostics
        .entries()
        .iter()
        .find(|diag| diag.code == "LOWER_AMBIGUOUS_ENUM_CTOR")
        .expect("ambiguity diagnostic");
    assert!(
        ambiguity.message.contains("Shape") && ambiguity.message.contains("Buffer"),
        "{}",
        ambiguity.message
    );
}

#[test]
fn lifts_a_lambda_into_a_function_whose_leading_params_are_its_captures() {
    let src = r#"
fn make_adder(n: Int) -> Fn(Int) -> Int {
  |x| x + n
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    assert!(
        !lowered.diagnostics.has_errors(),
        "{:?}",
        diagnostic_codes(&lowered)
    );
    assert_eq!(lowered.program.functions().len(), 2);

    let closure = lowered
        .program
        .exprs()
        .iter()
        .find_map(|expr| match &expr.kind {
            cielo_ir::core::ExprKind::MakeClosure { func, captures } => {
                Some((*func, captures.len()))
            }
            _ => None,
        })
        .expect("closure construction");
    let (func, captures) = closure;
    assert_eq!(captures, 1);
    let lifted = lowered.program.function(func).expect("lifted body");
    assert_eq!(lifted.params.len(), 2);
    // The outer `n` reaches the body as the closure's own first parameter, not
    // as the enclosing function's variable.
    let enclosing = lowered.program.functions().first().expect("make_adder");
    assert!(!lifted.params.contains(&enclosing.params[0]));
}

#[test]
fn rejects_a_closure_body_that_performs_an_effect() {
    let src = r#"
effect LocalState { fn tick() -> Int }
fn main() -> Int {
  let f = |x| { do LocalState.tick(); x };
  1
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let codes = diagnostic_codes(&lowered);
    assert!(
        codes.contains(&"LOWER_EFFECTFUL_CLOSURE".to_owned()),
        "got {codes:?}"
    );
}

#[test]
fn rejects_a_resume_captured_by_a_closure() {
    let src = r#"
effect LocalState { fn tick() -> Int }
fn apply(f: Fn(Int) -> Int, x: Int) -> Int { f(x) }
fn main() -> Int {
  handle { do LocalState.tick(); 9 } with LocalState {
    | tick(resume) => {
      let escape = |x| resume(x);
      apply(escape, 1)
    }
  }
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let codes = diagnostic_codes(&lowered);
    assert!(
        codes.contains(&"LOWER_RESUME_VALUE_ESCAPE".to_owned()),
        "got {codes:?}"
    );
}

#[test]
fn lowers_a_call_through_a_local_to_an_indirect_call() {
    let src = r#"
fn apply(f: Fn(Int) -> Int, x: Int) -> Int {
  f(x)
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    assert!(
        !lowered.diagnostics.has_errors(),
        "{:?}",
        diagnostic_codes(&lowered)
    );
    assert!(
        lowered
            .program
            .exprs()
            .iter()
            .any(|expr| matches!(expr.kind, cielo_ir::core::ExprKind::CallClosure { .. }))
    );
}

fn diagnostic_codes(lowered: &cielo_lowering::LowerOutput) -> Vec<String> {
    lowered
        .diagnostics
        .entries()
        .iter()
        .map(|diag| diag.code.to_owned())
        .collect()
}

#[test]
fn records_declared_let_types() {
    let src = r#"
fn wrap[T](x: T) -> T {
  let named: Int = 1;
  let param: T = x;
  x
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());

    let declared = lowered
        .program
        .stmts()
        .iter()
        .filter_map(|stmt| match &stmt.kind {
            StmtKind::Let { binding, .. } => lowered.program.declared_var_type(*binding).cloned(),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(declared.len(), 2);
    assert!(declared.contains(&CoreTypeRef::Primitive(
        cielo_ir::core::PrimitiveTypeRef::Int
    )));
    assert!(declared.contains(&CoreTypeRef::Param(interner.intern("T"))));
}

type ClauseShape = (
    cielo_base::SymbolId,
    Vec<cielo_base::VarId>,
    Option<cielo_base::VarId>,
    cielo_base::StmtId,
);
type HandlerShape = (
    cielo_base::EffectLabelId,
    cielo_base::VarId,
    cielo_base::StmtId,
    Vec<ClauseShape>,
);

/// Handler defs modulo spans, which necessarily differ between two sources.
fn handler_shapes(program: &cielo_ir::core::CoreProgram) -> Vec<HandlerShape> {
    program
        .handlers()
        .iter()
        .map(|handler| {
            (
                handler.effect,
                handler.return_param,
                handler.return_body,
                handler
                    .clauses
                    .iter()
                    .map(|clause| {
                        (
                            clause.operation,
                            clause.params.clone(),
                            clause.resume_param,
                            clause.body,
                        )
                    })
                    .collect(),
            )
        })
        .collect()
}

#[test]
fn named_handler_lowers_identically_to_the_same_clauses_inline() {
    let named = r#"
effect LocalState { fn tick() -> Int }
handler counter with LocalState {
  | tick(resume) => resume(0)
}
fn main() -> Int {
  let x = handle { do LocalState.tick(); 7 } with counter;
  x
}
"#;
    let inline = r#"
effect LocalState { fn tick() -> Int }
fn main() -> Int {
  let x = handle { do LocalState.tick(); 7 } with LocalState {
    | tick(resume) => resume(0)
  };
  x
}
"#;
    // One interner keeps SymbolIds comparable across the two programs.
    let mut interner = Interner::new();
    let named = parse_source(named, SourceId::from_u32(0), &mut interner);
    let inline = parse_source(inline, SourceId::from_u32(1), &mut interner);
    let named = lower_program(&named.program, LowerConfig::default());
    let inline = lower_program(&inline.program, LowerConfig::default());

    assert!(!named.diagnostics.has_errors());
    assert_eq!(
        handler_shapes(&named.program),
        handler_shapes(&inline.program)
    );
}

#[test]
fn a_named_handler_lowers_once_per_handle_site() {
    let src = r#"
effect LocalState { fn tick() -> Int }
handler counter with LocalState {
  | tick(resume) => resume(0)
}
fn a() -> Int {
  let x = handle { do LocalState.tick(); 1 } with counter;
  x
}
fn b() -> Int {
  let x = handle { do LocalState.tick(); 2 } with counter;
  x
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    assert!(!lowered.diagnostics.has_errors());
    // Each site gets its own HandlerDef so later passes can specialize them apart.
    assert_eq!(lowered.program.handlers().len(), 2);
}

#[test]
fn reports_an_unknown_handler_name() {
    let src = r#"
effect LocalState { fn tick() -> Int }
fn main() -> Int {
  let x = handle { do LocalState.tick(); 1 } with nope;
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
            .any(|d| d.code == "LOWER_UNKNOWN_HANDLER")
    );
}

#[test]
fn an_unknown_inline_handler_effect_emits_no_handler() {
    let src = r#"
fn main() -> Int {
  let out = handle { 1 } with Bogus {
    | note(n) => 2
  };
  out
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
            .any(|d| d.code == "LOWER_UNKNOWN_HANDLER_EFFECT")
    );
    assert!(
        lowered.program.handlers().is_empty(),
        "a handler over an unresolved effect would trip the pre-staging assertion"
    );
}

#[test]
fn an_unknown_handler_declaration_effect_emits_no_handler() {
    let src = r#"
handler h with Bogus {
  | note(n) => 2
}
fn main() -> Int {
  let out = handle { 1 } with h;
  out
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
            .any(|d| d.code == "LOWER_UNKNOWN_HANDLER_EFFECT")
    );
    assert!(lowered.program.handlers().is_empty());
}

#[test]
fn an_unknown_do_effect_emits_no_perform() {
    let src = r#"
fn main() -> Int {
  let x = do Bogus.note(1);
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
            .any(|d| d.code == "LOWER_UNKNOWN_EFFECT")
    );
    assert!(
        !lowered
            .program
            .stmts()
            .iter()
            .any(|stmt| matches!(stmt.kind, StmtKind::Perform { .. })),
        "a perform over an unresolved effect would trip the pre-staging assertion"
    );
}

#[test]
fn reports_a_duplicate_handler_declaration() {
    let src = r#"
effect LocalState { fn tick() -> Int }
handler counter with LocalState { | tick(resume) => resume(0) }
handler counter with LocalState { | tick(resume) => resume(1) }
fn main() -> Int {
  let x = handle { do LocalState.tick(); 1 } with counter;
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
            .any(|d| d.code == "LOWER_DUP_HANDLER_DECL")
    );
}

#[test]
fn lowers_struct_constructor_call() {
    let src = r#"
struct Point { x: Int, y: Int }
fn main() -> Int {
  let p = Point(1, 2);
  0
}
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    assert_eq!(lowered.program.structs().len(), 1);

    let has_ctor = lowered
        .program
        .exprs()
        .iter()
        .any(|expr| matches!(expr.kind, cielo_ir::core::ExprKind::MakeStruct { .. }));
    assert!(has_ctor);
}

#[test]
fn lowers_type_arguments_into_core() {
    let src = r#"
enum Option[T] { Some(T), None }
fn unwrap_or[T](opt: Option[T], fallback: T) -> T { fallback }
"#;
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    assert!(!lowered.diagnostics.has_errors());

    let option = interner.intern("Option");
    let param = interner.intern("T");
    let decl = lowered.program.enums().first().expect("enum decl");
    assert_eq!(decl.type_params, vec![param]);
    assert_eq!(decl.variants[0].fields, vec![CoreTypeRef::Param(param)]);

    let function = lowered.program.functions().first().expect("function");
    assert_eq!(
        function.param_types[0],
        CoreTypeRef::Applied {
            name: option,
            args: vec![CoreTypeRef::Param(param)],
        }
    );
    assert_eq!(function.param_types[1], CoreTypeRef::Param(param));
    assert_eq!(function.return_type, CoreTypeRef::Param(param));
}

#[test]
fn lowers_undeclared_type_name_as_named_not_param() {
    let src = "fn f(x: Itn) -> Int { 0 }";
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    let function = lowered.program.functions().first().expect("function");
    assert_eq!(
        function.param_types[0],
        CoreTypeRef::Named(interner.intern("Itn"))
    );
}

fn lower(src: &str) -> cielo_lowering::LowerOutput {
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    lower_program(&parsed.program, LowerConfig::default())
}

/// The literal first argument of every `Call` the list runs, in run order: a
/// `Val`'s body runs before the statement that follows it.
///
/// Branch arms are deliberately not entered, so an empty result over a list
/// containing a branch means nothing was hoisted out of that branch's arms.
fn unconditional_call_args(
    lowered: &cielo_lowering::LowerOutput,
    root: cielo_base::StmtId,
    out: &mut Vec<i64>,
) {
    let Some(stmt) = lowered.program.stmt(root) else {
        return;
    };
    match &stmt.kind {
        StmtKind::Call { args, next, .. } => {
            if let Some(&arg) = args.first()
                && let Some(ExprKind::Literal(Literal::Int(value))) =
                    lowered.program.expr(arg).map(|node| &node.kind)
            {
                out.push(*value);
            }
            unconditional_call_args(lowered, *next, out);
        }
        StmtKind::Val { value, next, .. } => {
            unconditional_call_args(lowered, *value, out);
            unconditional_call_args(lowered, *next, out);
        }
        StmtKind::Let { next, .. }
        | StmtKind::Perform { next, .. }
        | StmtKind::Resume { next, .. } => unconditional_call_args(lowered, *next, out),
        StmtKind::Handle { body, next, .. } | StmtKind::Stage { body, next, .. } => {
            unconditional_call_args(lowered, *body, out);
            if let Some(next) = next {
                unconditional_call_args(lowered, *next, out);
            }
        }
        _ => {}
    }
}

fn lower_with_names(src: &str) -> (cielo_lowering::LowerOutput, Interner) {
    let mut interner = Interner::new();
    let parsed = parse_source(src, SourceId::from_u32(0), &mut interner);
    let lowered = lower_program(&parsed.program, LowerConfig::default());
    (lowered, interner)
}

fn call_args_in_run_order(lowered: &cielo_lowering::LowerOutput, interner: &Interner) -> Vec<i64> {
    let main = lowered
        .program
        .functions()
        .iter()
        .find(|function| interner.resolve(function.name) == Some("main"))
        .expect("main");
    let mut out = Vec::new();
    unconditional_call_args(lowered, main.body, &mut out);
    out
}

fn sole_branch(lowered: &cielo_lowering::LowerOutput) -> (cielo_base::StmtId, cielo_base::StmtId) {
    let mut branches = lowered
        .program
        .stmts()
        .iter()
        .filter_map(|stmt| match stmt.kind {
            StmtKind::If {
                then_branch,
                else_branch,
                ..
            } => Some((then_branch, else_branch)),
            _ => None,
        });
    let branch = branches.next().expect("one branch");
    assert!(branches.next().is_none(), "expected a single branch");
    branch
}

const EFFECTFUL_HELPER: &str = r#"
effect St { fn tick(n: Int) -> Int }

fn note(x: Int) -> Int with St {
  let v = do St.tick(x);
  v
}
"#;

#[test]
fn hoists_statement_shaped_operand_out_of_a_pure_expression() {
    let lowered = lower(
        r#"
fn main() -> Int {
  let a = (if true { 1 } else { 2 }) + 5;
  a
}
"#,
    );
    assert!(!lowered.diagnostics.has_errors());
    let (then_branch, _) = sole_branch(&lowered);
    assert!(matches!(
        lowered.program.stmt(then_branch).map(|node| &node.kind),
        Some(StmtKind::Return(_))
    ));
    // The `+` survives as a pure expression over the hoisted variable.
    assert!(lowered.program.exprs().iter().any(|expr| matches!(
        expr.kind,
        ExprKind::Binary {
            op: BinaryOp::Add,
            ..
        }
    )));
}

/// Operands may perform now, so the order they are hoisted in is observable.
#[test]
fn hoists_operands_left_to_right() {
    let (lowered, interner) = lower_with_names(&format!(
        r#"{EFFECTFUL_HELPER}
fn main() -> Int {{
  handle {{ note(1) + note(2) }} with St {{
    | tick(n, resume) => resume(n)
  }}
}}
"#
    ));
    assert!(!lowered.diagnostics.has_errors());
    assert_eq!(call_args_in_run_order(&lowered, &interner), vec![1, 2]);
}

/// Hoisting an operand out of a branch arm would run the other arm's effects
/// too, so an arm is the boundary a hoist stops at.
#[test]
fn does_not_hoist_operands_across_a_branch() {
    let (lowered, interner) = lower_with_names(&format!(
        r#"{EFFECTFUL_HELPER}
fn main() -> Int {{
  let c = 0;
  handle {{
    if c > 0 {{ note(1) + 300 }} else {{ note(2) + 400 }}
  }} with St {{
    | tick(n, resume) => resume(n)
  }}
}}
"#
    ));
    assert!(!lowered.diagnostics.has_errors());
    assert!(
        call_args_in_run_order(&lowered, &interner).is_empty(),
        "neither arm's call may run before the branch"
    );
    let (then_branch, else_branch) = sole_branch(&lowered);
    let mut then_calls = Vec::new();
    unconditional_call_args(&lowered, then_branch, &mut then_calls);
    let mut else_calls = Vec::new();
    unconditional_call_args(&lowered, else_branch, &mut else_calls);
    assert_eq!(then_calls, vec![1]);
    assert_eq!(else_calls, vec![2]);
}

fn counts_logical_binaries(lowered: &cielo_lowering::LowerOutput) -> usize {
    lowered
        .program
        .exprs()
        .iter()
        .filter(|expr| {
            matches!(
                expr.kind,
                ExprKind::Binary {
                    op: BinaryOp::And | BinaryOp::Or,
                    ..
                }
            )
        })
        .count()
}

/// The branch whose `Return` yields `value` without consulting the right
/// operand: `false` for `&&`, `true` for `||`.
fn has_short_circuit_branch(lowered: &cielo_lowering::LowerOutput, value: bool) -> bool {
    lowered.program.stmts().iter().any(|stmt| {
        let StmtKind::If {
            then_branch,
            else_branch,
            ..
        } = stmt.kind
        else {
            return false;
        };
        let shortcut = if value { then_branch } else { else_branch };
        let Some(StmtKind::Return(expr)) = lowered.program.stmt(shortcut).map(|node| &node.kind)
        else {
            return false;
        };
        matches!(
            lowered.program.expr(*expr).map(|node| &node.kind),
            Some(ExprKind::Literal(Literal::Bool(literal))) if *literal == value
        )
    })
}

#[test]
fn lowers_logical_and_in_binding_position_into_short_circuiting_if() {
    let lowered = lower(
        r#"
fn main() -> Int {
  let d = 0;
  let safe = d != 0 && 10 / d > 1;
  if safe { 1 } else { 2 }
}
"#,
    );
    assert!(!lowered.diagnostics.has_errors());
    assert_eq!(counts_logical_binaries(&lowered), 0);
    assert!(has_short_circuit_branch(&lowered, false));
}

#[test]
fn lowers_logical_or_in_binding_position_into_short_circuiting_if() {
    let lowered = lower(
        r#"
fn main() -> Int {
  let d = 0;
  let safe = d == 0 || 10 / d > 1;
  if safe { 1 } else { 2 }
}
"#,
    );
    assert!(!lowered.diagnostics.has_errors());
    assert_eq!(counts_logical_binaries(&lowered), 0);
    assert!(has_short_circuit_branch(&lowered, true));
}

#[test]
fn lowers_short_circuit_in_tail_and_if_condition_position() {
    let lowered = lower(
        r#"
fn guard(d: Int) -> Bool {
  d != 0 && 10 / d > 1
}

fn main() -> Int {
  if guard(0) || guard(4) { 1 } else { 2 }
}
"#,
    );
    assert!(!lowered.diagnostics.has_errors());
    assert_eq!(counts_logical_binaries(&lowered), 0);
    assert!(has_short_circuit_branch(&lowered, false));
    assert!(has_short_circuit_branch(&lowered, true));
}

#[test]
fn short_circuits_logical_operator_in_pure_operand_position() {
    let lowered = lower(
        r#"
fn pick(b: Bool) -> Int {
  if b { 1 } else { 2 }
}

fn main() -> Int {
  let d = 0;
  pick(d != 0 && 10 / d > 1)
}
"#,
    );
    assert!(!lowered.diagnostics.has_errors());
    assert_eq!(counts_logical_binaries(&lowered), 0);
    assert!(has_short_circuit_branch(&lowered, false));
}
