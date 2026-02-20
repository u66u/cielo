use cielo::analysis::arc_alias::{ArcAliasClass, ArcAliasRelation, ArcAliasTables};
use cielo::analysis::arc_cfg::ArcCfg;
use cielo::analysis::arc_last_use::ArcLastUseTables;
use cielo::common::ids::{SourceId, StmtId};
use cielo::common::symbols::Interner;
use cielo::ir::core::{CoreProgram, ExprKind, Literal, StmtKind};
use cielo::{Compiler, CompilerConfig};

fn lower_to_core(source: &str) -> CoreProgram {
    let compiler = Compiler::new(CompilerConfig::default());
    let mut interner = Interner::new();
    let core = compiler.parse_and_lower_to_core(source, SourceId::from_u32(0), &mut interner);
    core.program().clone()
}

#[test]
fn arc_cfg_records_branch_successors_and_cond_uses() {
    let program = lower_to_core(
        r#"
fn main() -> Int {
  let x = 1;
  if x == 0 { 1 } else { 2 }
}
"#,
    );
    let cfg = ArcCfg::build(&program);

    let if_stmt = program
        .stmts()
        .iter()
        .enumerate()
        .find_map(|(idx, stmt)| {
            matches!(stmt.kind, StmtKind::If { .. }).then_some(StmtId::new(idx))
        })
        .expect("if stmt");
    let if_summary = cfg.summary(if_stmt).expect("if summary");
    assert_eq!(
        if_summary.successors.len(),
        2,
        "if statements should expose two control-flow successors"
    );

    let cond_var = match program.stmt(if_stmt).map(|stmt| &stmt.kind) {
        Some(StmtKind::If { cond, .. }) => match program.expr(*cond).map(|expr| &expr.kind) {
            Some(ExprKind::Binary { lhs, .. }) => match program.expr(*lhs).map(|expr| &expr.kind) {
                Some(ExprKind::Var(var)) => *var,
                other => panic!("expected lhs var in branch condition, got {other:?}"),
            },
            other => panic!("expected binary condition for if stmt, got {other:?}"),
        },
        other => panic!("expected if stmt, got {other:?}"),
    };
    assert!(
        if_summary.uses.contains(&cond_var),
        "if summary should include condition variable use"
    );
}

#[test]
fn arc_alias_marks_copy_aliases_shared() {
    let program = lower_to_core(
        r#"
fn main() -> Int {
  let x = 1;
  let y = x;
  let z = 2;
  y + z
}
"#,
    );
    let cfg = ArcCfg::build(&program);
    let alias = ArcAliasTables::analyze(&program, &cfg);

    let mut copied = None;
    let mut z_var = None;
    for stmt in program.stmts() {
        if let StmtKind::Let { binding, value, .. } = &stmt.kind {
            match program.expr(*value).map(|expr| &expr.kind) {
                Some(ExprKind::Var(source)) => copied = Some((*source, *binding)),
                Some(ExprKind::Literal(Literal::Int(2))) => z_var = Some(*binding),
                _ => {}
            }
        }
    }
    let (x, y) = copied.expect("copy alias x -> y");
    let z = z_var.expect("z var");

    assert_eq!(alias.class_of_var(x), Some(ArcAliasClass::Shared));
    assert_eq!(alias.class_of_var(y), Some(ArcAliasClass::Shared));
    assert_eq!(alias.relation(x, y), ArcAliasRelation::MayAlias);
    assert_eq!(alias.class_of_var(z), Some(ArcAliasClass::Unique));
}

#[test]
fn arc_last_use_marks_unique_terminal_use() {
    let program = lower_to_core(
        r#"
fn main() -> Int {
  let x = 1;
  let y = x + 2;
  y
}
"#,
    );
    let cfg = ArcCfg::build(&program);
    let alias = ArcAliasTables::analyze(&program, &cfg);
    let last_use = ArcLastUseTables::analyze(&cfg, &alias);

    let mut x_var = None;
    let mut y_let_stmt = None;
    for (idx, stmt) in program.stmts().iter().enumerate() {
        if let StmtKind::Let { binding, value, .. } = &stmt.kind {
            match program.expr(*value).map(|expr| &expr.kind) {
                Some(ExprKind::Literal(Literal::Int(1))) => x_var = Some(*binding),
                Some(ExprKind::Binary { lhs, .. }) => {
                    if let Some(ExprKind::Var(var)) = program.expr(*lhs).map(|expr| &expr.kind) {
                        x_var = Some(*var);
                        y_let_stmt = Some(StmtId::new(idx));
                    }
                }
                _ => {}
            }
        }
    }
    let x = x_var.expect("x var");
    let y_stmt = y_let_stmt.expect("let y stmt");
    assert!(
        last_use.last_uses(y_stmt).contains(&x),
        "unique use in let y should be marked as last use for x"
    );
}

#[test]
fn arc_last_use_skips_shared_alias_vars() {
    let program = lower_to_core(
        r#"
fn main() -> Int {
  let x = 1;
  let a = x;
  let b = x;
  b
}
"#,
    );
    let cfg = ArcCfg::build(&program);
    let alias = ArcAliasTables::analyze(&program, &cfg);
    let last_use = ArcLastUseTables::analyze(&cfg, &alias);

    let mut x_var = None;
    for stmt in program.stmts() {
        if let StmtKind::Let { binding, value, .. } = &stmt.kind
            && let Some(ExprKind::Literal(Literal::Int(1))) =
                program.expr(*value).map(|expr| &expr.kind)
        {
            x_var = Some(*binding);
        }
    }
    let x = x_var.expect("x var");
    assert_eq!(alias.class_of_var(x), Some(ArcAliasClass::Shared));
    assert!(
        cfg.reachable()
            .iter()
            .all(|stmt_id| !last_use.last_uses(*stmt_id).contains(&x)),
        "shared alias vars should never be considered last-use safe in v1 analysis"
    );
}
