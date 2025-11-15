use cielo::common::ids::{FuncId, SourceId, VarId};
use cielo::common::symbols::Interner;
use cielo::ir::core::Literal;
use cielo::ir::linear::{LinearExpr, LinearFunction, LinearMatchArm, LinearProgram, LinearStmt};
use cielo::passes::c_emit::emit_c_program;
use cielo::{Compiler, CompilerConfig};

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
