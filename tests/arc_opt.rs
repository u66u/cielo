use cielo::common::diagnostics::DiagnosticBag;
use cielo::common::ids::{SourceId, StmtId};
use cielo::common::symbols::Interner;
use cielo::ir::core::{CoreProgram, ExprKind, StmtKind};
use cielo::passes::arc_insert::{ArcOpKind, plan};
use cielo::passes::arc_opt::optimize;
use cielo::sema::typecheck::typecheck_core;
use cielo::{Compiler, CompilerConfig};

fn lower_and_typecheck(source: &str) -> (CoreProgram, cielo::pipeline::phases::SemanticTables) {
    let compiler = Compiler::new(CompilerConfig::default());
    let mut interner = Interner::new();
    let core = compiler.parse_and_lower_to_core(source, SourceId::from_u32(0), &mut interner);
    let program = core.program().clone();
    let mut diagnostics = DiagnosticBag::default();
    let sema = typecheck_core(&program, &mut diagnostics);
    assert!(
        !diagnostics.has_errors(),
        "fixture must typecheck cleanly, got diagnostics: {:?}",
        diagnostics.entries()
    );
    (program, sema)
}

#[test]
fn arc_opt_cancels_same_stmt_retain_release_pairs() {
    let (program, sema) = lower_and_typecheck(
        r#"
enum Boxed { Wrap(Int) }
fn main() -> Int {
  let a = Wrap(1);
  let b = a;
  match b {
    Wrap(n) => n,
  }
}
"#,
    );
    let planned = plan(&program, &sema);
    let optimized = optimize(planned.clone());

    let (copy_stmt, source_var) = program
        .stmts()
        .iter()
        .enumerate()
        .find_map(|(idx, stmt)| {
            if let StmtKind::Let { value, .. } = &stmt.kind
                && let Some(ExprKind::Var(source)) = program.expr(*value).map(|expr| &expr.kind)
            {
                return Some((StmtId::new(idx), *source));
            }
            None
        })
        .expect("copy stmt");

    assert!(
        planned.ops.iter().any(|op| {
            op.stmt == copy_stmt
                && matches!(op.kind, ArcOpKind::Retain { var } if var == source_var)
        }),
        "copy-site planning should include retain before optimization"
    );
    assert!(
        planned.ops.iter().any(|op| {
            op.stmt == copy_stmt
                && matches!(op.kind, ArcOpKind::Release { var } if var == source_var)
        }),
        "copy-site planning should include terminal release before optimization"
    );

    assert!(
        !optimized.plan.ops.iter().any(|op| {
            op.stmt == copy_stmt
                && matches!(
                    op.kind,
                    ArcOpKind::Retain { var } | ArcOpKind::Release { var } if var == source_var
                )
        }),
        "same-statement retain/release pair should be eliminated as move-equivalent"
    );
    assert!(
        optimized.stats.eliminated_move_pairs >= 1
            && optimized.stats.removed_retain_ops >= 1
            && optimized.stats.removed_release_ops >= 1,
        "optimizer stats should reflect pair elimination"
    );
}

#[test]
fn arc_opt_keeps_unpaired_release_ops() {
    let (program, sema) = lower_and_typecheck(
        r#"
enum Boxed { Wrap(Int) }
fn main() -> Int {
  let x = Wrap(1);
  0
}
"#,
    );
    let planned = plan(&program, &sema);
    let optimized = optimize(planned.clone());

    let (def_stmt, x_var) = program
        .stmts()
        .iter()
        .enumerate()
        .find_map(|(idx, stmt)| {
            if let StmtKind::Let { binding, value, .. } = &stmt.kind
                && let Some(ExprKind::MakeEnum { .. }) = program.expr(*value).map(|expr| &expr.kind)
            {
                return Some((StmtId::new(idx), *binding));
            }
            None
        })
        .expect("x definition");

    assert!(
        planned.ops.iter().any(|op| {
            op.stmt == def_stmt && matches!(op.kind, ArcOpKind::Release { var } if var == x_var)
        }),
        "dead managed definition should plan a release"
    );
    assert!(
        optimized.plan.ops.iter().any(|op| {
            op.stmt == def_stmt && matches!(op.kind, ArcOpKind::Release { var } if var == x_var)
        }),
        "unpaired release must survive optimization"
    );
}
