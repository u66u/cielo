//! Handler evidence placement, checked against emitted C and a real run.
//!
//! The arena half matters more than the stack half: stack placement is what the
//! backend already did unconditionally, so only the arena path is new code, and
//! a leak there is invisible to substring assertions.

use cielo_base::{EffectLabelId, Interner, LinearFuncId, VarId};
use cielo_ir::core::Literal;
use cielo_ir::linear::{LinearExpr, LinearFunction, LinearProgram, LinearStmt};
use std::ffi::OsString;
use std::process::{Command, Output, Stdio};

const HANDLED: EffectLabelId = EffectLabelId::INVALID;

fn emit(linear: &LinearProgram, interner: &Interner) -> String {
    let cfg = cielo_runtime::cfg_lower::run(linear);
    let constants = cielo_staging::passes::constant_table::build_for_linear(linear);
    cielo_backend_c::emit(&cfg, interner, &constants, false)
}

fn bodies(emitted: &str) -> &str {
    let start = emitted
        .find("\nstatic CieloValue cielo_fn_")
        .expect("emitted C should define at least one function");
    &emitted[start..]
}

#[test]
fn a_confined_handler_keeps_its_evidence_in_the_c_frame() {
    let mut interner = Interner::new();
    let main = interner.intern("main");
    let mut linear = LinearProgram::default();
    let zero = linear.push_expr(LinearExpr::Literal(Literal::Int(0)));
    let body = linear.push_stmt(LinearStmt::Return(zero));
    let handle = linear.push_stmt(LinearStmt::Handle {
        effect: HANDLED,
        clauses: Vec::new(),
        body,
        next: None,
    });
    linear.functions.push(LinearFunction {
        id: LinearFuncId::from_u32(0),
        name: main,
        params: vec![],
        body: handle,
    });
    linear.entrypoints.push(LinearFuncId::from_u32(0));

    let emitted = emit(&linear, &interner);
    let bodies = bodies(emitted.as_str());
    assert!(
        bodies.contains("CieloEvidence hev"),
        "confined evidence should be an automatic, not a pointer:\n{bodies}"
    );
    assert!(
        bodies.contains(", &hev"),
        "a stack slot is pushed by address:\n{bodies}"
    );
    assert!(
        !bodies.contains("cielo_region_open"),
        "a region with no arena slot should not open an arena:\n{bodies}"
    );
}

/// Built to force the arena while still terminating normally: a control call
/// inside the region may suspend, which the analysis cannot bound, but this one
/// returns, so the close still runs.
///
/// Terminating normally is the whole point. `cielo_trap` calls `abort`, and
/// LeakSanitizer's check runs at ordinary exit -- a fixture that traps would
/// report clean however badly the arena leaked.
fn escaping_handler(interner: &mut Interner) -> LinearProgram {
    let main = interner.intern("main");
    let callee_name = interner.intern("suspends");
    let mut linear = LinearProgram::default();
    let seven = linear.push_expr(LinearExpr::Literal(Literal::Int(7)));
    let callee_body = linear.push_stmt(LinearStmt::Return(seven));

    let result = VarId::from_u32(0);
    let value = linear.push_expr(LinearExpr::Var(result));
    let tail = linear.push_stmt(LinearStmt::Return(value));
    let call = linear.push_stmt(LinearStmt::ControlCall {
        result,
        callee: callee_name,
        callee_fn: LinearFuncId::from_u32(1),
        args: vec![],
        next: tail,
    });
    let handle = linear.push_stmt(LinearStmt::Handle {
        effect: HANDLED,
        clauses: Vec::new(),
        body: call,
        next: None,
    });
    linear.functions.push(LinearFunction {
        id: LinearFuncId::from_u32(0),
        name: main,
        params: vec![],
        body: handle,
    });
    linear.functions.push(LinearFunction {
        id: LinearFuncId::from_u32(1),
        name: callee_name,
        params: vec![],
        body: callee_body,
    });
    linear.entrypoints.push(LinearFuncId::from_u32(0));
    linear
}

#[test]
fn an_escaping_handler_allocates_its_evidence_in_the_region() {
    let mut interner = Interner::new();
    let linear = escaping_handler(&mut interner);
    let emitted = emit(&linear, &interner);
    let bodies = bodies(emitted.as_str());

    assert!(
        bodies.contains("CieloEvidence *hev"),
        "unproven evidence should be a pointer into the arena:\n{bodies}"
    );
    assert!(
        bodies.contains("cielo_region_alloc(&reg"),
        "the slot should come from the region:\n{bodies}"
    );
    assert_eq!(
        bodies.matches("cielo_region_open(&reg").count(),
        bodies.matches("cielo_region_close(&reg").count(),
        "every open needs its close, or the arena leaks:\n{bodies}"
    );
    assert!(
        !bodies.contains(", &hev"),
        "an arena slot is already a pointer and must not be address-taken again:\n{bodies}"
    );
}

/// A substring check cannot tell a working arena from one that never frees, so
/// this one runs the program under LeakSanitizer.
#[test]
fn the_region_arena_frees_what_it_allocates() {
    let mut interner = Interner::new();
    let linear = escaping_handler(&mut interner);
    let emitted = emit(&linear, &interner);

    let Some(run) = compile_and_run(emitted.as_str(), "region_arena") else {
        return;
    };
    let stderr = String::from_utf8_lossy(run.stderr.as_slice());
    assert!(
        !stderr.contains("LeakSanitizer"),
        "region arena leaked:\n{stderr}"
    );
    assert_eq!(
        run.status.code(),
        Some(7),
        "the fixture must reach a normal exit, or the leak check never ran"
    );
}

fn c_compiler_command() -> OsString {
    std::env::var_os("CC").unwrap_or_else(|| OsString::from("cc"))
}

/// `None` when no C compiler is installed, so callers skip rather than fail.
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

    let dir = std::env::temp_dir().join(format!("cielo_regions_{name}_{}", std::process::id()));
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
        // A toolchain without ASan should skip, not fail the suite.
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
        .expect("run binary");
    let _ = std::fs::remove_dir_all(dir.as_path());
    Some(run)
}
