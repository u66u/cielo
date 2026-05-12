//! Structured emission: what the backend recovers, and what it gives up on.
//!
//! The give-up case has no producer -- Core has no loop form, so nothing
//! lowers to a CFG with a back edge -- and a hand-built cycle is the only way
//! to reach the labels-and-gotos fallback at all.

use cielo_base::{CfgFuncId, Interner, SourceId};
use cielo_ir::cfg::{CfgExpr, CfgFunction, CfgProgram, CfgTerminator};
use cielo_ir::constants::ConstantTable;
use cielo_ir::core::{BinaryOp, Literal};
use cielo_test_support::{PassConfig, PassHarness};
use std::ffi::OsString;
use std::process::{Command, Stdio};

fn bodies(emitted: &str) -> &str {
    let start = emitted
        .find("CieloValue cielo_fn_")
        .expect("emitted C should declare at least one function");
    &emitted[start..]
}

fn compile_to_c(source: &str) -> String {
    let mut interner = Interner::new();
    let compiler = PassHarness::new(PassConfig::default());
    let compiled = compiler.compile_source_to_c(source, SourceId::from_u32(0), &mut interner);
    assert!(
        !compiled.residual.diagnostics().has_errors(),
        "source did not reach C emission: {:?}",
        compiled
            .residual
            .diagnostics()
            .entries()
            .iter()
            .map(|entry| entry.code.to_owned())
            .collect::<Vec<_>>()
    );
    compiled.c_source
}

/// The arms carry an ADT rather than two literals, or the whole function folds
/// away at compile time and there is no branch left to structure.
#[test]
fn a_runtime_if_becomes_an_if_else_with_no_goto() {
    let emitted = compile_to_c(
        r#"
enum Box { B(Int) }
fn pick(flag: Bool, x: Box, y: Box) -> Int {
  if flag {
    match x { B(a) => a }
  } else {
    match y { B(b) => b }
  }
}
fn main() -> Int {
  pick(true, B(3), B(4))
}
"#,
    );
    let bodies = bodies(emitted.as_str());
    assert!(
        bodies.contains("if (cv_truthy(") && bodies.contains("} else {"),
        "a branch should emit an if/else:\n{bodies}"
    );
    assert!(
        !bodies.contains("goto b"),
        "an acyclic CFG needs no goto at all:\n{bodies}"
    );
    assert!(
        !bodies.contains(": ;"),
        "and therefore no labels either:\n{bodies}"
    );
    assert!(
        bodies.contains("const CieloValue v"),
        "a parameter nothing writes back into is const:\n{bodies}"
    );
}

/// The arms of a match join on one continuation. That join has several
/// predecessors, so it is laid out once at its immediate dominator rather than
/// duplicated into every arm.
#[test]
fn match_arms_join_on_a_single_continuation() {
    let emitted = compile_to_c(
        r#"
enum Shape { Circle(Int), Square(Int) }
fn area(s: Shape) -> Int {
  let n = match s { Circle(r) => r, Square(w) => w };
  n + 1
}
fn main() -> Int {
  area(Circle(7))
}
"#,
    );
    let bodies = bodies(emitted.as_str());
    assert_eq!(
        bodies.matches("cv_add(").count(),
        1,
        "the continuation after the match belongs to one site:\n{bodies}"
    );
    assert!(
        !bodies.contains("goto b"),
        "an acyclic CFG needs no goto at all:\n{bodies}"
    );
}

