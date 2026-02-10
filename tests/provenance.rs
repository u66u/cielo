use std::collections::HashSet;

use cielo::common::ids::{ExprId, SourceId, StmtId};
use cielo::common::symbols::Interner;
use cielo::ir::core::{CoreProgram, StmtKind};
use cielo::pipeline::phases::Reason;
use cielo::pipeline::provenance::{runtime_provenance_lines, staging_root_causes};
use cielo::{Compiler, CompilerConfig};

#[test]
fn runtime_provenance_tracks_forced_runtime_dependency_chain() {
    let src = r#"
fn main() -> Int {
  let y = @runtime { 1 + 2 };
  let z = y + 1;
  z
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);

    let main = residual
        .program()
        .functions()
        .first()
        .expect("main function");
    let ret_expr = first_return_expr(residual.program(), main.body).expect("main return expr");
    let lines = runtime_provenance_lines(residual.program(), residual.bta(), ret_expr, 6);

    assert!(
        lines
            .iter()
            .any(|line| line.contains("explicitly marked @runtime")),
        "expected forced runtime root cause in provenance chain: {lines:?}"
    );
}

#[test]
fn runtime_provenance_reports_non_thunkable_effect_blocker() {
    let src = r#"
effect Console { fn print(s: String) -> () }

fn io() -> Int with Console {
  do Console.print("x");
  1
}

fn main() -> Int {
  let x = io();
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);

    let main = residual
        .program()
        .functions()
        .iter()
        .find(|function| interner.resolve(function.name) == Some("main"))
        .expect("main function");
    let ret_expr = first_return_expr(residual.program(), main.body).expect("main return expr");
    let lines = runtime_provenance_lines(residual.program(), residual.bta(), ret_expr, 6);

    assert!(
        lines
            .iter()
            .any(|line| line.contains("not thunkable/discharged")),
        "expected effect blocker root cause in provenance chain: {lines:?}"
    );
}

fn first_return_expr(program: &CoreProgram, root: StmtId) -> Option<ExprId> {
    let mut stack = vec![root];
    let mut seen = HashSet::new();

    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let stmt = program.stmt(stmt_id)?;
        match &stmt.kind {
            StmtKind::Return(expr) => return Some(*expr),
            StmtKind::Let { next, .. }
            | StmtKind::Call { next, .. }
            | StmtKind::Resume { next, .. }
            | StmtKind::Perform { next, .. } => stack.push(*next),
            StmtKind::Val { value, next, .. } => {
                stack.push(*next);
                stack.push(*value);
            }
            StmtKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                stack.push(*else_branch);
                stack.push(*then_branch);
            }
            StmtKind::Match { arms, default, .. } => {
                if let Some(default_stmt) = default {
                    stack.push(*default_stmt);
                }
                for arm in arms {
                    stack.push(arm.body);
                }
            }
            StmtKind::Handle { body, next, .. } | StmtKind::Stage { body, next, .. } => {
                if let Some(next_stmt) = next {
                    stack.push(*next_stmt);
                }
                stack.push(*body);
            }
            StmtKind::Hole { .. } | StmtKind::Error(_) => {}
        }
    }

    None
}

#[test]
fn test_root_cause_rollup_parameter_taint() {
    let compiler = Compiler::new(CompilerConfig::default());
    let mut interner = Interner::new();

    // `input` is RT. It taints `a`, `b`, and all the binary operations.
    let source = r#"
    fn main(input: Int) -> Int {
        let a = input + 1;
        let b = a * 2;
        b
    }
    "#;

    // Stop at the BTA phase so we can inspect the analytical tables
    let core = compiler.parse_and_lower_to_core(source, SourceId::new(0), &mut interner);
    let bta = compiler.run_v1_evaluate_classify(core);

    let rollups = staging_root_causes(bta.program(), bta.bta());

    let root = rollups
        .iter()
        .find(|root| matches!(root.terminal_reason, Reason::Parameter { .. }))
        .expect("Expected at least one parameter root cause");
    assert!(
        matches!(root.terminal_reason, Reason::Parameter { .. }),
        "Root cause should be a parameter, got {:?}",
        root.terminal_reason
    );

    // It should taint at least more than the parameter expression itself.
    assert!(
        root.taint_count >= 2,
        "Root cause should taint multiple dependent expressions, got {}",
        root.taint_count
    );
}
