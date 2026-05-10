//! Handle sites erasure could not discharge, checked end to end.
//!
//! A substring assertion cannot tell a clause table that dispatches from one
//! that traps, so the fixtures are compiled and run.
//!
//! They are also counted. LeakSanitizer missed the first real bug found here --
//! a dispatched clause that dropped its owned argument -- because the dead
//! frame still held the pointer, so the block came back "reachable". The
//! allocation counters in the runtime header cannot be fooled that way, and
//! `CIELO_ARC_STATS` also makes every retain and release observable, which
//! stops the C compiler folding away pairs the ARC pass emitted.

use std::ffi::OsString;
use std::process::{Command, Output, Stdio};

use cielo::{Compiler, CompilerConfig};
use cielo_base::SourceId;

/// A perform one call deeper than inlining can see, answered by a clause whose
/// value is the resumption's value. This is the shape CIELO-1 reported.
const TICK_IN_A_CALLEE: &str = r#"
effect St { fn tick() -> Int }

fn inner() -> Int with St {
  let t = do St.tick();
  t
}

fn outer() -> Int with St {
  let a = inner();
  a + 1
}

fn main() -> Int {
  handle { outer() } with St {
    | tick(resume) => resume(41)
  }
}
"#;

/// `clause` answers `St.note(String)`, performed a call deeper than inlining
/// reaches. `str_concat` allocates, where a string literal would be pooled and
/// immortal and so leak nothing however wrong the ownership was.
fn owned_argument_in_a_callee(clause: &str) -> String {
    format!(
        r#"
effect St {{ fn note(s: String) -> Int }}

fn inner(s: String) -> Int with St {{
  let n = do St.note(s);
  n
}}

fn outer(s: String) -> Int with St {{
  let a = inner(s);
  a + 1
}}

fn main() -> Int {{
  let msg = str_concat("hel", "lo");
  handle {{ outer(msg) }} with St {{
    {clause}
  }}
}}
"#
    )
}

fn emit(source: &str) -> String {
    let compiler = Compiler::new(CompilerConfig::default());
    let emitted = compiler.compile(source, SourceId::from_u32(0));
    let diagnostics = &emitted.memory.runtime.runtime.diagnostics;
    assert!(
        !diagnostics.has_errors(),
        "residual lowering must not error: {:?}",
        diagnostics
            .entries()
            .iter()
            .map(|entry| entry.code.to_owned())
            .collect::<Vec<_>>()
    );
    emitted.c_source.clone()
}

#[test]
fn a_callee_perform_dispatches_through_a_clause_table() {
    let c_source = emit(TICK_IN_A_CALLEE);
    assert!(
        c_source.contains("static const CieloClauseEntry cielo_clauses_h0[]"),
        "the handle site should publish a clause table:\n{c_source}"
    );
    assert!(
        c_source.contains(".clause_count = 1, .clauses = cielo_clauses_h0"),
        "the evidence should point at that table:\n{c_source}"
    );
}

/// The clause table is the fallback, not a replacement. A perform inlining can
/// see leaves no evidence for anything to dispatch through.
#[test]
fn an_erased_handler_publishes_no_clause_table() {
    let c_source = emit(
        r#"
effect St { fn tick() -> Int }

fn main() -> Int {
  let out = handle {
    let t = do St.tick();
    t
  } with St {
    | tick(resume) => resume(41)
  };
  out
}
"#,
    );
    assert!(
        !c_source.contains("CieloClauseEntry cielo_clauses_h"),
        "an erased handler needs no clause table:\n{c_source}"
    );
    // The header defines the push helper, so only the emitted bodies can say
    // whether anything still installs evidence.
    let bodies = c_source
        .split_once("\nstatic CieloValue cielo_fn_")
        .expect("emitted C declares at least one function")
        .1;
    assert!(
        !bodies.contains("cielo_handler_push_with_evidence("),
        "an erased handler installs no evidence at all:\n{bodies}"
    );
}

#[test]
fn a_dispatched_clause_resumes_with_the_right_value() {
    check(TICK_IN_A_CALLEE, "residual_tick", 42);
}

/// One table, two operations: the dispatcher has to match on the operation
/// symbol rather than take the first entry.
#[test]
fn a_clause_table_dispatches_each_operation_separately() {
    check(
        r#"
effect St {
  fn inc(n: Int) -> Int
  fn dec(n: Int) -> Int
}

fn inner(x: Int) -> Int with St {
  let a = do St.inc(x);
  let b = do St.dec(a);
  b
}

fn outer(x: Int) -> Int with St {
  let a = inner(x);
  a
}

fn main() -> Int {
  handle { outer(10) } with St {
    | inc(n, resume) => resume(n + 5)
    | dec(n, resume) => resume(n - 3)
  }
}
"#,
        "residual_two_ops",
        12,
    );
}

