use cielo::common::ids::SourceId;
use cielo::common::symbols::Interner;
use cielo::frontend::parser::parse_source;
use cielo::ir::core::StmtKind;
use cielo::passes::lowering::{LowerConfig, lower_program};

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
    program: &cielo::ir::core::CoreProgram,
    root: cielo::common::ids::StmtId,
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
        .any(|expr| matches!(expr.kind, cielo::ir::core::ExprKind::MakeEnum { .. }));
    assert!(has_ctor);
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
        .any(|expr| matches!(expr.kind, cielo::ir::core::ExprKind::MakeStruct { .. }));
    assert!(has_ctor);
}
