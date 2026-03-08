use cielo::analysis::arc_cfg::ArcCfg;
use cielo::analysis::arc_last_use::ArcLastUseTables;
use cielo::common::ids::{ExprId, StmtId, VarId};
use cielo::ir::core::{CoreProgram, ExprKind, StmtKind};
use cielo::passes::arc_insert::{ArcOpKind, plan};

use crate::helpers::core::lower_and_typecheck;

#[test]
fn arc_insert_plans_retain_for_managed_alias_copy() {
    let (program, sema) = lower_and_typecheck(
        r#"
enum Boxed { Wrap(Int) }
fn consume(v: Boxed) -> Int {
  match v {
    Wrap(n) => n,
  }
}
fn main() -> Int {
  let a = Wrap(1);
  let b = a;
  let c = a;
  let x = consume(b);
  let y = consume(c);
  x + y
}
"#,
    );
    let insertion = plan(&program, &sema);
    let cfg = ArcCfg::build(&program);
    let last_use = ArcLastUseTables::analyze(&cfg);

    let mut non_last_alias_site = None;
    for (idx, stmt) in program.stmts().iter().enumerate() {
        if let StmtKind::Let { value, .. } = &stmt.kind
            && let Some(ExprKind::Var(var)) = program.expr(*value).map(|expr| &expr.kind)
        {
            let stmt_id = StmtId::new(idx);
            if !last_use.last_uses(stmt_id).contains(var) {
                non_last_alias_site = Some((stmt_id, *var));
                break;
            }
        }
    }
    let (alias_stmt, source_var) = non_last_alias_site.expect("non-last alias stmt");
    assert!(
        insertion.ops.iter().any(|op| {
            op.stmt == alias_stmt
                && matches!(op.kind, ArcOpKind::Retain { var } if var == source_var)
        }),
        "non-last-use managed alias copies should plan a retain operation at the copy site"
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
fn main() -> Int {
  let b = Wrap(1);
  match b {
    Wrap(n) => n,
  }
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
        "unique managed last-use in local scope should produce a release plan"
    );
    assert!(
        insertion.stats.release_ops >= 1,
        "release stats should track planned release operations"
    );
}

#[test]
fn arc_insert_drops_dead_alias_binding_without_arc_churn() {
    let (program, sema) = lower_and_typecheck(
        r#"
enum Boxed { Wrap(Int) }
fn main() -> Int {
  let a = Wrap(1);
  let b = a;
  let c = b;
  0
}
"#,
    );
    let insertion = plan(&program, &sema);

    let dead_alias_binding = program
        .stmts()
        .iter()
        .enumerate()
        .find_map(|(idx, stmt)| {
            if let StmtKind::Let { binding, value, .. } = &stmt.kind
                && let Some(ExprKind::Var(source)) = program.expr(*value).map(|expr| &expr.kind)
            {
                let stmt_id = StmtId::new(idx);
                if program
                    .stmts()
                    .iter()
                    .enumerate()
                    .skip(idx + 1)
                    .all(|(_, later)| !later.child_exprs().iter().copied().any(|expr_id| {
                        matches!(program.expr(expr_id).map(|expr| &expr.kind), Some(ExprKind::Var(var)) if var == binding)
                    }))
                {
                    return Some((stmt_id, *binding, *source));
                }
            }
            None
        })
        .expect("dead alias binding");
    assert!(
        !insertion.ops.iter().any(|op| {
            op.stmt == dead_alias_binding.0
                && matches!(op.kind, ArcOpKind::Retain { var } if var == dead_alias_binding.2)
        }),
        "dead alias binding should not retain the copied source at its definition site"
    );
    assert!(
        !insertion.ops.iter().any(|op| {
            op.stmt == dead_alias_binding.0
                && matches!(op.kind, ArcOpKind::Release { var } if var == dead_alias_binding.1)
        }),
        "dead alias binding should not emit a release for the dead destination binding"
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

#[test]
fn arc_insert_moves_last_use_call_arg_into_callee() {
    let (program, sema) = lower_and_typecheck(
        r#"
enum Boxed { Wrap(Int) }
fn consume(v: Boxed) -> Int {
  match v {
    Wrap(n) => n,
  }
}
fn main() -> Int {
  let x = Wrap(1);
  consume(x)
}
"#,
    );
    let insertion = plan(&program, &sema);

    let main_x = program
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
        .expect("main x binding");
    let main_call_stmt = program
        .stmts()
        .iter()
        .enumerate()
        .find_map(|(idx, _)| {
            let stmt_id = StmtId::new(idx);
            stmt_contains_pure_call_arg_var(&program, stmt_id, main_x).then_some(stmt_id)
        })
        .expect("main call stmt");
    assert!(
        !insertion.ops.iter().any(|op| {
            op.stmt == main_call_stmt
                && matches!(
                    op.kind,
                    ArcOpKind::Retain { var } | ArcOpKind::Release { var } if var == main_x
                )
        }),
        "last-use managed call arg should transfer ownership to callee without caller retain/release pair"
    );

    let consume_param = program
        .functions()
        .iter()
        .find_map(|function| (!function.params.is_empty()).then_some(function.params[0]))
        .expect("consume param");
    assert!(
        insertion
            .ops
            .iter()
            .any(|op| matches!(op.kind, ArcOpKind::Release { var } if var == consume_param)),
        "callee should release transferred managed parameter"
    );
}

#[test]
fn arc_insert_moves_last_use_alias_copy_without_extra_arc_ops() {
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
    let insertion = plan(&program, &sema);

    let (alias_stmt, source_var) = program
        .stmts()
        .iter()
        .enumerate()
        .find_map(|(idx, stmt)| {
            if let StmtKind::Let { value, .. } = &stmt.kind
                && let Some(ExprKind::Var(var)) = program.expr(*value).map(|expr| &expr.kind)
            {
                return Some((StmtId::new(idx), *var));
            }
            None
        })
        .expect("alias stmt");
    assert!(
        !insertion.ops.iter().any(|op| {
            op.stmt == alias_stmt
                && matches!(
                    op.kind,
                    ArcOpKind::Retain { var } | ArcOpKind::Release { var } if var == source_var
                )
        }),
        "last-use managed alias copy should transfer ownership without local retain/release churn"
    );
}

#[test]
fn arc_insert_retains_non_last_use_call_arg() {
    let (program, sema) = lower_and_typecheck(
        r#"
enum Boxed { Wrap(Int) }
fn consume(v: Boxed) -> Int {
  match v {
    Wrap(n) => n,
  }
}
fn main() -> Int {
  let x = Wrap(1);
  let a = consume(x);
  let b = consume(x);
  a + b
}
"#,
    );
    let insertion = plan(&program, &sema);

    let x_var = program
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
        .expect("x binding");
    let call_stmts = program
        .stmts()
        .iter()
        .enumerate()
        .filter_map(|(idx, _)| {
            let stmt_id = StmtId::new(idx);
            stmt_contains_pure_call_arg_var(&program, stmt_id, x_var).then_some(stmt_id)
        })
        .collect::<Vec<_>>();
    assert!(
        call_stmts.iter().any(|stmt_id| {
            insertion.ops.iter().any(|op| {
                op.stmt == *stmt_id && matches!(op.kind, ArcOpKind::Retain { var } if var == x_var)
            }) && !insertion.ops.iter().any(|op| {
                op.stmt == *stmt_id && matches!(op.kind, ArcOpKind::Release { var } if var == x_var)
            })
        }),
        "non-last-use managed call arg should be retained at call boundary"
    );
}

#[test]
fn arc_insert_ignores_trivial_call_arg_arc_ops() {
    let (program, sema) = lower_and_typecheck(
        r#"
fn id(v: Int) -> Int {
  v
}
fn main() -> Int {
  let x = 1;
  id(x)
}
"#,
    );
    let insertion = plan(&program, &sema);

    let x_var = program
        .stmts()
        .iter()
        .find_map(|stmt| {
            if let StmtKind::Let { binding, value, .. } = &stmt.kind
                && let Some(ExprKind::Literal(cielo::ir::core::Literal::Int(1))) =
                    program.expr(*value).map(|expr| &expr.kind)
            {
                return Some(*binding);
            }
            None
        })
        .expect("main x binding");
    assert!(
        !insertion.ops.iter().any(|op| {
            matches!(
                op.kind,
                ArcOpKind::Retain { var } | ArcOpKind::Release { var } if var == x_var
            )
        }),
        "trivial call arguments must never receive ARC retain/release planning"
    );
}

#[test]
fn arc_insert_tracks_only_managed_call_args_in_mixed_signature_calls() {
    let (program, sema) = lower_and_typecheck(
        r#"
enum Boxed { Wrap(Int) }
fn mix(v: Boxed, n: Int) -> Int {
  match v {
    Wrap(x) => x + n,
  }
}
fn main() -> Int {
  let b = Wrap(1);
  let n = 2;
  let a = mix(b, n);
  let c = mix(b, n);
  a + c
}
"#,
    );
    let insertion = plan(&program, &sema);

    let managed_var = program
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
        .expect("managed var b");
    let trivial_var = program
        .stmts()
        .iter()
        .find_map(|stmt| {
            if let StmtKind::Let { binding, value, .. } = &stmt.kind
                && let Some(ExprKind::Literal(cielo::ir::core::Literal::Int(2))) =
                    program.expr(*value).map(|expr| &expr.kind)
            {
                return Some(*binding);
            }
            None
        })
        .expect("trivial var n");
    assert!(
        insertion.ops.iter().any(|op| {
            matches!(
                op.kind,
                ArcOpKind::Retain { var } | ArcOpKind::Release { var } if var == managed_var
            )
        }),
        "mixed call sites should still plan ARC ops for the managed argument"
    );
    assert!(
        !insertion.ops.iter().any(|op| {
            matches!(
                op.kind,
                ArcOpKind::Retain { var } | ArcOpKind::Release { var } if var == trivial_var
            )
        }),
        "mixed call sites must not plan ARC ops for trivial arguments"
    );
}

fn stmt_contains_pure_call_arg_var(program: &CoreProgram, stmt_id: StmtId, target: VarId) -> bool {
    let Some(stmt) = program.stmt(stmt_id) else {
        return false;
    };
    if let StmtKind::Call { args, .. } = &stmt.kind {
        for arg in args {
            if let Some(ExprKind::Var(var)) = program.expr(*arg).map(|expr| &expr.kind)
                && *var == target
            {
                return true;
            }
        }
    }
    for expr_id in stmt.child_exprs() {
        if expr_contains_pure_call_arg_var(program, expr_id, target) {
            return true;
        }
    }
    false
}

fn expr_contains_pure_call_arg_var(program: &CoreProgram, expr_id: ExprId, target: VarId) -> bool {
    let Some(expr) = program.expr(expr_id) else {
        return false;
    };
    match &expr.kind {
        ExprKind::PureCall { args, .. } => {
            for arg in args {
                if let Some(ExprKind::Var(var)) = program.expr(*arg).map(|expr| &expr.kind)
                    && *var == target
                {
                    return true;
                }
                if expr_contains_pure_call_arg_var(program, *arg, target) {
                    return true;
                }
            }
            false
        }
        ExprKind::Unary { expr, .. } => expr_contains_pure_call_arg_var(program, *expr, target),
        ExprKind::Binary { lhs, rhs, .. } => {
            expr_contains_pure_call_arg_var(program, *lhs, target)
                || expr_contains_pure_call_arg_var(program, *rhs, target)
        }
        ExprKind::MakeStruct { fields, .. } | ExprKind::MakeEnum { fields, .. } => fields
            .iter()
            .copied()
            .any(|field| expr_contains_pure_call_arg_var(program, field, target)),
        ExprKind::Var(_) | ExprKind::Literal(_) | ExprKind::Error(_) => false,
    }
}
