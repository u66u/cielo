use cielo::common::ids::{EffectLabelId, SourceId};
use cielo::common::symbols::Interner;
use cielo::ir::core::StmtKind;
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
    assert_eq!(residual.program().functions().len(), 2);
    assert_eq!(residual.program().entrypoints().len(), 1);
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
    assert_eq!(residual.program().handlers().len(), 1);
    let main_body = residual.program().functions()[0].body;
    assert_eq!(
        residual.sema().effects_of_stmt[main_body.index()],
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
    assert_eq!(residual.program().structs().len(), 1);
    assert_eq!(residual.program().enums().len(), 1);
    assert!(residual.diagnostics().entries().is_empty());
}

#[test]
fn residualize_erases_declared_effect_annotations_and_keeps_call_summaries() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn ping() -> Int with Console {
  do Console.print("x");
  7
}
fn main() -> Int {
  let y = ping();
  y
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);

    for function in residual.program().functions() {
        assert!(
            function.declared_effects.is_empty(),
            "residual function effects should be erased"
        );
    }

    let ping_id = residual
        .program()
        .functions()
        .iter()
        .enumerate()
        .find_map(|(idx, function)| {
            (interner.resolve(function.name) == Some("ping")).then_some(idx)
        })
        .expect("ping function must exist");
    let ping_effects = residual
        .residual()
        .function_effect_summary
        .get(&cielo::common::ids::FuncId::new(ping_id))
        .expect("ping summary must exist");
    assert!(ping_effects.contains(EffectLabelId::from_u32(0)));

    let main_body = residual
        .program()
        .functions()
        .iter()
        .find(|f| interner.resolve(f.name) == Some("main"))
        .expect("main")
        .body;
    let mut stack = vec![main_body];
    let mut saw_call = false;
    while let Some(stmt_id) = stack.pop() {
        let stmt = residual.program().stmt(stmt_id).expect("reachable stmt");
        match &stmt.kind {
            StmtKind::Call { effects, .. } => {
                assert!(effects.contains(EffectLabelId::from_u32(0)));
                saw_call = true;
            }
            StmtKind::Let { next, .. } | StmtKind::Perform { next, .. } => stack.push(*next),
            StmtKind::Val { value, next, .. } => {
                stack.push(*value);
                stack.push(*next);
            }
            StmtKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                stack.push(*then_branch);
                stack.push(*else_branch);
            }
            StmtKind::Match { arms, default, .. } => {
                for arm in arms {
                    stack.push(arm.body);
                }
                if let Some(default_stmt) = default {
                    stack.push(*default_stmt);
                }
            }
            StmtKind::Handle { body, next, .. } | StmtKind::Stage { body, next, .. } => {
                stack.push(*body);
                if let Some(next_stmt) = next {
                    stack.push(*next_stmt);
                }
            }
            StmtKind::Return(_) | StmtKind::Hole { .. } | StmtKind::Error(_) => {}
        }
    }
    assert!(saw_call, "expected at least one call in main flow");
}
