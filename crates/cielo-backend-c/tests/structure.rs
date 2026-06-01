//! Structured emission: what the backend recovers, and what it gives up on.
//!
//! The give-up case has no producer -- Core has no loop form, so nothing
//! lowers to a CFG with a back edge -- and a hand-built cycle is the only way
//! to reach the labels-and-gotos fallback at all.

use cielo_base::{CfgFuncId, Interner, SourceId};
use cielo_ir::cfg::{CfgExpr, CfgFunction, CfgProgram, CfgTerminator};
use cielo_ir::constants::ConstantTable;
use cielo_ir::core::{BinaryOp, Literal};
use cielo_memory::MemoryPreset;
use cielo_test_support::{PassConfig, PassHarness};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn bodies(emitted: &str) -> &str {
    emitted
        .split_once(cielo_backend_c::EMITTED_BODIES_MARKER)
        .expect("emitted C should carry the body marker")
        .1
}

fn compile_to_c(source: &str) -> String {
    compile_to_c_with_preset(source, MemoryPreset::ArcOptimized)
}

fn compile_to_c_with_preset(source: &str, preset: MemoryPreset) -> String {
    let mut interner = Interner::new();
    let compiler = PassHarness::new(PassConfig::default().with_memory_preset(preset));
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

/// One emitted definition, storage class stripped. The forward declaration of
/// the same function is skipped: only the definition's first line ends in `{`.
fn body_of<'a>(bodies: &'a str, name: &str) -> &'a str {
    bodies
        .split("\nstatic ")
        .find(|chunk| {
            chunk
                .split_once('\n')
                .is_some_and(|(head, _)| head.contains(name) && head.ends_with('{'))
        })
        .unwrap_or_else(|| panic!("no definition of {name} in:\n{bodies}"))
}

/// A call in tail position has to reach C as a `return` of the call itself.
/// Binding the result first leaves nothing syntactically in tail position, GCC
/// declines the sibling call, and the frames pile up. The language has no loop
/// form, so this is how an ordinary million-element traversal is written.
#[test]
fn a_tail_recursive_call_returns_the_call_itself() {
    let emitted = compile_to_c(
        r#"
fn count(n: Int, acc: Int) -> Int {
  if n == 0 { acc } else { count(n - 1, acc + 1) }
}
fn main() -> Int {
  count(1000000, 0) - 999900
}
"#,
    );
    let bodies = bodies(emitted.as_str());
    let count = body_of(bodies, "cielo_fn_count");
    assert!(
        !count.contains("= CIELO_CALL_PURE(cielo_fn_count"),
        "the recursive call must not be bound on the way to the return:\n{count}"
    );
    assert!(
        count.contains("return CIELO_CALL_PURE(cielo_fn_count"),
        "and must be returned directly:\n{count}"
    );
    assert!(
        !count.starts_with("inline"),
        "`static inline` lets GCC inline the body into itself, and the copy's \
         merged exits put the innermost call back out of tail position:\n{count}"
    );

    let Some(code) = compile_and_run(emitted.as_str(), "tail_recursion") else {
        return;
    };
    assert_eq!(
        code,
        Some(100),
        "a million-deep tail recursion must not grow the stack"
    );
}

const MANAGED_TAIL_RECURSION: &str = r#"
enum List { Nil, Cons(Int, List) }

fn build(n: Int, acc: List) -> List {
  if n == 0 { acc } else { build(n - 1, Cons(n, acc)) }
}

fn len(l: List, acc: Int) -> Int {
  match l {
    | Nil => acc
    | Cons(v, rest) => len(rest, acc + 1)
  }
}

fn main() -> Int {
  let l = build(1000000, Nil());
  let n = len(l, 0);
  if n == 1000000 { 42 } else { 0 }
}
"#;

#[test]
fn managed_tail_calls_survive_every_public_memory_preset() {
    let presets = [
        ("unmanaged", MemoryPreset::Unmanaged),
        ("arc_raw", MemoryPreset::ArcRaw),
        ("arc_optimized", MemoryPreset::ArcOptimized),
        ("arc_no_verify", MemoryPreset::ArcNoVerify),
    ];

    for (name, preset) in presets {
        let emitted = compile_to_c_with_preset(MANAGED_TAIL_RECURSION, preset);
        let bodies = bodies(emitted.as_str());
        for function in ["build", "len"] {
            let body = body_of(bodies, format!("cielo_fn_{function}").as_str());
            let call = format!("CIELO_CALL_PURE(cielo_fn_{function}");
            assert!(
                body.contains(format!("return {call}").as_str()),
                "{name}: recursive {function} call must be returned directly:\n{body}"
            );
            assert!(
                !body.contains(format!("= {call}").as_str()),
                "{name}: recursive {function} call must not be bound:\n{body}"
            );
        }

        let Some(code) = compile_and_run(
            emitted.as_str(),
            format!("managed_tail_recursion_{name}").as_str(),
        ) else {
            continue;
        };
        assert_eq!(
            code,
            Some(42),
            "{name}: million-node build and traversal must not grow the stack"
        );
    }
}

