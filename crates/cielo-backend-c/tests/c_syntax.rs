//! The other backend tests validate emitted C by substring, so none of them
//! would notice output that is not valid C at all. This runs a real compiler
//! over it.

use cielo_base::{CfgFuncId, Interner, SourceId};
use cielo_ir::cfg::{CfgExpr, CfgFunction, CfgProgram, CfgTerminator};
use cielo_ir::constants::ConstantTable;
use cielo_ir::core::{BinaryOp, Literal};
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

/// Nothing lowers to `Switch` yet, so the only way to exercise its emission is
/// to hand-build a CFG. `main` returns the arm the selector picks.
fn switch_program(interner: &mut Interner) -> CfgProgram {
    let mut cfg = CfgProgram::default();
    let lhs = cfg.push_expr(CfgExpr::Literal(Literal::Int(1)), None);
    let rhs = cfg.push_expr(CfgExpr::Literal(Literal::Int(1)), None);
    let selector = cfg.push_expr(
        CfgExpr::Binary {
            op: BinaryOp::Add,
            lhs,
            rhs,
        },
        None,
    );

    let targets = [10, 20, 30]
        .into_iter()
        .map(|result| {
            let block = cfg.push_block(Vec::new(), None);
            let value = cfg.push_expr(CfgExpr::Literal(Literal::Int(result)), None);
            cfg.set_terminator(block, CfgTerminator::Return(value));
            block
        })
        .collect::<Vec<_>>();
    let default = cfg.push_block(Vec::new(), None);
    let fallback = cfg.push_expr(CfgExpr::Literal(Literal::Int(99)), None);
    cfg.set_terminator(default, CfgTerminator::Return(fallback));

    let entry = cfg.push_block(Vec::new(), None);
    cfg.set_terminator(
        entry,
        CfgTerminator::Switch {
            selector,
            targets,
            default,
        },
    );

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
fn switch_terminator_emits_c_that_dispatches_on_the_selector() {
    let mut interner = Interner::new();
    let cfg = switch_program(&mut interner);
    assert_eq!(cfg.validate(), Ok(()));

    let c_source = cielo_backend_c::emit(&cfg, &interner, &ConstantTable::default(), false);
    assert!(
        c_source.contains("switch ((int)"),
        "switch terminator should emit a C switch:\n{c_source}"
    );

    if !c_compiler_available() {
        eprintln!("skipping switch execution check: no C compiler found");
        return;
    }

    let dir = std::env::temp_dir().join(format!("cielo_backend_switch_{}", std::process::id()));
    std::fs::create_dir_all(dir.as_path()).expect("temp dir");
    let source = dir.join("switch.c");
    let binary = dir.join("switch");
    std::fs::write(source.as_path(), c_source.as_str()).expect("write emitted C");

    let build = Command::new(c_compiler_command())
        .arg("-std=c11")
        .arg("-Wall")
        .arg("-Werror=switch")
        .arg("-o")
        .arg(binary.as_path())
        .arg(source.as_path())
        .output()
        .expect("invoke C compiler");
    assert!(
        build.status.success(),
        "emitted switch does not compile:\n{}",
        String::from_utf8_lossy(build.stderr.as_slice())
    );

    let run = Command::new(binary.as_path()).status().expect("run binary");
    assert_eq!(
        run.code(),
        Some(30),
        "selector 1 + 1 should reach the third case"
    );

    let _ = std::fs::remove_dir_all(dir.as_path());
}