/// Two live frames for one effect. The dispatcher walks the handler stack from
/// the top, so the inner handler must win.
#[test]
fn the_innermost_residual_handler_answers_the_perform() {
    check(
        r#"
effect St { fn tick() -> Int }

fn deep() -> Int with St {
  let t = do St.tick();
  t
}

fn mid() -> Int with St {
  let a = deep();
  a
}

fn main() -> Int {
  let outer = handle {
    let innermost = handle { mid() } with St {
      | tick(resume) => resume(1)
    };
    innermost
  } with St {
    | tick(resume) => resume(2)
  };
  outer
}
"#,
        "residual_nested",
        1,
    );
}

#[test]
fn a_dispatched_clause_consumes_an_argument_it_reads() {
    check(
        &owned_argument_in_a_callee("| note(s, resume) => resume(str_len(s))"),
        "residual_owned_read",
        6,
    );
}

/// The case that leaked: nothing in the clause mentions `s`, so the reference
/// the dispatcher handed over has to be released by the clause's own frame.
#[test]
fn a_dispatched_clause_consumes_an_argument_it_ignores() {
    check(
        &owned_argument_in_a_callee("| note(s, resume) => resume(6)"),
        "residual_owned_ignored",
        7,
    );
}

/// Runs the fixture and asserts its exit code, that no allocation outlived the
/// program, and that no sanitizer complained.
fn check(source: &str, name: &str, expected_exit: i32) {
    let Some(run) = compile_and_run(instrument(emit(source).as_str()).as_str(), name) else {
        return;
    };
    let stderr = String::from_utf8_lossy(run.stderr.as_slice());
    assert_eq!(
        run.status.code(),
        Some(expected_exit),
        "{name} produced the wrong answer:\n{stderr}"
    );
    assert!(
        !stderr.contains("Sanitizer"),
        "{name} tripped a sanitizer:\n{stderr}"
    );
    let counts = stderr
        .lines()
        .find_map(|line| line.strip_prefix("cielo-arc "))
        .expect("the instrumented main prints allocation counts")
        .split_whitespace()
        .map(|count| count.parse::<u64>().expect("a count"))
        .collect::<Vec<_>>();
    assert_eq!(
        counts[0], counts[1],
        "{name} allocated {} strings and freed {}",
        counts[0], counts[1]
    );
    assert_eq!(
        counts[2], counts[3],
        "{name} allocated {} constructors and freed {}",
        counts[2], counts[3]
    );
}

/// Wraps the emitted entry point so the allocation counters can be read after
/// it returns.
fn instrument(c_source: &str) -> String {
    let mut out = c_source.replacen("int main(void) {", "static int cielo_entry(void) {", 1);
    assert!(
        out.contains("static int cielo_entry(void)"),
        "emitted C should define a main to wrap"
    );
    out.push_str(
        r#"
int main(void) {
    int code = cielo_entry();
    CieloArcStats stats = cielo_arc_stats_snapshot();
    fprintf(stderr, "cielo-arc %llu %llu %llu %llu\n",
            (unsigned long long)stats.str_allocations,
            (unsigned long long)stats.str_frees,
            (unsigned long long)stats.ctor_allocations,
            (unsigned long long)stats.ctor_frees);
    return code;
}
"#,
    );
    out
}

fn c_compiler_command() -> OsString {
    std::env::var_os("CC").unwrap_or_else(|| OsString::from("cc"))
}

/// `None` when no C compiler or no sanitizer is installed, so callers skip
/// rather than fail.
fn compile_and_run(c_source: &str, name: &str) -> Option<Output> {
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

    let dir = std::env::temp_dir().join(format!("cielo_residual_{name}_{}", std::process::id()));
    std::fs::create_dir_all(dir.as_path()).expect("temp dir");
    let source = dir.join(format!("{name}.c"));
    let binary = dir.join(name);
    std::fs::write(source.as_path(), c_source).expect("write emitted C");

    let build = Command::new(c_compiler_command())
        .arg("-std=c11")
        .arg("-DCIELO_ARC_STATS")
        .arg("-fsanitize=address")
        .arg("-g")
        .arg("-o")
        .arg(binary.as_path())
        .arg(source.as_path())
        .output()
        .expect("invoke C compiler");
    if !build.status.success() {
        let stderr = String::from_utf8_lossy(build.stderr.as_slice());
        if stderr.contains("sanitize") {
            eprintln!("skipping {name}: no AddressSanitizer support");
            let _ = std::fs::remove_dir_all(dir.as_path());
            return None;
        }
        panic!("emitted C for {name} does not compile:\n{stderr}");
    }

    let run = Command::new(binary.as_path())
        .env("ASAN_OPTIONS", "detect_leaks=1")
        .output()
        .expect("run emitted program");
    let _ = std::fs::remove_dir_all(dir.as_path());
    Some(run)
}
