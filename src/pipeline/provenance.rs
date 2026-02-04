use std::collections::{HashMap, HashSet};

use crate::common::densemap::DenseId;
use crate::common::ids::{ExprId, FuncId, StmtId, VarId};
use crate::ir::core::{CoreProgram, ExprKind, StmtKind};
use crate::pipeline::phases::{BtaTables, Reason, Stage};

pub fn runtime_provenance_lines(
    program: &CoreProgram,
    bta: &BtaTables,
    root: ExprId,
    limit: usize,
) -> Vec<String> {
    let var_sources = VarSources::build(program);
    let mut seen_exprs = HashSet::new();
    let mut seen_vars = HashSet::new();
    let mut lines = Vec::new();
    let mut cursor = Cursor::Expr(root);

    for step in 0..limit.max(1) {
        match cursor {
            Cursor::Expr(expr_id) => {
                if !seen_exprs.insert(expr_id) {
                    break;
                }
                let Some(stage) = bta.stage_of_expr.get(&expr_id).copied() else {
                    break;
                };
                let Stage::Rt(reason) = stage else {
                    break;
                };
                lines.push(format!(
                    "{:>2}. e{}: {}",
                    step + 1,
                    expr_id.as_u32(),
                    reason_text(reason)
                ));
                cursor = match next_from_reason(reason, program, bta, Some(expr_id)) {
                    Some(next) => next,
                    None => break,
                };
            }
            Cursor::Var(var_id) => {
                if !seen_vars.insert(var_id) {
                    break;
                }
                if let Some(Stage::Rt(reason)) = bta.stage_of_var.get(&var_id).copied() {
                    lines.push(format!(
                        "{:>2}. v{}: {}",
                        step + 1,
                        var_id.as_u32(),
                        reason_text(reason)
                    ));
                    cursor = match next_from_reason(reason, program, bta, None) {
                        Some(next) => next,
                        None => break,
                    };
                    continue;
                }

                let Some(source) = var_sources.0.get(&var_id).copied() else {
                    break;
                };
                match source {
                    VarSource::Param { func, param_index } => {
                        lines.push(format!(
                            "{:>2}. v{}: parameter #{} of f{} is runtime",
                            step + 1,
                            var_id.as_u32(),
                            param_index + 1,
                            func.as_u32()
                        ));
                        break;
                    }
                    VarSource::Expr(expr_id) => {
                        cursor = Cursor::Expr(expr_id);
                    }
                    VarSource::Stmt(stmt_id) => {
                        let Some(expr_id) = first_runtime_expr_in_stmt(program, bta, stmt_id)
                        else {
                            break;
                        };
                        cursor = Cursor::Expr(expr_id);
                    }
                }
            }
        }
    }

    lines
}

fn next_from_reason(
    reason: Reason,
    program: &CoreProgram,
    bta: &BtaTables,
    current_expr: Option<ExprId>,
) -> Option<Cursor> {
    match reason {
        Reason::DependsOnVar(var) => Some(Cursor::Var(var)),
        Reason::BranchOnRuntime(expr) => Some(Cursor::Expr(expr)),
        Reason::UnclassifiedRuntime => current_expr
            .and_then(|expr_id| infer_runtime_dependency_from_expr(program, bta, expr_id)),
        Reason::Parameter { .. }
        | Reason::EffectNotDischarged(_)
        | Reason::HandlerIsRuntime(_)
        | Reason::NotPersistable(_)
        | Reason::UserForcedRuntime
        | Reason::CtOnlyWithRuntimeArgs(_) => None,
    }
}

fn infer_runtime_dependency_from_expr(
    program: &CoreProgram,
    bta: &BtaTables,
    expr_id: ExprId,
) -> Option<Cursor> {
    let expr = program.expr(expr_id)?;
    match &expr.kind {
        ExprKind::Var(var) => Some(Cursor::Var(*var)),
        ExprKind::Unary { expr, .. } => is_runtime_expr(*expr, bta).then_some(Cursor::Expr(*expr)),
        ExprKind::Binary { lhs, rhs, .. } => {
            if is_runtime_expr(*lhs, bta) {
                Some(Cursor::Expr(*lhs))
            } else if is_runtime_expr(*rhs, bta) {
                Some(Cursor::Expr(*rhs))
            } else {
                None
            }
        }
        ExprKind::PureCall { args, .. } => args
            .iter()
            .find(|arg| is_runtime_expr(**arg, bta))
            .copied()
            .map(Cursor::Expr),
        ExprKind::MakeStruct { fields, .. } | ExprKind::MakeEnum { fields, .. } => fields
            .iter()
            .find(|field| is_runtime_expr(**field, bta))
            .copied()
            .map(Cursor::Expr),
        ExprKind::Literal(_) | ExprKind::Error(_) => None,
    }
}

