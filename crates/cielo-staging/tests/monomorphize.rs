use std::ffi::OsString;
use std::process::{Command, Stdio};

use cielo_base::Interner;
use cielo_base::SourceId;
use cielo_ir::core::CoreTypeRef;
use cielo_sema::check_core;
use cielo_staging::passes::monomorphize;
use cielo_staging::pipeline::phases::Monomorphized;
use cielo_test_support::{PassConfig, PassHarness};

fn monomorphize_source(src: &str, interner: &mut Interner) -> Monomorphized {
    let compiler = PassHarness::new(PassConfig::default());
    let core = compiler.parse_and_lower_to_core(src, SourceId::from_u32(0), interner);
    let (program, diagnostics) = core.into_parts();
    monomorphize::run(check_core(program, diagnostics))
}

fn instances_named(mono: &Monomorphized, interner: &Interner, name: &str) -> usize {
    mono.program()
        .functions()
        .iter()
        .filter(|function| interner.resolve(function.name) == Some(name))
        .count()
}

fn diagnostic_codes(mono: &Monomorphized) -> Vec<&'static str> {
    mono.diagnostics()
        .entries()
        .iter()
        .map(|entry| entry.code)
        .collect()
}

#[test]
fn monomorphize_summary_tracks_identity_in_v0() {
    let src = r#"
fn add(a: Int, b: Int) -> Int {
  a + b
}

fn main() -> Int {
  add(1, 2)
}
"#;
    let mut interner = Interner::new();
    let compiler = PassHarness::new(PassConfig::default());
    let residual = compiler.compile_source_baseline(src, SourceId::from_u32(0), &mut interner);

    assert_eq!(residual.program().functions().len(), 2);
    assert_eq!(residual.mono().source_to_mono.len(), 2);
}

#[test]
fn v1_pipeline_compacts_monomorphization_summary_after_pruning() {
    let src = r#"
fn add(a: Int, b: Int) -> Int {
  a + b
}

fn main() -> Int {
  add(1, 2)
}
"#;
    let mut interner = Interner::new();
    let compiler = PassHarness::new(PassConfig::default());
    let residual = compiler.compile_source(src, SourceId::from_u32(1), &mut interner);
    let func_count = residual.program().functions().len();
    assert!(
        residual
            .mono()
            .source_to_mono
            .iter()
            .all(|(source, monos)| {
                source.index() < func_count && monos.iter().all(|mono| mono.index() < func_count)
            }),
        "v1 residualization/specialization must keep monomorphization ids within compacted function bounds"
    );
    assert_eq!(
        residual.program().functions().len(),
        1,
        "v1 may fold/prune unused helpers from the final residual function set"
    );
}

#[test]
fn v1_pipeline_keeps_runtime_reachable_helpers() {
    let src = r#"
fn add(a: Int, b: Int) -> Int {
  a + b
}

fn main() -> Int {
  let x = @runtime { 1 };
  add(x, 2)
}
"#;
    let mut interner = Interner::new();
    let compiler = PassHarness::new(PassConfig::default());
    let residual = compiler.compile_source(src, SourceId::from_u32(2), &mut interner);
    let names = residual
        .program()
        .functions()
        .iter()
        .map(|f| interner.resolve(f.name).unwrap_or("<missing>").to_owned())
        .collect::<Vec<_>>();
    assert!(
        names.iter().any(|name| name == "add"),
        "runtime-dependent calls must keep helper function bodies reachable in v1"
    );
    assert!(
        names.iter().any(|name| name == "main"),
        "entrypoint must remain reachable in v1"
    );
    assert!(
        residual.mono().source_to_mono.iter().all(|(_, monos)| monos
            .iter()
            .all(|mono| mono.index() < residual.program().functions().len())),
        "monomorphization summary must remain in-bounds after v1 pruning"
    );
}

