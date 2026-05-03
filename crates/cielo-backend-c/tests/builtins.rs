//! Builtins are the only way a program says anything beyond its exit status,
//! so these cases assert on stdout rather than on the process result.

use cielo_base::{Interner, SourceId};
use cielo_test_support::{PassConfig, PassHarness};
use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

fn c_compiler_command() -> OsString {
    std::env::var_os("CC").unwrap_or_else(|| OsString::from("cc"))
}

fn c_compiler_available() -> bool {
    Command::new(c_compiler_command())
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// Compiles `source` through the pipeline, builds the emitted C, runs it, and
/// returns stdout.
fn run_program(dir: &Path, name: &str, source: &str) -> String {
    let mut interner = Interner::new();
    let compiler = PassHarness::new(PassConfig::default());
    let compiled = compiler.compile_source_to_c(source, SourceId::from_u32(0), &mut interner);
    assert!(
        !compiled.residual.diagnostics().has_errors(),
        "case {name} did not reach C emission: {:?}",
        compiled
            .residual
            .diagnostics()
            .entries()
            .iter()
            .map(|d| d.code.to_owned())
            .collect::<Vec<_>>()
    );

    let source_path = dir.join(format!("{name}.c"));
    let binary_path = dir.join(format!("{name}.bin"));
    std::fs::write(source_path.as_path(), &compiled.c_source).expect("write emitted C");

    let build = Command::new(c_compiler_command())
        .arg("-std=c11")
        .arg("-Wall")
        .arg("-Werror=implicit-function-declaration")
        .arg("-DCIELO_ARC_STATS")
        .arg("-o")
        .arg(binary_path.as_path())
        .arg(source_path.as_path())
        .output()
        .expect("invoke C compiler");
    assert!(
        build.status.success(),
        "emitted C for {name} does not compile:\n{}",
        String::from_utf8_lossy(build.stderr.as_slice())
    );

    let run = Command::new(binary_path.as_path())
        .output()
        .expect("run compiled program");
    assert!(
        run.status.success(),
        "{name} exited with {:?}:\n{}",
        run.status.code(),
        String::from_utf8_lossy(run.stderr.as_slice())
    );
    String::from_utf8(run.stdout).expect("stdout is utf-8")
}

#[test]
fn print_builtin_writes_each_value_kind() {
    if !c_compiler_available() {
        eprintln!("skipping print builtin test: no C compiler found");
        return;
    }
    let dir = std::env::temp_dir().join(format!("cielo_builtins_{}", std::process::id()));
    std::fs::create_dir_all(dir.as_path()).expect("temp dir");

    let stdout = run_program(
        dir.as_path(),
        "print_values",
        r#"
fn main() -> Int {
  print(1234);
  print(true);
  print("hello");
  0
}
"#,
    );
    assert_eq!(stdout, "1234\ntrue\nhello\n");

    let _ = std::fs::remove_dir_all(dir.as_path());
}

/// A `print` reached through a call, not straight from `main`, so the builtin
/// has to survive inlining and the staging passes.
#[test]
fn print_builtin_survives_a_call_boundary() {
    if !c_compiler_available() {
        eprintln!("skipping print builtin test: no C compiler found");
        return;
    }
    let dir = std::env::temp_dir().join(format!("cielo_builtins_call_{}", std::process::id()));
    std::fs::create_dir_all(dir.as_path()).expect("temp dir");

    let stdout = run_program(
        dir.as_path(),
        "print_in_callee",
        r#"
fn announce(n: Int) -> Int {
  print(n);
  n
}
fn main() -> Int {
  let a = announce(7);
  let b = announce(8);
  a + b - 15
}
"#,
    );
    assert_eq!(stdout, "7\n8\n");

    let _ = std::fs::remove_dir_all(dir.as_path());
}

/// Runtime-built strings: `str_concat` allocates, so equality can no longer
/// ride on literal pooling, and ARC has to free what it produced.
#[test]
fn string_builtins_construct_compare_and_measure() {
    if !c_compiler_available() {
        eprintln!("skipping string builtin test: no C compiler found");
        return;
    }
    let dir = std::env::temp_dir().join(format!("cielo_builtins_str_{}", std::process::id()));
    std::fs::create_dir_all(dir.as_path()).expect("temp dir");

    let stdout = run_program(
        dir.as_path(),
        "string_ops",
        r#"
fn main() -> Int {
  let joined = str_concat("hello, ", "world");
  print(joined);
  print(str_len(joined));
  print(str_len(""));
  0
}
"#,
    );
    assert_eq!(stdout, "hello, world\n12\n0\n");

    // Two separately built strings with the same bytes must compare equal:
    // pointer identity would say no.
    let stdout = run_program(
        dir.as_path(),
        "string_equality",
        r#"
fn main() -> Int {
  let a = str_concat("ab", "cd");
  let b = str_concat("abc", "d");
  print(a == b);
  print(a == "abcd");
  print(a == "other");
  0
}
"#,
    );
    assert_eq!(stdout, "true\ntrue\nfalse\n");

    let _ = std::fs::remove_dir_all(dir.as_path());
}

/// A nested call's result has no value id, so ARC cannot release it at the
/// call site. Builtin arguments are therefore sink arguments, released by the
/// runtime; treating them as borrows leaked the string built here.
#[test]
fn owned_temporaries_passed_to_builtins_are_released() {
    if !c_compiler_available() {
        eprintln!("skipping string builtin test: no C compiler found");
        return;
    }
    let dir = std::env::temp_dir().join(format!("cielo_builtins_temp_{}", std::process::id()));
    std::fs::create_dir_all(dir.as_path()).expect("temp dir");

    let stdout = run_program(
        dir.as_path(),
        "nested_owned_temporary",
        r#"
enum Item { Named(String) }
fn label(i: Item) -> String {
  match i { Named(s) => s }
}
fn main() -> Int {
  print(label(Named(str_concat("wid", "get"))));
  print(str_len(str_concat("a", "b")));
  0
}
"#,
    );
    assert_eq!(stdout, "widget\n2\n");

    let _ = std::fs::remove_dir_all(dir.as_path());
}

/// The symbol-keyed table, not the direct call path: an effect operation named
/// `print` with no handler in scope resolves to the builtin instead of trapping.
#[test]
fn unhandled_print_effect_resolves_through_the_builtin_table() {
    if !c_compiler_available() {
        eprintln!("skipping print builtin test: no C compiler found");
        return;
    }
    let dir = std::env::temp_dir().join(format!("cielo_builtins_perform_{}", std::process::id()));
    std::fs::create_dir_all(dir.as_path()).expect("temp dir");

    let stdout = run_program(
        dir.as_path(),
        "print_via_perform",
        r#"
effect Console { fn print(s: String) -> () }
fn main() -> Int with Console {
  do Console.print("from an effect");
  0
}
"#,
    );
    assert_eq!(stdout, "from an effect\n");

    let _ = std::fs::remove_dir_all(dir.as_path());
}