fn is_runtime_expr(expr_id: ExprId, bta: &BtaTables) -> bool {
    matches!(bta.stage_of_expr.get(&expr_id), Some(Stage::Rt(_)))
}

fn first_runtime_expr_in_stmt(
    program: &CoreProgram,
    bta: &BtaTables,
    root: StmtId,
) -> Option<ExprId> {
    let mut stack = vec![root];
    let mut seen = HashSet::new();

    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };

        match &stmt.kind {
            StmtKind::Return(expr) => {
                if is_runtime_expr(*expr, bta) {
                    return Some(*expr);
                }
            }
            StmtKind::Let { value, next, .. } => {
                if is_runtime_expr(*value, bta) {
                    return Some(*value);
                }
                stack.push(*next);
            }
            StmtKind::Val { value, next, .. } => {
                stack.push(*next);
                stack.push(*value);
            }
            StmtKind::Call { args, next, .. } | StmtKind::Perform { args, next, .. } => {
                if let Some(arg) = args.iter().find(|arg| is_runtime_expr(**arg, bta)).copied() {
                    return Some(arg);
                }
                stack.push(*next);
            }
            StmtKind::Resume { arg, next, .. } => {
                if is_runtime_expr(*arg, bta) {
                    return Some(*arg);
                }
                stack.push(*next);
            }
            StmtKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
                if is_runtime_expr(*cond, bta) {
                    return Some(*cond);
                }
                stack.push(*else_branch);
                stack.push(*then_branch);
            }
            StmtKind::Match {
                scrutinee,
                arms,
                default,
            } => {
                if is_runtime_expr(*scrutinee, bta) {
                    return Some(*scrutinee);
                }
                if let Some(default_stmt) = default {
                    stack.push(*default_stmt);
                }
                for arm in arms {
                    stack.push(arm.body);
                }
            }
            StmtKind::Handle { body, next, .. } | StmtKind::Stage { body, next, .. } => {
                if let Some(next_stmt) = next {
                    stack.push(*next_stmt);
                }
                stack.push(*body);
            }
            StmtKind::Hole { .. } | StmtKind::Error(_) => {}
        }
    }

    None
}

fn reason_text(reason: Reason) -> String {
    match reason {
        Reason::UnclassifiedRuntime => "runtime classification has not been refined yet".to_owned(),
        Reason::Parameter { func, index } => {
            format!("parameter #{} of f{} is runtime", index + 1, func.as_u32())
        }
        Reason::DependsOnVar(var) => format!("depends on v{} which is runtime", var.as_u32()),
        Reason::EffectNotDischarged(effect) => {
            format!("effect e{} is not thunkable/discharged", effect.as_u32())
        }
        Reason::HandlerIsRuntime(handler) => format!("handler h{} is runtime", handler.as_u32()),
        Reason::BranchOnRuntime(expr) => {
            format!("branch condition e{} is runtime", expr.as_u32())
        }
        Reason::NotPersistable(ty) => format!("type t{} is not persistable", ty.as_u32()),
        Reason::UserForcedRuntime => "explicitly marked @runtime".to_owned(),
        Reason::CtOnlyWithRuntimeArgs(func) => {
            format!(
                "ct-only function f{} was called with runtime args",
                func.as_u32()
            )
        }
    }
}

struct VarSources(HashMap<VarId, VarSource>);

