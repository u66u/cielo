use std::collections::{HashMap, HashSet};

use crate::common::ids::{LinearExprId, LinearStmtId, VarId};
use crate::ir::linear::{LinearExpr, LinearProgram, LinearStmt};
use crate::pipeline::phases::SemanticTables;
use crate::sema::ownership::OwnershipClass;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ArcEmitStats {
    pub planned_retain_ops: u32,
    pub planned_release_ops: u32,
    pub eliminated_move_pairs: u32,
    pub optimized_retain_ops: u32,
    pub optimized_release_ops: u32,
}

#[derive(Clone, Debug, Default)]
pub struct ArcEmitPlan {
    pre_retain_by_stmt: HashMap<LinearStmtId, Vec<VarId>>,
    post_release_by_stmt: HashMap<LinearStmtId, Vec<VarId>>,
    pub stats: ArcEmitStats,
}

impl ArcEmitPlan {
    pub fn disabled() -> Self {
        Self::default()
    }

    pub fn build(program: &LinearProgram, sema: &SemanticTables) -> Self {
        let reachable = collect_reachable_stmts(program);
        if reachable.is_empty() {
            return Self::default();
        }

        let mut summaries = vec![None; program.stmts().len()];
        for stmt_id in reachable.iter().copied() {
            summaries[stmt_id.index()] = Some(summarize_stmt(program, stmt_id));
        }

        let mut live_in = vec![HashSet::new(); program.stmts().len()];
        let mut live_out = vec![HashSet::new(); program.stmts().len()];
        let mut changed = true;
        while changed {
            changed = false;
            for stmt_id in reachable.iter().copied().rev() {
                let Some(summary) = summaries.get(stmt_id.index()).and_then(Option::as_ref) else {
                    continue;
                };
                let mut out = HashSet::new();
                for succ in &summary.successors {
                    out.extend(live_in[succ.index()].iter().copied());
                }
                let mut next_in = out.clone();
                for def in &summary.defs {
                    next_in.remove(def);
                }
                next_in.extend(summary.uses.iter().copied());
                if out != live_out[stmt_id.index()] {
                    live_out[stmt_id.index()] = out;
                    changed = true;
                }
                if next_in != live_in[stmt_id.index()] {
                    live_in[stmt_id.index()] = next_in;
                    changed = true;
                }
            }
        }

        let mut plan = ArcEmitPlan::default();
        for stmt_id in reachable.iter().copied() {
            let Some(stmt) = program.stmt(stmt_id) else {
                continue;
            };
            if let LinearStmt::Let { binding, value, .. } = &stmt.kind
                && let Some(expr) = program.expr(*value)
                && let LinearExpr::Var(source) = expr.kind
                && (is_managed_var(sema, source) || is_managed_var(sema, *binding))
            {
                push_unique(
                    plan.pre_retain_by_stmt.entry(stmt_id).or_default(),
                    source,
                    &mut plan.stats.planned_retain_ops,
                );
            }

            let Some(summary) = summaries.get(stmt_id.index()).and_then(Option::as_ref) else {
                continue;
            };
            for used in &summary.uses {
                if !live_out[stmt_id.index()].contains(used) && is_managed_var(sema, *used) {
                    push_unique(
                        plan.post_release_by_stmt.entry(stmt_id).or_default(),
                        *used,
                        &mut plan.stats.planned_release_ops,
                    );
                }
            }
            for def in &summary.defs {
                if !live_out[stmt_id.index()].contains(def) && is_managed_var(sema, *def) {
                    push_unique(
                        plan.post_release_by_stmt.entry(stmt_id).or_default(),
                        *def,
                        &mut plan.stats.planned_release_ops,
                    );
                }
            }
        }

        for stmt_id in reachable.iter().copied() {
            let Some(retains) = plan.pre_retain_by_stmt.get(&stmt_id).cloned() else {
                continue;
            };
            let Some(releases) = plan.post_release_by_stmt.get(&stmt_id).cloned() else {
                continue;
            };
            let retain_set = retains.into_iter().collect::<HashSet<_>>();
            let release_set = releases.into_iter().collect::<HashSet<_>>();
            let cancelled = retain_set
                .intersection(&release_set)
                .copied()
                .collect::<Vec<_>>();
            if cancelled.is_empty() {
                continue;
            }
            plan.stats.eliminated_move_pairs = plan
                .stats
                .eliminated_move_pairs
                .saturating_add(cancelled.len() as u32);
            if let Some(retains) = plan.pre_retain_by_stmt.get_mut(&stmt_id) {
                retains.retain(|var| !cancelled.contains(var));
            }
            if let Some(releases) = plan.post_release_by_stmt.get_mut(&stmt_id) {
                releases.retain(|var| !cancelled.contains(var));
            }
        }

        for vars in plan.pre_retain_by_stmt.values_mut() {
            vars.sort_by_key(|var| var.index());
            vars.dedup();
            plan.stats.optimized_retain_ops = plan
                .stats
                .optimized_retain_ops
                .saturating_add(vars.len() as u32);
        }
        for vars in plan.post_release_by_stmt.values_mut() {
            vars.sort_by_key(|var| var.index());
            vars.dedup();
            plan.stats.optimized_release_ops = plan
                .stats
                .optimized_release_ops
                .saturating_add(vars.len() as u32);
        }

        plan
    }

