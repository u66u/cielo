use cielo::common::ids::SourceId;
use cielo::common::symbols::Interner;
use cielo::sema::effect::SortedEffectRow;
use cielo::{Compiler, CompilerConfig};

#[test]
fn compiles_source_through_v0_skeleton_pipeline() {
    let src = r#"
fn add(a: Int, b: Int) -> Int {
  a + b
}
fn main() -> Int {
  add(1, 2)
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);
    assert_eq!(residual.program.functions().len(), 2);
    assert_eq!(residual.program.entrypoints().len(), 1);
}

#[test]
fn compiles_handle_flow_and_keeps_root_stmt_effects_discharged() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  let x = handle { do Console.print("x"); 7 } with Console {
    | print(s) => 0
  };
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);
    assert_eq!(residual.program.handlers().len(), 1);
    let main_body = residual.program.functions()[0].body;
    assert_eq!(
        residual.sema.effects_of_stmt[main_body.index()],
        SortedEffectRow::empty()
    );
}

#[test]
fn compiles_adt_constructor_flow_without_diagnostics() {
    let src = r#"
enum Option { Some(Int), None }
struct Pair { a: Int, b: Int }
fn main() -> Int {
  let x = Some(1);
  let y = Pair(1, 2);
  0
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);
    assert_eq!(residual.program.structs().len(), 1);
    assert_eq!(residual.program.enums().len(), 1);
    assert!(residual.diagnostics.entries().is_empty());
}
