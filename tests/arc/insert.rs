use cielo::common::ids::{StmtId, VarId};
use cielo::ir::core::{CoreProgram, ExprKind, StmtKind};
use cielo::passes::arc_insert::{ArcOpKind, plan};

use crate::helpers::core::lower_and_typecheck;

#[test]
fn arc_insert_plans_retain_for_managed_alias_copy() {
    let (program, sema) = lower_and_typecheck(
        r#"
enum Boxed { Wrap(Int) }
fn main() -> Int {
  let a = Wrap(1);
  let b = a;
  0
}
"#,
    );
    let insertion = plan(&program, &sema);

    let mut alias_stmt = None;
    let mut source_var = None;
    for (idx, stmt) in program.stmts().iter().enumerate() {
        if let StmtKind::Let { value, .. } = &stmt.kind
            && let Some(ExprKind::Var(var)) = program.expr(*value).map(|expr| &expr.kind)
        {
            alias_stmt = Some(StmtId::new(idx));
            source_var = Some(*var);
        }
    }
    let alias_stmt = alias_stmt.expect("alias let stmt");
    let source_var = source_var.expect("alias source var");
    assert!(
        insertion.ops.iter().any(|op| {
            op.stmt == alias_stmt
                && matches!(op.kind, ArcOpKind::Retain { var } if var == source_var)
        }),
        "managed alias copies should plan a retain operation at the copy site"
    );
    assert!(
        insertion.stats.retain_ops >= 1,
        "retain stats should track planned retain operations"
    );
}

#[test]
fn arc_insert_plans_release_for_unique_managed_last_use() {
    let (program, sema) = lower_and_typecheck(
        r#"
enum Boxed { Wrap(Int) }
fn consume(v: Boxed) -> Int {
  match v {
    Wrap(n) => n,
  }
}
fn main() -> Int {
  let b = Wrap(1);
  consume(b)
}
"#,
    );
    let insertion = plan(&program, &sema);

    let b_var = program
        .stmts()
        .iter()
        .find_map(|stmt| {
            if let StmtKind::Let { binding, value, .. } = &stmt.kind
                && let Some(ExprKind::MakeEnum { .. }) = program.expr(*value).map(|expr| &expr.kind)
            {
                return Some(*binding);
            }
            None
        })
        .expect("managed binding b");

    assert!(
        insertion
            .ops
            .iter()
            .any(|op| matches!(op.kind, ArcOpKind::Release { var } if var == b_var)),
        "unique managed last-use should produce a release plan"
    );
    assert!(
        insertion.stats.release_ops >= 1,
        "release stats should track planned release operations"
    );
}

#[test]
fn arc_insert_releases_shared_alias_roots_when_they_go_dead() {
    let (program, sema) = lower_and_typecheck(
        r#"
enum Boxed { Wrap(Int) }
fn main() -> Int {
  let a = Wrap(1);
  let b = a;
  let c = a;
  0
}
"#,
    );
    let insertion = plan(&program, &sema);

    let a_var = root_binding_var(&program).expect("a binding");
    assert!(
        insertion
            .ops
            .iter()
            .any(|op| matches!(op.kind, ArcOpKind::Release { var } if var == a_var)),
        "shared alias roots should still be released when their source binding reaches terminal use"
    );
}

#[test]
fn arc_insert_releases_dead_managed_defs_without_reads() {
    let (program, sema) = lower_and_typecheck(
        r#"
enum Boxed { Wrap(Int) }
fn main() -> Int {
  let x = Wrap(1);
  0
}
"#,
    );
    let insertion = plan(&program, &sema);

    let (x_var, x_stmt) = program
        .stmts()
        .iter()
        .enumerate()
        .find_map(|(idx, stmt)| {
            if let StmtKind::Let { binding, value, .. } = &stmt.kind
                && let Some(ExprKind::MakeEnum { .. }) = program.expr(*value).map(|expr| &expr.kind)
            {
                return Some((*binding, StmtId::new(idx)));
            }
            None
        })
        .expect("x binding");
    assert!(
        insertion
            .ops
            .iter()
            .any(|op| op.stmt == x_stmt
                && matches!(op.kind, ArcOpKind::Release { var } if var == x_var)),
        "managed defs that never become live should still release at their defining statement"
    );
}

fn root_binding_var(program: &CoreProgram) -> Option<VarId> {
    program.stmts().iter().find_map(|stmt| {
        if let StmtKind::Let { binding, value, .. } = &stmt.kind
            && let Some(ExprKind::MakeEnum { .. } | ExprKind::MakeStruct { .. }) =
                program.expr(*value).map(|expr| &expr.kind)
        {
            return Some(*binding);
        }
        None
    })
}
