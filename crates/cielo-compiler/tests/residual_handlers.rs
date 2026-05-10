//! Handle sites erasure could not discharge, checked end to end.
//!
//! A substring assertion cannot tell a clause table that dispatches from one
//! that traps, and it cannot see the reference the dispatcher forgot to
//! release, so both fixtures are compiled and run under LeakSanitizer.

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

/// The same shape with a heap-allocated argument, so the reference travels
/// through the perform site, the dispatcher's argument array, and the clause
/// function. `str_concat` allocates; a string literal would be immortal and
/// leak nothing however wrong the ownership was.
const OWNED_ARGUMENT_IN_A_CALLEE: &str = r#"
effect St { fn note(s: String) -> Int }

fn inner(s: String) -> Int with St {
  let n = do St.note(s);
  n
}

fn outer(s: String) -> Int with St {
  let a = inner(s);
  a + 1
}

fn main() -> Int {
  let msg = str_concat("hel", "lo");
  handle { outer(msg) } with St {
    | note(s, resume) => resume(str_len(s))
  }
}
"#;

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
    let Some(run) = compile_and_run(emit(TICK_IN_A_CALLEE).as_str(), "residual_tick") else {
        return;
    };
    assert_eq!(
        run.status.code(),
        Some(42),
        "resume(41) must reach the perform site: {}",
        String::from_utf8_lossy(run.stderr.as_slice())
    );
    assert_no_leak(&run);
}

#[test]
fn a_dispatched_clause_consumes_its_arguments() {
    let Some(run) = compile_and_run(emit(OWNED_ARGUMENT_IN_A_CALLEE).as_str(), "residual_owned")
    else {
        return;
    };
    assert_eq!(
        run.status.code(),
        Some(6),
        "str_len(\"hello\") + 1: {}",
        String::from_utf8_lossy(run.stderr.as_slice())
    );
    assert_no_leak(&run);
}

fn assert_no_leak(run: &Output) {
    let stderr = String::from_utf8_lossy(run.stderr.as_slice());
    assert!(
        !stderr.contains("LeakSanitizer"),
        "dispatching through the clause table leaked:\n{stderr}"
    );
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
