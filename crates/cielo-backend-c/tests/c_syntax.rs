//! The other backend tests validate emitted C by substring, so none of them
//! would notice output that is not valid C at all. This runs a real compiler
//! over it.

use cielo_base::{CfgExprId, CfgFuncId, Interner, SourceId};
use cielo_ir::cfg::{CfgExpr, CfgFunction, CfgProgram, CfgTerminator};
use cielo_ir::constants::ConstantTable;
use cielo_ir::core::{BinaryOp, Literal};
use cielo_test_support::{PassConfig, PassHarness};
use std::ffi::OsString;
use std::process::{Command, Output, Stdio};

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
fn switch_program(
    interner: &mut Interner,
    selector: impl FnOnce(&mut CfgProgram) -> CfgExprId,
) -> CfgProgram {
    let mut cfg = CfgProgram::default();
    let selector = selector(&mut cfg);

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

/// `None` when no C compiler is installed, so callers skip rather than fail.
fn compile_and_run(c_source: &str, name: &str) -> Option<Output> {
    if !c_compiler_available() {
        eprintln!("skipping {name} execution check: no C compiler found");
        return None;
    }

    let dir = std::env::temp_dir().join(format!("cielo_backend_{name}_{}", std::process::id()));
    std::fs::create_dir_all(dir.as_path()).expect("temp dir");
    let source = dir.join(format!("{name}.c"));
    let binary = dir.join(name);
    std::fs::write(source.as_path(), c_source).expect("write emitted C");

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
        "emitted C for {name} does not compile:\n{}",
        String::from_utf8_lossy(build.stderr.as_slice())
    );

    let run = Command::new(binary.as_path()).output().expect("run binary");
    let _ = std::fs::remove_dir_all(dir.as_path());
    Some(run)
}

#[test]
fn switch_terminator_emits_c_that_dispatches_on_the_selector() {
    let mut interner = Interner::new();
    let cfg = switch_program(&mut interner, |cfg| {
        let lhs = cfg.push_expr(CfgExpr::Literal(Literal::Int(1)), None);
        let rhs = cfg.push_expr(CfgExpr::Literal(Literal::Int(1)), None);
        cfg.push_expr(
            CfgExpr::Binary {
                op: BinaryOp::Add,
                lhs,
                rhs,
            },
            None,
        )
    });
    assert_eq!(cfg.validate(), Ok(()));

    let c_source = cielo_backend_c::emit(&cfg, &interner, &ConstantTable::default(), false);
    assert!(
        c_source.contains("switch (cv_switch_index("),
        "switch terminator should emit a C switch:\n{c_source}"
    );

    let Some(run) = compile_and_run(c_source.as_str(), "switch") else {
        return;
    };
    assert_eq!(
        run.status.code(),
        Some(30),
        "selector 1 + 1 should reach the third case"
    );
}

/// The selector is read through an accessor rather than `.as.i` so that a
/// mistyped selector aborts instead of jumping to whatever case the other
/// union member's bits happen to name.
#[test]
fn switch_traps_on_a_non_integer_selector() {
    let mut interner = Interner::new();
    let cfg = switch_program(&mut interner, |cfg| {
        cfg.push_expr(CfgExpr::Literal(Literal::Bool(true)), None)
    });

    let c_source = cielo_backend_c::emit(&cfg, &interner, &ConstantTable::default(), false);
    let Some(run) = compile_and_run(c_source.as_str(), "switchtrap") else {
        return;
    };
    assert_eq!(
        run.status.code(),
        None,
        "a non-integer selector should abort, not return a case"
    );
    let stderr = String::from_utf8_lossy(run.stderr.as_slice());
    assert!(
        stderr.contains("switch selector is not an integer"),
        "expected the selector trap, got: {stderr}"
    );
}