impl VarSources {
    fn build(program: &CoreProgram) -> Self {
        let mut map = HashMap::new();

        for (func_idx, function) in program.functions().iter().enumerate() {
            let func_id = FuncId::new(func_idx);
            for (param_idx, var) in function.params.iter().copied().enumerate() {
                map.entry(var).or_insert(VarSource::Param {
                    func: func_id,
                    param_index: param_idx as u16,
                });
            }
        }

        for (stmt_idx, stmt) in program.stmts().iter().enumerate() {
            match &stmt.kind {
                StmtKind::Let { binding, value, .. } => {
                    map.insert(*binding, VarSource::Expr(*value));
                }
                StmtKind::Val { binding, value, .. } => {
                    map.insert(*binding, VarSource::Stmt(*value));
                }
                StmtKind::Call { result, .. } => {
                    map.insert(*result, VarSource::Stmt(StmtId::new(stmt_idx)));
                }
                StmtKind::Perform {
                    result: Some(result),
                    ..
                } => {
                    map.insert(*result, VarSource::Stmt(StmtId::new(stmt_idx)));
                }
                _ => {}
            }
        }

        Self(map)
    }
}

#[derive(Clone, Copy)]
enum VarSource {
    Param { func: FuncId, param_index: u16 },
    Expr(ExprId),
    Stmt(StmtId),
}

enum Cursor {
    Expr(ExprId),
    Var(VarId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootCauseTaint {
    pub terminal_reason: Reason,
    pub description: String,
    pub taint_count: usize,
}

pub fn staging_root_causes(program: &CoreProgram, bta: &BtaTables) -> Vec<RootCauseTaint> {
    let mut counts: HashMap<String, (Reason, usize)> = HashMap::new();
    let var_sources = VarSources::build(program);

    for (expr_idx, stage) in bta.stage_of_expr.iter() {
        if let Stage::Rt(_) = stage {
            let expr_id = ExprId::from_index(expr_idx.index());
            let terminal = find_terminal_reason(expr_id, program, bta, &var_sources);
            let desc = reason_text(terminal);

            let entry = counts.entry(desc).or_insert((terminal, 0));
            entry.1 += 1;
        }
    }

    let mut rollups: Vec<RootCauseTaint> = counts
        .into_iter()
        .map(
            |(description, (terminal_reason, taint_count))| RootCauseTaint {
                terminal_reason,
                description,
                taint_count,
            },
        )
        .collect();

    // sort:must be detemernistic. highest taint first, then alphabetically by description
    rollups.sort_by(|a, b| {
        b.taint_count
            .cmp(&a.taint_count)
            .then(a.description.cmp(&b.description))
    });

    rollups
}

fn find_terminal_reason(
    start_expr: ExprId,
    program: &CoreProgram,
    bta: &BtaTables,
    var_sources: &VarSources,
) -> Reason {
    let mut seen_exprs = HashSet::new();
    let mut seen_vars = HashSet::new();
    let mut cursor = Cursor::Expr(start_expr);
    let mut last_reason = Reason::UnclassifiedRuntime;

    for _ in 0..100 {
        // bounded depth to prevent infinite loops on broken IR (do we need this?)
        match cursor {
            Cursor::Expr(expr_id) => {
                if !seen_exprs.insert(expr_id) {
                    break;
                }
                let Some(Stage::Rt(reason)) = bta.stage_of_expr.get(&expr_id).copied() else {
                    break;
                };
                last_reason = reason;

                if let Some(next) = next_from_reason(reason, program, bta, Some(expr_id)) {
                    cursor = next;
                } else {
                    break;
                }
            }
            Cursor::Var(var_id) => {
                if !seen_vars.insert(var_id) {
                    break;
                }
                if let Some(Stage::Rt(reason)) = bta.stage_of_var.get(&var_id).copied() {
                    last_reason = reason;
                    if let Some(next) = next_from_reason(reason, program, bta, None) {
                        cursor = next;
                        continue;
                    } else {
                        break;
                    }
                }

                // variable isn't explicitly staged -> trace back to its definition
                if let Some(source) = var_sources.0.get(&var_id).copied() {
                    match source {
                        VarSource::Param { func, param_index } => {
                            last_reason = Reason::Parameter {
                                func,
                                index: param_index,
                            };
                            break;
                        }
                        VarSource::Expr(e) => cursor = Cursor::Expr(e),
                        VarSource::Stmt(s) => {
                            if let Some(e) = first_runtime_expr_in_stmt(program, bta, s) {
                                cursor = Cursor::Expr(e);
                            } else {
                                break;
                            }
                        }
                    }
                } else {
                    break;
                }
            }
        }
    }
    last_reason
}
