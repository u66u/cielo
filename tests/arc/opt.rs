use cielo::common::ids::{ExprId, StmtId, VarId};
use cielo::ir::core::{ExprKind, StmtKind};
use cielo::passes::arc_insert::{ArcInsertionPlan, ArcOpKind, ArcPlannedOp, plan};
use cielo::passes::arc_opt::{optimize, optimize_with_cfg};

use crate::helpers::core::lower_and_typecheck;

#[test]
fn arc_opt_cancels_same_stmt_retain_release_pairs() {
    let (program, _sema) = lower_and_typecheck(
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

    let planned = ArcInsertionPlan {
        ops: vec![
            ArcPlannedOp {
                stmt: copy_stmt,
                kind: ArcOpKind::Retain { var: source_var },
            },
            ArcPlannedOp {
                stmt: copy_stmt,
                kind: ArcOpKind::Release { var: source_var },
            },
        ],
        ..Default::default()
    };
    let optimized = optimize_with_cfg(&program, planned.clone());

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

#[test]
fn arc_opt_keeps_call_boundary_retain_release_pairs() {
    let (program, sema) = lower_and_typecheck(
        r#"
enum Boxed { Wrap(Int) }
fn consume(a: Boxed, b: Boxed) -> Int {
  match a {
    Wrap(x) => match b {
      Wrap(y) => x + y,
    },
  }
}
fn main() -> Int {
  let x = Wrap(1);
  consume(x, x)
}
"#,
    );
    let planned = plan(&program, &sema);
    let optimized = optimize_with_cfg(&program, planned.clone());

    let (call_stmt, x_var) = program
        .stmts()
        .iter()
        .enumerate()
        .find_map(|(idx, _)| {
            let stmt_id = StmtId::new(idx);
            call_boundary_arg_var(&program, stmt_id).map(|var| (stmt_id, var))
        })
        .expect("managed duplicate-arg call");

    assert!(
        planned.ops.iter().any(|op| {
            op.stmt == call_stmt && matches!(op.kind, ArcOpKind::Retain { var } if var == x_var)
        }),
        "planned ops should retain managed call arg when used multiple times"
    );
    assert!(
        planned.ops.iter().any(|op| {
            op.stmt == call_stmt && matches!(op.kind, ArcOpKind::Release { var } if var == x_var)
        }),
        "planned ops should release managed call arg at terminal call use"
    );
    assert!(
        optimized.plan.ops.iter().any(|op| {
            op.stmt == call_stmt && matches!(op.kind, ArcOpKind::Retain { var } if var == x_var)
        }),
        "optimizer must not cancel call-boundary retain ops as move-equivalent"
    );
    assert!(
        optimized.plan.ops.iter().any(|op| {
            op.stmt == call_stmt && matches!(op.kind, ArcOpKind::Release { var } if var == x_var)
        }),
        "optimizer must not cancel call-boundary release ops as move-equivalent"
    );
}

fn call_boundary_arg_var(program: &cielo::ir::core::CoreProgram, stmt_id: StmtId) -> Option<VarId> {
    let stmt = program.stmt(stmt_id)?;
    if let StmtKind::Call { args, .. } = &stmt.kind
        && args.len() == 2
        && let Some(ExprKind::Var(var)) = program.expr(args[0]).map(|expr| &expr.kind)
    {
        return Some(*var);
    }
    for expr_id in stmt.child_exprs() {
        if let Some(var) = expr_call_boundary_arg_var(program, expr_id) {
            return Some(var);
        }
    }
    None
}

fn expr_call_boundary_arg_var(
    program: &cielo::ir::core::CoreProgram,
    expr_id: ExprId,
) -> Option<VarId> {
    let expr = program.expr(expr_id)?;
    match &expr.kind {
        ExprKind::PureCall { args, .. } => {
            if args.len() == 2
                && let Some(ExprKind::Var(var)) = program.expr(args[0]).map(|expr| &expr.kind)
            {
                return Some(*var);
            }
            for arg in args {
                if let Some(var) = expr_call_boundary_arg_var(program, *arg) {
                    return Some(var);
                }
            }
            None
        }
        ExprKind::Unary { expr, .. } => expr_call_boundary_arg_var(program, *expr),
        ExprKind::Binary { lhs, rhs, .. } => expr_call_boundary_arg_var(program, *lhs)
            .or_else(|| expr_call_boundary_arg_var(program, *rhs)),
        ExprKind::MakeStruct { fields, .. } | ExprKind::MakeEnum { fields, .. } => {
            for field in fields {
                if let Some(var) = expr_call_boundary_arg_var(program, *field) {
                    return Some(var);
                }
            }
            None
        }
        ExprKind::Var(_) | ExprKind::Literal(_) | ExprKind::Error(_) => None,
    }
}
