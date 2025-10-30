use cielo::common::ids::SourceId;
use cielo::common::symbols::Interner;
use cielo::ir::core::{ExprKind, StmtKind};
use cielo::pipeline::phases::Stage;
use cielo::{Compiler, CompilerConfig};

#[test]
fn ct_and_bta_tables_are_populated() {
    let src = r#"
fn main() -> Int {
  let x = 1 + 2;
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);

    assert!(!residual.ct.ct_cache.is_empty());
    assert!(
        residual
            .bta
            .stage_of_expr
            .values()
            .any(|stage| matches!(stage, Stage::Ct))
    );
}

#[test]
fn runtime_directive_forces_runtime_stage_inside_block() {
    let src = r#"
fn main() -> Int {
  let y = @runtime { 1 + 2 };
  y
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);

    let main = residual.program.functions().first().expect("main");
    let mut cursor = main.body;
    let mut stage_body = None;
    while let Some(stmt) = residual.program.stmt(cursor) {
        match &stmt.kind {
            StmtKind::Val { value, next, .. } => {
                if let Some(StmtKind::Stage { body, .. }) =
                    residual.program.stmt(*value).map(|n| &n.kind)
                {
                    stage_body = Some(*body);
                    break;
                }
                cursor = *next;
            }
            StmtKind::Let { next, .. } => cursor = *next,
            StmtKind::Return(_) => break,
            _ => break,
        }
    }

    let stage_body = stage_body.expect("expected stage body");
    let staged_return_expr = match residual.program.stmt(stage_body).map(|n| &n.kind) {
        Some(StmtKind::Return(expr)) => *expr,
        _ => panic!("expected return in stage body"),
    };

    assert!(matches!(
        residual.bta.stage_of_expr.get(&staged_return_expr),
        Some(Stage::Rt(_))
    ));
}

#[test]
fn comptime_directive_forces_ct_stage_inside_block() {
    let src = r#"
fn main() -> Int {
  let x = 1;
  let y = @comptime { x };
  y
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);

    let main = residual.program.functions().first().expect("main");
    let mut cursor = main.body;
    let mut stage_body = None;
    while let Some(stmt) = residual.program.stmt(cursor) {
        match &stmt.kind {
            StmtKind::Val { value, next, .. } => {
                if let Some(StmtKind::Stage { body, .. }) =
                    residual.program.stmt(*value).map(|n| &n.kind)
                {
                    stage_body = Some(*body);
                    break;
                }
                cursor = *next;
            }
            StmtKind::Let { next, .. } => cursor = *next,
            StmtKind::Return(_) => break,
            _ => break,
        }
    }

    let stage_body = stage_body.expect("expected stage body");
    let staged_return_expr = match residual.program.stmt(stage_body).map(|n| &n.kind) {
        Some(StmtKind::Return(expr)) => *expr,
        _ => panic!("expected return in stage body"),
    };

    assert!(matches!(
        residual.program.expr(staged_return_expr).map(|e| &e.kind),
        Some(ExprKind::Var(_))
    ));
    assert!(matches!(
        residual.bta.stage_of_expr.get(&staged_return_expr),
        Some(Stage::Ct)
    ));
}