    pub fn pre_retain_vars(&self, stmt_id: LinearStmtId) -> &[VarId] {
        self.pre_retain_by_stmt
            .get(&stmt_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn post_release_vars(&self, stmt_id: LinearStmtId) -> &[VarId] {
        self.post_release_by_stmt
            .get(&stmt_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

#[derive(Clone, Debug, Default)]
struct LinearStmtSummary {
    uses: Vec<VarId>,
    defs: Vec<VarId>,
    successors: Vec<LinearStmtId>,
}

fn summarize_stmt(program: &LinearProgram, stmt_id: LinearStmtId) -> LinearStmtSummary {
    let mut summary = LinearStmtSummary::default();
    let Some(stmt) = program.stmt(stmt_id) else {
        return summary;
    };

    let mut seen_exprs = HashSet::new();
    for expr_id in stmt.child_exprs() {
        collect_expr_uses(program, expr_id, &mut seen_exprs, &mut summary.uses);
    }
    summary.successors.extend(stmt.child_stmts());

    match &stmt.kind {
        LinearStmt::Let { binding, .. } | LinearStmt::Val { binding, .. } => {
            push_unique_no_stats(&mut summary.defs, *binding);
        }
        LinearStmt::PureCall { result, .. }
        | LinearStmt::DirectCall { result, .. }
        | LinearStmt::ControlCall { result, .. } => {
            push_unique_no_stats(&mut summary.defs, *result);
        }
        LinearStmt::Perform {
            result: Some(result),
            ..
        } => push_unique_no_stats(&mut summary.defs, *result),
        LinearStmt::Perform { result: None, .. } => {}
        LinearStmt::Match { arms, .. } => {
            for arm in arms {
                for binder in arm.binders.iter().copied() {
                    push_unique_no_stats(&mut summary.defs, binder);
                }
            }
        }
        LinearStmt::Return(_)
        | LinearStmt::If { .. }
        | LinearStmt::Handle { .. }
        | LinearStmt::Stage { .. }
        | LinearStmt::Hole
        | LinearStmt::Error => {}
    }

    summary.uses.sort_by_key(|var| var.index());
    summary.uses.dedup();
    summary.defs.sort_by_key(|var| var.index());
    summary.defs.dedup();
    summary
}

fn collect_expr_uses(
    program: &LinearProgram,
    expr_id: LinearExprId,
    seen_exprs: &mut HashSet<LinearExprId>,
    out: &mut Vec<VarId>,
) {
    if !seen_exprs.insert(expr_id) {
        return;
    }
    let Some(expr) = program.expr(expr_id) else {
        return;
    };
    match &expr.kind {
        LinearExpr::Var(var) => push_unique_no_stats(out, *var),
        LinearExpr::Unary { expr, .. } => collect_expr_uses(program, *expr, seen_exprs, out),
        LinearExpr::Binary { lhs, rhs, .. } => {
            collect_expr_uses(program, *lhs, seen_exprs, out);
            collect_expr_uses(program, *rhs, seen_exprs, out);
        }
        LinearExpr::PureCall { args, .. }
        | LinearExpr::MakeStruct { fields: args, .. }
        | LinearExpr::MakeEnum { fields: args, .. } => {
            for arg in args {
                collect_expr_uses(program, *arg, seen_exprs, out);
            }
        }
        LinearExpr::Literal(_) | LinearExpr::Error => {}
    }
}

fn collect_reachable_stmts(program: &LinearProgram) -> Vec<LinearStmtId> {
    let roots = program
        .functions
        .iter()
        .map(|function| function.body)
        .collect::<Vec<_>>();
    let mut stack = roots;
    let mut seen = HashSet::new();
    let mut reachable = Vec::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        reachable.push(stmt_id);
        if let Some(stmt) = program.stmt(stmt_id) {
            stack.extend(stmt.child_stmts());
        }
    }
    reachable.sort_by_key(|id| id.index());
    reachable
}

fn is_managed_var(sema: &SemanticTables, var: VarId) -> bool {
    sema.ownership_of_var
        .get(&var)
        .copied()
        .unwrap_or(OwnershipClass::BorrowedView)
        == OwnershipClass::RcManaged
}

fn push_unique(out: &mut Vec<VarId>, var: VarId, counter: &mut u32) {
    if out.contains(&var) {
        return;
    }
    out.push(var);
    *counter = counter.saturating_add(1);
}

fn push_unique_no_stats(out: &mut Vec<VarId>, var: VarId) {
    if !out.contains(&var) {
        out.push(var);
    }
}
