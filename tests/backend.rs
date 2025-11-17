use cielo::common::ids::{FuncId, SourceId, VarId};
use cielo::common::symbols::Interner;
use cielo::ir::core::Literal;
use cielo::ir::linear::{
    CallConvention, LinearExpr, LinearFunction, LinearMatchArm, LinearProgram, LinearStmt,
};
use cielo::passes::c_emit::emit_c_program;
use cielo::{Compiler, CompilerConfig};
use std::collections::HashSet;

#[test]
fn emits_c_for_basic_arithmetic_program() {
    let src = r#"
fn main() -> Int {
  let x = 1 + 2;
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    assert_eq!(compiled.linear.functions.len(), 1);
    assert!(compiled.c_source.contains("cv_add("));
    assert!(compiled.c_source.contains("int main(void)"));
    assert!(compiled.residual.diagnostics().entries().is_empty());
}

#[test]
fn emits_runtime_stub_calls_for_effect_operations() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  do Console.print("hello");
  0
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    assert!(compiled.c_source.contains("cielo_perform("));
    assert!(compiled.c_source.contains("print"));
}

#[test]
fn emits_handler_scope_push_pop_for_handle() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  let x = handle { do Console.print("hello"); 1 } with Console {
    | print(s) => 0
  };
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    assert!(compiled.c_source.contains("cielo_handler_push("));
    assert!(compiled.c_source.contains("cielo_handler_pop("));
}

#[test]
fn emits_match_branches_with_ctor_runtime_helpers() {
    let mut interner = Interner::new();
    let main_name = interner.intern("main");
    let option_name = interner.intern("Option");
    let some_name = interner.intern("Some");

    let mut program = LinearProgram::default();
    let seven = program.push_expr(LinearExpr::Literal(Literal::Int(7)));
    let scrutinee = program.push_expr(LinearExpr::MakeEnum {
        ty: option_name,
        variant: some_name,
        fields: vec![seven],
    });

    let binder = VarId::from_u32(0);
    let binder_expr = program.push_expr(LinearExpr::Var(binder));
    let arm_body = program.push_stmt(LinearStmt::Return(binder_expr));
    let default_expr = program.push_expr(LinearExpr::Literal(Literal::Int(0)));
    let default_body = program.push_stmt(LinearStmt::Return(default_expr));
    let body = program.push_stmt(LinearStmt::Match {
        scrutinee,
        arms: vec![LinearMatchArm {
            tag: some_name,
            binders: vec![binder],
            body: arm_body,
        }],
        default: Some(default_body),
    });

    program.functions.push(LinearFunction {
        id: FuncId::new(0),
        name: main_name,
        params: vec![],
        body,
    });
    program.entrypoints = vec![FuncId::new(0)];

    let emitted = emit_c_program(&program, &interner);
    assert!(emitted.contains("cielo_ctor_is_variant("));
    assert!(emitted.contains("cielo_ctor_field("));
    assert!(emitted.contains("cielo_make_ctor("));
}

#[test]
fn linearize_classifies_effectful_calls_by_convention() {
    let src = r#"
effect LocalState { fn tick() -> Int }
effect Console { fn print(s: String) -> () }

fn local() -> Int with LocalState {
  do LocalState.tick();
  1
}

fn io() -> Int with Console {
  do Console.print("x");
  2
}

fn main() -> Int {
  let a = local();
  let b = io();
  b
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    let main = compiled
        .linear
        .functions
        .iter()
        .find(|function| interner.resolve(function.name) == Some("main"))
        .expect("main function");

    let mut seen = HashSet::new();
    let mut conventions = Vec::new();
    collect_call_conventions(&compiled.linear, main.body, &mut seen, &mut conventions);

    assert!(conventions.contains(&CallConvention::Direct));
    assert!(conventions.contains(&CallConvention::Control));
}

#[test]
fn c_emitter_threads_call_convention_wrappers() {
    let src = r#"
effect LocalState { fn tick() -> Int }
effect Console { fn print(s: String) -> () }

fn pure(x: Int) -> Int {
  x + 1
}

fn local() -> Int with LocalState {
  do LocalState.tick();
  1
}

fn io() -> Int with Console {
  do Console.print("x");
  2
}

fn main() -> Int {
  let p = pure(1);
  let a = local();
  let b = io();
  b
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    assert!(compiled.c_source.contains("CIELO_CALL_PURE("));
    assert!(compiled.c_source.contains("CIELO_CALL_DIRECT("));
    assert!(compiled.c_source.contains("CIELO_CALL_CONTROL("));
}

fn collect_call_conventions(
    program: &LinearProgram,
    stmt_id: cielo::common::ids::LinearStmtId,
    seen: &mut HashSet<cielo::common::ids::LinearStmtId>,
    out: &mut Vec<CallConvention>,
) {
    if !seen.insert(stmt_id) {
        return;
    }
    let Some(stmt) = program.stmt(stmt_id) else {
        return;
    };
    match &stmt.kind {
        LinearStmt::Call {
            convention, next, ..
        } => {
            out.push(*convention);
            collect_call_conventions(program, *next, seen, out);
        }
        LinearStmt::Let { next, .. } | LinearStmt::Perform { next, .. } => {
            collect_call_conventions(program, *next, seen, out);
        }
        LinearStmt::Val { value, next, .. } => {
            collect_call_conventions(program, *value, seen, out);
            collect_call_conventions(program, *next, seen, out);
        }
        LinearStmt::If {
            then_branch,
            else_branch,
            ..
        } => {
            collect_call_conventions(program, *then_branch, seen, out);
            collect_call_conventions(program, *else_branch, seen, out);
        }
        LinearStmt::Match { arms, default, .. } => {
            for arm in arms {
                collect_call_conventions(program, arm.body, seen, out);
            }
            if let Some(default_stmt) = default {
                collect_call_conventions(program, *default_stmt, seen, out);
            }
        }
        LinearStmt::Handle { body, next, .. } | LinearStmt::Stage { body, next, .. } => {
            collect_call_conventions(program, *body, seen, out);
            if let Some(next_stmt) = next {
                collect_call_conventions(program, *next_stmt, seen, out);
            }
        }
        LinearStmt::Return(_) | LinearStmt::Hole | LinearStmt::Error => {}
    }
}