/// Alternates a million times, so a frame per step overflows. `is_even`
/// returns 100 and `is_odd` returns 0, which tells the two apart in the exit
/// code if the dispatch ever entered the wrong member.
const MUTUAL: &str = r#"
fn is_even(n: Int) -> Int { if n == 0 { 100 } else { is_odd(n - 1) } }
fn is_odd(n: Int) -> Int { if n == 0 { 0 } else { is_even(n - 1) } }

fn main() -> Int { is_even(1000000) }
"#;

/// CIELO-62. Two functions in one tail-call cycle are both in tail position and
/// neither is `static inline`, and GCC still inlines one into the other, merges
/// the copy's exits and leaves the surviving call off the sibling-call path. So
/// the cycle stops being calls at all: its members become blocks of one
/// dispatch function that reaches them by `switch` and back edge.
#[test]
fn a_mutual_tail_call_cycle_becomes_one_function() {
    let emitted = compile_to_c(MUTUAL);
    let bodies = bodies(emitted.as_str());
    let cycle = body_of(bodies, "cielo_cycle_is_even");
    assert!(
        !cycle.contains("cielo_fn_is_even") && !cycle.contains("cielo_fn_is_odd"),
        "no member of the cycle may still be called from inside it:\n{cycle}"
    );
    assert!(
        cycle.contains("switch (cv_switch_index(") && cycle.contains("goto b"),
        "the members are entered by dispatch and re-entered by back edge:\n{cycle}"
    );
    assert!(
        body_of(bodies, "cielo_fn_is_odd").contains("return CIELO_CALL_PURE(cielo_cycle_is_even"),
        "and each keeps a wrapper, so callers outside the cycle are unchanged"
    );

    let Some(code) = compile_and_run(emitted.as_str(), "mutual_tail_recursion") else {
        return;
    };
    assert_eq!(
        code,
        Some(100),
        "a million-deep alternation must not grow the stack"
    );
}

/// The exit code above would pass on its own on a machine whose stack happened
/// to be large enough for the depth. This is the property that actually holds:
/// in the machine code there is no call left that reaches a cycle member.
#[test]
fn no_call_to_a_cycle_member_survives() {
    let emitted = compile_to_c(MUTUAL);
    let Some(dump) = disassemble(emitted.as_str(), "mutual_no_calls") else {
        return;
    };
    let calls = dump
        .lines()
        .filter(|line| line.contains("call"))
        .collect::<Vec<_>>();
    // Not a claim about the program, but about this test: if objdump's format
    // ever stops matching, every filter below silently finds nothing.
    assert!(
        !calls.is_empty(),
        "no call instruction at all, so the filter is not reading disassembly"
    );
    let into_cycle = calls
        .iter()
        .filter(|line| line.contains("cielo_fn_is_") || line.contains("cielo_cycle_is_"))
        .copied()
        .collect::<Vec<_>>();
    assert!(
        into_cycle.is_empty(),
        "the cycle must be reached by jump only:\n{}",
        into_cycle.join("\n")
    );
}

/// The eligibility boundary. `total` takes the head out of the cons cell, so
/// its release is scheduled after the recursive call; that release has to run
/// once the call comes back, and a tail call never comes back.
#[test]
fn a_release_after_the_call_keeps_the_binding() {
    let emitted = compile_to_c(
        r#"
enum List { Cons(Int, List), Nil }
fn total(xs: List, acc: Int) -> Int {
  match xs {
    Cons(head, tail) => total(tail, acc + head),
    Nil => acc,
  }
}
fn main() -> Int {
  total(Cons(3, Cons(4, Nil)), 0)
}
"#,
    );
    let bodies = bodies(emitted.as_str());
    let total = body_of(bodies, "cielo_fn_total");
    assert!(
        total.contains("= CIELO_CALL_PURE(cielo_fn_total"),
        "a call with work left after it stays bound:\n{total}"
    );

    let Some(code) = compile_and_run(emitted.as_str(), "released_after_call") else {
        return;
    };
    assert_eq!(code, Some(7), "and still computes the sum");
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
/// The caller owns the directory it returns and has to remove it.
fn compile_to_binary(c_source: &str, name: &str) -> Option<(PathBuf, PathBuf)> {
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
    Some((dir, binary))
}

fn compile_and_run(c_source: &str, name: &str) -> Option<Option<i32>> {
    let (dir, binary) = compile_to_binary(c_source, name)?;
    let run = Command::new(binary.as_path()).output().expect("run binary");
    let _ = std::fs::remove_dir_all(dir.as_path());
    Some(run.status.code())
}

/// `None` when either tool is missing, so callers skip rather than fail.
fn disassemble(c_source: &str, name: &str) -> Option<String> {
    if Command::new("objdump")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        eprintln!("skipping {name} disassembly check: no objdump found");
        return None;
    }
    let (dir, binary) = compile_to_binary(c_source, name)?;
    let dump = Command::new("objdump")
        .arg("-d")
        .arg(binary.as_path())
        .output()
        .expect("invoke objdump");
    let _ = std::fs::remove_dir_all(dir.as_path());
    assert!(dump.status.success(), "objdump failed on {name}");
    Some(String::from_utf8_lossy(dump.stdout.as_slice()).into_owned())
}