#[test]
fn emits_one_copy_per_distinct_type_argument_tuple() {
    let src = r#"
fn identity[T](x: T) -> T {
  x
}

fn main() -> Int {
  let a = identity(1);
  let b = identity(true);
  let c = identity(2);
  if b { a + c } else { 0 }
}
"#;
    let mut interner = Interner::new();
    let mono = monomorphize_source(src, &mut interner);
    assert!(
        !mono.diagnostics().has_errors(),
        "{:?}",
        diagnostic_codes(&mono)
    );
    assert_eq!(
        instances_named(&mono, &interner, "identity"),
        2,
        "Int and Bool are two instantiations, and the second Int call reuses the first"
    );
}

#[test]
fn specialized_signatures_are_free_of_type_parameters() {
    let src = r#"
enum Option[T] { Some(T), None }

fn unwrap_or[T](opt: Option[T], fallback: T) -> T {
  match opt {
    | Some(v) => v
    | None => fallback
  }
}

fn main() -> Int {
  unwrap_or(Some(7), 0)
}
"#;
    let mut interner = Interner::new();
    let mono = monomorphize_source(src, &mut interner);
    assert!(
        !mono.diagnostics().has_errors(),
        "{:?}",
        diagnostic_codes(&mono)
    );
    for function in mono.program().functions() {
        assert!(
            !function.return_type.mentions_param()
                && !function.param_types.iter().any(CoreTypeRef::mentions_param),
            "generic templates must not survive monomorphization"
        );
    }

    let specialized = mono
        .program()
        .functions()
        .iter()
        .find(|function| interner.resolve(function.name) == Some("unwrap_or"))
        .expect("specialized copy of unwrap_or");
    assert_eq!(
        specialized.param_types[0],
        CoreTypeRef::Applied {
            name: interner.intern("Option"),
            args: vec![CoreTypeRef::Primitive(
                cielo_ir::core::PrimitiveTypeRef::Int
            )],
        }
    );
}

#[test]
fn specialized_copies_keep_the_declared_effect_row() {
    let src = r#"
effect LocalState { fn tick() -> Int }

fn tagged[T](x: T) -> T with LocalState {
  do LocalState.tick();
  x
}

fn main() -> Int {
  let a = handle { tagged(5) } with LocalState {
    | tick(resume) => resume(1)
  };
  let b = handle { tagged(true) } with LocalState {
    | tick(resume) => resume(1)
  };
  if b { a } else { 0 }
}
"#;
    let mut interner = Interner::new();
    let mono = monomorphize_source(src, &mut interner);
    assert!(
        !mono.diagnostics().has_errors(),
        "{:?}",
        diagnostic_codes(&mono)
    );

    let rows = mono
        .program()
        .functions()
        .iter()
        .filter(|function| interner.resolve(function.name) == Some("tagged"))
        .map(|function| function.declared_effects.clone())
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter().all(|row| !row.is_empty()),
        "effect rows are monomorphic and must survive specialization unchanged"
    );
}

#[test]
fn clones_handlers_installed_inside_a_generic_body() {
    let src = r#"
effect LocalState { fn tick() -> Int }

fn wrapped[T](x: T) -> T {
  let n = handle { do LocalState.tick(); 1 } with LocalState {
    | tick(resume) => resume(2)
  };
  x
}

fn main() -> Int {
  let a = wrapped(5);
  let b = wrapped(true);
  if b { a } else { 0 }
}
"#;
    let mut interner = Interner::new();
    let mono = monomorphize_source(src, &mut interner);
    assert!(
        !mono.diagnostics().has_errors(),
        "{:?}",
        diagnostic_codes(&mono)
    );
    assert_eq!(
        mono.program().handlers().len(),
        3,
        "each instance needs its own handler: clause bodies belong to the specialized body"
    );
}

#[test]
fn cloned_bodies_do_not_share_var_ids_with_their_template() {
    let src = r#"
fn identity[T](x: T) -> T {
  x
}

fn main() -> Int {
  let a = identity(1);
  let b = identity(true);
  if b { a } else { 0 }
}
"#;
    let mut interner = Interner::new();
    let mono = monomorphize_source(src, &mut interner);
    let params = mono
        .program()
        .functions()
        .iter()
        .filter(|function| interner.resolve(function.name) == Some("identity"))
        .flat_map(|function| function.params.iter().copied())
        .collect::<Vec<_>>();
    assert_eq!(params.len(), 2);
    assert_ne!(
        params[0], params[1],
        "ownership_of_var is keyed globally, so instances must not share var ids"
    );
}

