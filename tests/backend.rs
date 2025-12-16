use cielo::common::ids::{LinearFuncId, SourceId, VarId};
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

    assert!(
        compiled
            .c_source
            .contains("(void)cielo_perform(0, \"print\"")
    );
    assert!(compiled.c_source.contains("print"));
}

#[test]
fn source_if_with_ct_condition_is_pruned_before_linear_ir() {
    let src = r#"
fn main() -> Int {
  let x = if true { 1 } else { 2 };
  x
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

    assert!(
        !linear_stmt_graph_contains_if(&compiled.linear, main.body),
        "ct-known source if should be pruned before linearization"
    );
}

#[test]
fn source_if_with_alias_ct_condition_is_pruned_before_linear_ir() {
    let src = r#"
fn main() -> Int {
  let c0 = true;
  let c1 = c0;
  let x = if c1 { 1 } else { 2 };
  x
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

    assert!(
        !linear_stmt_graph_contains_if(&compiled.linear, main.body),
        "alias-to-literal ct condition should be pruned before linearization"
    );
    assert!(
        linear_stmt_graph_contains_literal_int(&compiled.linear, main.body, 1),
        "selected branch payload should remain"
    );
}

#[test]
fn source_match_with_known_variant_is_pruned_before_linear_ir() {
    let src = r#"
enum Flag { On, Off }
fn main() -> Int {
  let x = match On() {
    | On => 1
    | _ => 0
  };
  x
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

    assert!(
        !linear_stmt_graph_contains_match(&compiled.linear, main.body),
        "known-variant source match should be pruned before linearization"
    );
}

#[test]
fn source_match_with_binder_is_pruned_and_binder_flow_is_preserved() {
    let src = r#"
enum OptionI { Some(Int), None }
fn main() -> Int {
  let x = match Some(41) {
    | Some(v) => v + 1
    | _ => 0
  };
  x
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

    assert!(
        !linear_stmt_graph_contains_match(&compiled.linear, main.body),
        "known-variant source match with binders should be pruned before linearization"
    );
    assert!(
        linear_stmt_graph_contains_literal_int(&compiled.linear, main.body, 41),
        "selected match arm should preserve payload literal flow"
    );
    assert!(
        linear_stmt_graph_contains_add_rhs_int(&compiled.linear, main.body, 1),
        "selected match arm body should remain after pruning"
    );
}

#[test]
fn source_match_with_alias_scrutinee_and_binder_is_pruned() {
    let src = r#"
enum OptionI { Some(Int), None }
fn main() -> Int {
  let s0 = Some(41);
  let s1 = s0;
  let x = match s1 {
    | Some(v) => v + 1
    | _ => 0
  };
  x
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

    assert!(
        !linear_stmt_graph_contains_match(&compiled.linear, main.body),
        "alias-to-constructor scrutinee should still prune match before linearization"
    );
    assert!(
        linear_stmt_graph_contains_literal_int(&compiled.linear, main.body, 41),
        "selected arm should preserve payload flow"
    );
    assert!(
        linear_stmt_graph_contains_add_rhs_int(&compiled.linear, main.body, 1),
        "selected arm computation should remain after pruning"
    );
}

#[test]
fn c_emitter_dedups_string_literals_in_const_pool() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  do Console.print("same");
  do Console.print("same");
  0
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    assert_eq!(
        compiled
            .c_source
            .matches("static const char* cielo_const_s_")
            .count(),
        1,
        "duplicated string literals should be pooled exactly once"
    );
    assert!(
        compiled.c_source.contains("cv_string(cielo_const_s_0)"),
        "pooled string should be referenced through const symbol"
    );
}

#[test]
fn c_emitter_respects_const_pool_entry_size_cap() {
    let long = "a".repeat(1100);
    let src = format!(
        r#"
effect Console {{ fn print(s: String) -> () }}
fn main() -> Int {{
  do Console.print("{long}");
  0
}}
"#
    );
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled =
        compiler.compile_source_v0_to_c(src.as_str(), SourceId::from_u32(0), &mut interner);

    assert!(
        !compiled
            .c_source
            .contains("static const char* cielo_const_s_"),
        "oversized literals should not be added to const pool"
    );
    assert!(
        compiled.c_source.contains("cv_string(\""),
        "oversized literals should still be emitted inline"
    );
}

#[test]
fn lowers_handled_perform_into_clause_without_runtime_dispatch() {
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

    assert!(
        !compiled.c_source.contains("cielo_handler_push(0);"),
        "handled callsites should not emit runtime handler push/pop"
    );
    assert!(
        !compiled
            .c_source
            .contains("(void)cielo_perform(0, \"print\""),
        "handled operations should lower into clause bodies instead of runtime perform stubs"
    );
    assert!(
        compiled.c_source.contains("cv_int(0)"),
        "handler clause return should be reflected in emitted C body"
    );
}

#[test]
fn lowers_resumptive_clause_into_continuation_flow() {
    let src = r#"
effect LocalState { fn tick() -> Int }
fn main() -> Int {
  let x = handle { do LocalState.tick(); 9 } with LocalState {
    | tick(resume) => resume(41)
  };
  x
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

    assert!(
        !compiled
            .c_source
            .contains("(void)cielo_perform(0, \"tick\""),
        "handled resumptive operations should not call runtime perform stubs"
    );
    assert!(
        compiled.c_source.contains("cv_int(9)"),
        "resumptive clause should continue into the operation continuation"
    );
    assert!(
        linear_stmt_graph_tail_resume_wrapper_count(&compiled.linear, main.body) == 1,
        "tail resumptions should leave exactly one identity wrapper shape after TR optimization"
    );
}

#[test]
fn lowers_non_tail_resume_with_control_flow_after_resumption() {
    let src = r#"
effect LocalState { fn tick() -> Int }
fn main() -> Int {
  let x = handle { do LocalState.tick(); 9 } with LocalState {
    | tick(resume) => {
      let y = resume(41);
      y + 1
    }
  };
  x
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

    assert!(
        !linear_stmt_graph_contains_perform_effect(&compiled.linear, main.body, 0),
        "handled resumptions should not leave residual perform dispatch for handled effect"
    );
    assert!(
        linear_stmt_graph_contains_literal_int(&compiled.linear, main.body, 9),
        "continuation value should still flow from resumed branch"
    );
    assert!(
        linear_stmt_graph_contains_add_rhs_int(&compiled.linear, main.body, 1),
        "non-tail code after resume should be preserved"
    );
    assert!(
        linear_stmt_graph_tail_resume_wrapper_count(&compiled.linear, main.body) == 0,
        "non-tail resumptions should not collapse into the tail identity-wrapper shape"
    );
}

#[test]
fn reports_multi_shot_resume_in_handler_clause() {
    let src = r#"
effect LocalState { fn tick() -> Int }
fn main() -> Int {
  let x = handle { do LocalState.tick(); 9 } with LocalState {
    | tick(resume) => {
      let a = resume(41);
      resume(a)
    }
  };
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    assert!(
        compiled
            .residual
            .diagnostics()
            .entries()
            .iter()
            .any(|diag| diag.code == "LINEARIZE_MULTI_SHOT_RESUME"),
        "multi-shot resume usage should produce a dedicated linearization diagnostic"
    );
}

#[test]
fn allows_branch_exclusive_single_shot_resume_paths() {
    let src = r#"
effect LocalState { fn tick(flag: Bool) -> Int }
fn main() -> Int {
  let x = handle { do LocalState.tick(true); 9 } with LocalState {
    | tick(flag, resume) => {
      if flag {
        resume(41)
      } else {
        resume(42)
      }
    }
  };
  x
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

    assert!(
        !compiled
            .residual
            .diagnostics()
            .entries()
            .iter()
            .any(|diag| diag.code == "LINEARIZE_MULTI_SHOT_RESUME"),
        "distinct branches that each resume once should be accepted as single-shot"
    );
    assert!(
        !compiled
            .residual
            .diagnostics()
            .entries()
            .iter()
            .any(|diag| diag.code == "LINEARIZE_DIRECT_RESUME_NON_TAIL"),
        "branch-exclusive tail resumptions should stay on direct tail paths"
    );
    assert!(
        !linear_stmt_graph_contains_perform_effect(&compiled.linear, main.body, 0),
        "handled branch-exclusive resumptions should not leave residual perform dispatch"
    );
    assert!(
        linear_stmt_graph_tail_resume_wrapper_count(&compiled.linear, main.body) >= 1,
        "single-shot branch resumptions should still lower through tail-resumption wrappers"
    );
}

#[test]
fn reports_multi_shot_when_one_branch_resumes_twice_on_the_same_path() {
    let src = r#"
effect LocalState { fn tick(flag: Bool) -> Int }
fn main() -> Int {
  let x = handle { do LocalState.tick(false); 9 } with LocalState {
    | tick(flag, resume) => {
      if flag {
        resume(41)
      } else {
        let y = resume(42);
        resume(y)
      }
    }
  };
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    assert!(
        compiled
            .residual
            .diagnostics()
            .entries()
            .iter()
            .any(|diag| diag.code == "LINEARIZE_MULTI_SHOT_RESUME"),
        "a control-flow path with two resumes must still be rejected as multi-shot"
    );
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
        id: LinearFuncId::new(0),
        name: main_name,
        params: vec![],
        body,
    });
    program.entrypoints = vec![LinearFuncId::new(0)];

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

#[test]
fn linearize_prunes_unreachable_recursive_function_cycles() {
    let src = r#"
fn live() -> Int {
  7
}

fn dead_a() -> Int {
  dead_b()
}

fn dead_b() -> Int {
  dead_a()
}

fn main() -> Int {
  live()
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    let mut names = linear_function_names(&compiled.linear, &interner);
    names.sort_unstable();
    assert_eq!(names, vec!["live".to_owned(), "main".to_owned()]);
    assert!(
        !compiled.c_source.contains("cielo_fn_dead_a_"),
        "unreachable dead_a must not be emitted"
    );
    assert!(
        !compiled.c_source.contains("cielo_fn_dead_b_"),
        "unreachable dead_b must not be emitted"
    );
}

#[test]
fn linearize_keeps_pure_call_dependencies_reachable() {
    let src = r#"
fn helper(x: Int) -> Int {
  x + 1
}

fn wrapper() -> Int {
  helper(41)
}

fn dead() -> Int {
  0
}

fn main() -> Int {
  wrapper()
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    let mut names = linear_function_names(&compiled.linear, &interner);
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["helper".to_owned(), "main".to_owned(), "wrapper".to_owned()]
    );
    assert!(
        !compiled.c_source.contains("cielo_fn_dead_"),
        "unreachable pure function must not be emitted"
    );
}

#[test]
fn linearize_drops_unspecialized_copy_after_handler_specialization() {
    let src = r#"
effect Console { fn print(s: String) -> () }

fn loop() -> Int with Console {
  loop()
}

fn main() -> Int {
  let x = handle { loop() } with Console {
    | print(s) => 0
  };
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    let loop_count = compiled
        .linear
        .functions
        .iter()
        .filter(|function| interner.resolve(function.name) == Some("loop"))
        .count();
    assert_eq!(
        compiled.linear.functions.len(),
        2,
        "only main + specialized loop should remain reachable"
    );
    assert_eq!(
        loop_count, 1,
        "unspecialized loop copy should be pruned after callsite retargeting"
    );
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
        LinearStmt::PureCall { next, .. } => {
            out.push(CallConvention::Pure);
            collect_call_conventions(program, *next, seen, out);
        }
        LinearStmt::DirectCall { next, .. } => {
            out.push(CallConvention::Direct);
            collect_call_conventions(program, *next, seen, out);
        }
        LinearStmt::ControlCall { next, .. } => {
            out.push(CallConvention::Control);
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

fn linear_stmt_graph_contains_perform_effect(
    program: &LinearProgram,
    root: cielo::common::ids::LinearStmtId,
    effect: u32,
) -> bool {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if matches!(stmt.kind, LinearStmt::Perform { effect: eff, .. } if eff.as_u32() == effect) {
            return true;
        }
        for child in stmt.child_stmts() {
            stack.push(child);
        }
    }
    false
}

fn linear_stmt_graph_contains_if(
    program: &LinearProgram,
    root: cielo::common::ids::LinearStmtId,
) -> bool {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if matches!(stmt.kind, LinearStmt::If { .. }) {
            return true;
        }
        for child in stmt.child_stmts() {
            stack.push(child);
        }
    }
    false
}

fn linear_stmt_graph_contains_match(
    program: &LinearProgram,
    root: cielo::common::ids::LinearStmtId,
) -> bool {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if matches!(stmt.kind, LinearStmt::Match { .. }) {
            return true;
        }
        for child in stmt.child_stmts() {
            stack.push(child);
        }
    }
    false
}

fn linear_stmt_graph_contains_literal_int(
    program: &LinearProgram,
    root: cielo::common::ids::LinearStmtId,
    value: i64,
) -> bool {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        for expr_id in stmt.child_exprs() {
            if linear_expr_is_int_literal(program, expr_id, value) {
                return true;
            }
        }
        for child in stmt.child_stmts() {
            stack.push(child);
        }
    }
    false
}

fn linear_stmt_graph_contains_add_rhs_int(
    program: &LinearProgram,
    root: cielo::common::ids::LinearStmtId,
    rhs: i64,
) -> bool {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };

        let exprs = match &stmt.kind {
            LinearStmt::Return(expr) => vec![*expr],
            LinearStmt::Let { value, .. } => vec![*value],
            _ => Vec::new(),
        };
        for expr_id in exprs {
            if let Some(expr) = program.expr(expr_id)
                && let LinearExpr::Binary {
                    op, rhs: rhs_expr, ..
                } = expr.kind
                && op == cielo::ir::core::BinaryOp::Add
                && linear_expr_is_int_literal(program, rhs_expr, rhs)
            {
                return true;
            }
        }

        for child in stmt.child_stmts() {
            stack.push(child);
        }
    }
    false
}

fn linear_stmt_graph_tail_resume_wrapper_count(
    program: &LinearProgram,
    root: cielo::common::ids::LinearStmtId,
) -> usize {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    let mut count = 0usize;
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if let LinearStmt::Val { binding, next, .. } = stmt.kind
            && let Some(next_stmt) = program.stmt(next)
            && let LinearStmt::Let {
                binding: return_param,
                value,
                next: return_next,
            } = next_stmt.kind
            && linear_expr_is_var(program, value, binding)
            && let Some(return_stmt) = program.stmt(return_next)
            && let LinearStmt::Return(return_expr) = return_stmt.kind
            && linear_expr_is_var(program, return_expr, return_param)
        {
            count += 1;
        }
        for child in stmt.child_stmts() {
            stack.push(child);
        }
    }
    count
}

fn linear_expr_is_var(
    program: &LinearProgram,
    expr_id: cielo::common::ids::LinearExprId,
    var: VarId,
) -> bool {
    program
        .expr(expr_id)
        .is_some_and(|expr| matches!(expr.kind, LinearExpr::Var(bound) if bound == var))
}

fn linear_expr_is_int_literal(
    program: &LinearProgram,
    expr_id: cielo::common::ids::LinearExprId,
    value: i64,
) -> bool {
    program.expr(expr_id).is_some_and(
        |expr| matches!(&expr.kind, LinearExpr::Literal(Literal::Int(lit)) if *lit == value),
    )
}

fn linear_function_names(program: &LinearProgram, interner: &Interner) -> Vec<String> {
    program
        .functions
        .iter()
        .map(|function| {
            interner
                .resolve(function.name)
                .unwrap_or("<missing>")
                .to_owned()
        })
        .collect()
}