/// Counts down from five and returns `n + 7`, so a fallback that dropped the
/// back edge would return 12 instead of 7.
fn countdown(interner: &mut Interner) -> CfgProgram {
    let mut cfg = CfgProgram::default();
    let counter = cfg.push_value(None);

    let entry = cfg.push_block(Vec::new(), None);
    let header = cfg.push_block(vec![counter], None);
    let body = cfg.push_block(Vec::new(), None);
    let exit = cfg.push_block(Vec::new(), None);

    let five = cfg.push_expr(CfgExpr::Literal(Literal::Int(5)), None);
    cfg.set_terminator(
        entry,
        CfgTerminator::Goto {
            target: header,
            args: vec![five],
        },
    );

    let read = cfg.push_expr(CfgExpr::Value(counter), None);
    let zero = cfg.push_expr(CfgExpr::Literal(Literal::Int(0)), None);
    let test = cfg.push_expr(
        CfgExpr::Binary {
            op: BinaryOp::Gt,
            lhs: read,
            rhs: zero,
        },
        None,
    );
    cfg.set_terminator(
        header,
        CfgTerminator::Branch {
            cond: test,
            then_target: body,
            else_target: exit,
        },
    );

    let one = cfg.push_expr(CfgExpr::Literal(Literal::Int(1)), None);
    let decrement = cfg.push_expr(
        CfgExpr::Binary {
            op: BinaryOp::Sub,
            lhs: read,
            rhs: one,
        },
        None,
    );
    cfg.set_terminator(
        body,
        CfgTerminator::Goto {
            target: header,
            args: vec![decrement],
        },
    );

    let seven = cfg.push_expr(CfgExpr::Literal(Literal::Int(7)), None);
    let total = cfg.push_expr(
        CfgExpr::Binary {
            op: BinaryOp::Add,
            lhs: read,
            rhs: seven,
        },
        None,
    );
    cfg.set_terminator(exit, CfgTerminator::Return(total));

    let id = CfgFuncId::new(0);
    cfg.functions.push(CfgFunction {
        id,
        name: interner.intern("main"),
        params: Vec::new(),
        entry,
    });
    cfg.entrypoints.push(id);
    cfg
}

#[test]
fn a_back_edge_falls_back_to_a_label_and_a_goto() {
    let mut interner = Interner::new();
    let cfg = countdown(&mut interner);
    assert_eq!(cfg.validate(), Ok(()));

    let emitted = cielo_backend_c::emit(&cfg, &interner, &ConstantTable::default(), false);
    let bodies = bodies(emitted.as_str());
    assert!(
        bodies.contains("goto b"),
        "a loop header is not structured, so the back edge stays a goto:\n{bodies}"
    );
    assert_eq!(
        bodies.matches(": ;").count(),
        1,
        "only the loop header earns a label:\n{bodies}"
    );

    let Some(code) = compile_and_run(emitted.as_str(), "countdown") else {
        return;
    };
    assert_eq!(code, Some(7), "the fallback must still run the loop");
}

fn c_compiler_command() -> OsString {
    std::env::var_os("CC").unwrap_or_else(|| OsString::from("cc"))
}

/// `None` when no C compiler is installed, so callers skip rather than fail.
fn compile_and_run(c_source: &str, name: &str) -> Option<Option<i32>> {
    if Command::new(c_compiler_command())
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        eprintln!("skipping {name} execution check: no C compiler found");
        return None;
    }

    let dir = std::env::temp_dir().join(format!("cielo_structure_{name}_{}", std::process::id()));
    std::fs::create_dir_all(dir.as_path()).expect("temp dir");
    let source = dir.join(format!("{name}.c"));
    let binary = dir.join(name);
    std::fs::write(source.as_path(), c_source).expect("write emitted C");

    let build = Command::new(c_compiler_command())
        .arg("-std=c11")
        .arg("-O2")
        .arg("-Wall")
        .arg("-Werror=implicit-function-declaration")
        .arg("-o")
        .arg(binary.as_path())
        .arg(source.as_path())
        .output()
        .expect("invoke C compiler");
    assert!(
        build.status.success(),
        "emitted C for {name} does not compile:\n{}",
        String::from_utf8_lossy(build.stderr.as_slice())
    );

    let run = Command::new(binary.as_path()).output().expect("run binary");
    let _ = std::fs::remove_dir_all(dir.as_path());
    Some(run.status.code())
}