#[test]
fn rejects_infinitely_polymorphic_recursion() {
    let src = r#"
enum Box[T] { Wrap(T) }

fn grow[T](x: T) -> Int {
  grow(Wrap(x))
}

fn main() -> Int {
  grow(1)
}
"#;
    let mut interner = Interner::new();
    let mono = monomorphize_source(src, &mut interner);
    assert!(
        diagnostic_codes(&mono).contains(&"MONO_POLYMORPHIC_RECURSION"),
        "an ever-growing instantiation must be reported instead of looping: {:?}",
        diagnostic_codes(&mono)
    );
}

#[test]
fn drops_generic_functions_that_are_never_called() {
    let src = r#"
enum Option[T] { Some(T), None }

fn unused[T](x: T) -> Option[T] {
  Some(x)
}

fn main() -> Int {
  1
}
"#;
    let mut interner = Interner::new();
    let mono = monomorphize_source(src, &mut interner);
    assert!(
        !mono.diagnostics().has_errors(),
        "{:?}",
        diagnostic_codes(&mono)
    );
    assert_eq!(instances_named(&mono, &interner, "unused"), 0);
}

#[test]
fn rejects_a_generic_entrypoint() {
    let src = r#"
fn main[T](x: T) -> T {
  x
}
"#;
    let mut interner = Interner::new();
    let mono = monomorphize_source(src, &mut interner);
    assert!(
        diagnostic_codes(&mono).contains(&"MONO_GENERIC_ENTRYPOINT"),
        "{:?}",
        diagnostic_codes(&mono)
    );
}

fn c_compiler_command() -> OsString {
    std::env::var_os("CC").unwrap_or_else(|| OsString::from("cc"))
}

#[test]
fn generic_function_and_generic_enum_run_with_the_expected_exit_code() {
    let src = r#"
enum Option[T] { Some(T), None }

struct Pair[A, B] { first: A, second: B }

fn identity[T](x: T) -> T {
  x
}

fn unwrap_or[T](opt: Option[T], fallback: T) -> T {
  match opt {
    | Some(v) => v
    | None => fallback
  }
}

fn first[A, B](p: Pair[A, B]) -> A {
  p.first
}

fn main() -> Int {
  let present = unwrap_or(Some(30), 0);
  let missing = unwrap_or(None(), 4);
  let flag = unwrap_or(Some(true), false);
  let left = first(Pair(8, flag));
  identity(present) + missing + left
}
"#;
    let mut interner = Interner::new();
    let compiler = PassHarness::new(PassConfig::default());
    let compiled = compiler.compile_source_to_c(src, SourceId::from_u32(0), &mut interner);
    assert!(
        !compiled.residual.diagnostics().has_errors(),
        "generic program must reach C emission: {:?}",
        compiled
            .residual
            .diagnostics()
            .entries()
            .iter()
            .map(|entry| entry.code)
            .collect::<Vec<_>>()
    );

    if Command::new(c_compiler_command())
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        eprintln!("skipping generics execution check: no C compiler found");
        return;
    }

    let dir = std::env::temp_dir().join(format!("cielo_generics_{}", std::process::id()));
    std::fs::create_dir_all(dir.as_path()).expect("temp dir");
    let source = dir.join("generics.c");
    let binary = dir.join("generics");
    std::fs::write(source.as_path(), &compiled.c_source).expect("write emitted C");

    let build = Command::new(c_compiler_command())
        .arg("-std=c11")
        .arg("-o")
        .arg(binary.as_path())
        .arg(source.as_path())
        .output()
        .expect("invoke C compiler");
    assert!(
        build.status.success(),
        "emitted C does not compile:\n{}",
        String::from_utf8_lossy(build.stderr.as_slice())
    );

    let run = Command::new(binary.as_path()).status().expect("run binary");
    let _ = std::fs::remove_dir_all(dir.as_path());
    assert_eq!(run.code(), Some(42), "30 + 4 + 8");
}
