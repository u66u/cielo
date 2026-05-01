//! The other backend tests validate emitted C by substring, so none of them
//! would notice output that is not valid C at all. This runs a real compiler
//! over it.

use cielo_base::{Interner, SourceId};
use cielo_test_support::{PassConfig, PassHarness};
use std::ffi::OsString;
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

const CASES: &[(&str, &str)] = &[
    (
        "arithmetic",
        r#"
fn main() -> Int {
  let x = 1 + 2 * 3;
  x - 4
}
"#,
    ),
    (
        "adt_and_match",
        r#"
enum Shape { Circle(Int), Square(Int) }
fn area(s: Shape) -> Int {
  match s {
    Circle(r) => r,
    Square(w) => w
  }
}
fn main() -> Int {
  area(Circle(7))
}
"#,
    ),
    (
        "struct_field_access",
        r#"
struct Pair { a: Int, b: Int }
fn main() -> Int {
  let p = Pair(10, 32);
  p.a + p.b
}
"#,
    ),
    (
        "handler_with_resume",
        r#"
effect St { fn note(n: Int) -> Int }
fn main() -> Int {
  let out = handle {
    let v = do St.note(7);
    v
  } with St {
    | note(n, resume) => resume(n + 1)
  };
  out
}
"#,
    ),
    (
        "two_handler_specializations",
        r#"
effect St { fn note(n: Int) -> Int }
fn work(x: Int) -> Int with St {
  do St.note(x);
  x + 1
}
fn main() -> Int {
  let a = handle { work(1) } with St { | note(n) => 10 };
  let b = handle { work(2) } with St { | note(n) => 20 };
  a + b
}
"#,
    ),
    (
        "branching_ownership",
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
    ),
];

#[test]
fn emitted_c_passes_a_real_compiler_syntax_check() {
    if !c_compiler_available() {
        eprintln!("skipping emitted C syntax check: no C compiler found");
        return;
    }

    let dir = std::env::temp_dir().join(format!("cielo_backend_syntax_{}", std::process::id()));
    std::fs::create_dir_all(dir.as_path()).expect("temp dir");

    for (name, source) in CASES {
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

        let path = dir.join(format!("{name}.c"));
        std::fs::write(path.as_path(), &compiled.c_source).expect("write emitted C");
        let check = Command::new(c_compiler_command())
            .arg("-std=c11")
            .arg("-Wall")
            .arg("-Werror=implicit-function-declaration")
            .arg("-Werror=incompatible-pointer-types")
            .arg("-Werror=switch")
            .arg("-fsyntax-only")
            .arg(path.as_path())
            .output()
            .expect("invoke C compiler");
        assert!(
            check.status.success(),
            "emitted C for {name} does not compile:\n{}",
            String::from_utf8_lossy(check.stderr.as_slice())
        );
    }

    let _ = std::fs::remove_dir_all(dir.as_path());
}