/// The driver writes the runtime header next to the emitted C, so anything that
/// includes it by name can reach it twice through another header.
#[test]
fn runtime_header_can_be_included_twice() {
    if !c_compiler_available() {
        eprintln!("skipping runtime header include check: no C compiler found");
        return;
    }

    let dir = std::env::temp_dir().join(format!("cielo_header_guard_{}", std::process::id()));
    std::fs::create_dir_all(dir.as_path()).expect("temp dir");
    std::fs::write(dir.join("cielo_runtime.h"), cielo_backend_c::RUNTIME_HEADER)
        .expect("write header");

    let source = dir.join("twice.c");
    std::fs::write(
        source.as_path(),
        concat!(
            "#define CIELO_RUNTIME_IMPL\n",
            "#include \"cielo_runtime.h\"\n",
            "#include \"cielo_runtime.h\"\n",
            "int main(void) { return (int)cv_int(0).as.i; }\n"
        ),
    )
    .expect("write source");

    let build = Command::new(c_compiler_command())
        .arg("-std=c11")
        .arg("-I")
        .arg(dir.as_path())
        .arg("-o")
        .arg(dir.join("twice"))
        .arg(source.as_path())
        .output()
        .expect("invoke C compiler");
    let stderr = String::from_utf8_lossy(build.stderr.as_slice()).into_owned();
    let _ = std::fs::remove_dir_all(dir.as_path());
    assert!(
        build.status.success(),
        "double include does not compile:\n{stderr}"
    );
}

/// The runtime's mutable state is one program-wide copy, not one per
/// translation unit. With `static` globals this linked and ran, and the second
/// unit's `perform` silently missed a handler the first unit had pushed.
#[test]
fn two_translation_units_share_one_handler_stack() {
    if !c_compiler_available() {
        eprintln!("skipping two-unit link check: no C compiler found");
        return;
    }

    let dir = std::env::temp_dir().join(format!("cielo_two_tu_{}", std::process::id()));
    std::fs::create_dir_all(dir.as_path()).expect("temp dir");
    std::fs::write(dir.join("cielo_runtime.h"), cielo_backend_c::RUNTIME_HEADER)
        .expect("write header");

    // Stands in for the generated unit: it defines the runtime.
    std::fs::write(
        dir.join("provider.c"),
        r#"#define CIELO_RUNTIME_IMPL
#include "cielo_runtime.h"

static int hits = 0;

static CieloValue tick(CieloEvidence *evidence, CieloContinuation *continuation,
                       size_t argc, const CieloValue *args) {
  (void)evidence;
  (void)continuation;
  (void)argc;
  (void)args;
  hits += 1;
  return cv_unit();
}

static CieloClauseEntry entries[1] = {{123u, tick}};
static CieloEvidence evidence;

uint32_t provider_push(void) {
  evidence.clause_count = 1u;
  evidence.clauses = entries;
  return cielo_handler_push_with_evidence(7u, &evidence);
}

int provider_hits(void) { return hits; }
size_t provider_depth(void) { return g_cielo_handler_depth; }
"#,
    )
    .expect("write provider");

    // Stands in for an FFI consumer: declarations only, linked against the
    // provider.
    std::fs::write(
        dir.join("consumer.c"),
        r#"#include "cielo_runtime.h"

uint32_t provider_push(void);
int provider_hits(void);
size_t provider_depth(void);

int main(void) {
  uint32_t theirs = provider_push();
  if (theirs == 0u)
    return 2;
  if (g_cielo_handler_depth != 1u || provider_depth() != 1u)
    return 3;
  if (cielo_handler_find_capability(7u) != theirs)
    return 4;
  (void)cielo_perform(7u, 123u, "tick", 0u, NULL);
  if (provider_hits() != 1)
    return 5;
  uint32_t mine = cielo_handler_push(9u);
  if (mine == theirs)
    return 6;
  cielo_handler_pop(mine);
  cielo_handler_pop(theirs);
  if (g_cielo_handler_depth != 0u)
    return 7;
  return 0;
}
"#,
    )
    .expect("write consumer");

    let binary = dir.join("two_tu");
    let build = Command::new(c_compiler_command())
        .arg("-std=c11")
        .arg("-Wall")
        .arg("-Werror=unused-function")
        .arg("-Werror=implicit-function-declaration")
        .arg("-I")
        .arg(dir.as_path())
        .arg("-o")
        .arg(binary.as_path())
        .arg(dir.join("provider.c"))
        .arg(dir.join("consumer.c"))
        .output()
        .expect("invoke C compiler");
    let build_stderr = String::from_utf8_lossy(build.stderr.as_slice()).into_owned();
    assert!(
        build.status.success(),
        "two units including the runtime header do not link:\n{build_stderr}"
    );

    let run = Command::new(binary.as_path())
        .output()
        .expect("run two-unit binary");
    let _ = std::fs::remove_dir_all(dir.as_path());
    assert_eq!(
        run.status.code(),
        Some(0),
        "units disagreed about the runtime's shared state:\n{}",
        String::from_utf8_lossy(run.stderr.as_slice())
    );
}
