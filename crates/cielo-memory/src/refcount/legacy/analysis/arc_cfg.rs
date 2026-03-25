use std::collections::HashSet;

use crate::analysis::function_graph::collect_reachable_functions;
use crate::common::ids::{ExprId, StmtId, VarId};
use crate::ir::core::{CoreProgram, ExprKind, StmtKind};
use smallvec::SmallVec;

#[derive(Clone, Debug, Default)]
pub struct ArcStmtSummary {
    pub uses: SmallVec<[VarId; 4]>,
    pub defs: SmallVec<[VarId; 2]>,
    pub successors: SmallVec<[StmtId; 4]>,
}

#[derive(Clone, Debug, Default)]
pub struct ArcCfg {
    summaries: Vec<Option<ArcStmtSummary>>,
    roots: Vec<StmtId>,
    reachable: Vec<StmtId>,
}

impl ArcCfg {
    pub fn build(program: &CoreProgram) -> Self {
        let roots = collect_reachable_functions(program)
            .into_iter()
            .filter_map(|func_id| program.function(func_id).map(|function| function.body))
            .collect::<Vec<_>>();
        let mut summaries = vec![None; program.stmts().len()];
        let mut stack = roots.clone();
        let mut seen = HashSet::new();
        let mut reachable = Vec::new();

        while let Some(stmt_id) = stack.pop() {
            if !seen.insert(stmt_id) {
                continue;
            }
            let Some(stmt) = program.stmt(stmt_id) else {
                continue;
            };
            let summary = summarize_stmt(program, stmt_id);
            stack.extend(summary.successors.iter().copied());
            summaries[stmt_id.index()] = Some(summary);
            reachable.push(stmt_id);
            stack.extend(stmt.child_stmts());
        }

        reachable.sort_by_key(|id| id.index());
        Self {
            summaries,
            roots,
            reachable,
        }
    }

    pub fn summary(&self, stmt_id: StmtId) -> Option<&ArcStmtSummary> {
        self.summaries.get(stmt_id.index()).and_then(Option::as_ref)
    }

    pub fn roots(&self) -> &[StmtId] {
        &self.roots
    }

    pub fn reachable(&self) -> &[StmtId] {
        &self.reachable
    }

    pub fn stmt_capacity(&self) -> usize {
        self.summaries.len()
    }
}

fn summarize_stmt(program: &CoreProgram, stmt_id: StmtId) -> ArcStmtSummary {
    let mut summary = ArcStmtSummary::default();
    let Some(stmt) = program.stmt(stmt_id) else {
        return summary;
    };

    let mut seen_exprs = HashSet::new();
    for expr_id in stmt.child_exprs() {
        collect_expr_uses(program, expr_id, &mut seen_exprs, &mut summary.uses);
    }

    summary.successors.extend(stmt.child_stmts());
    dedup_vars(&mut summary.uses);

    match &stmt.kind {
        StmtKind::Let { binding, .. } | StmtKind::Val { binding, .. } => {
            summary.defs.push(*binding);
        }
        StmtKind::Call { result, .. } | StmtKind::Resume { result, .. } => {
            summary.defs.push(*result);
        }
        StmtKind::Perform {
            result: Some(result),
            ..
        } => {
            summary.defs.push(*result);
        }
        StmtKind::Perform { result: None, .. } => {}
        StmtKind::Match { arms, .. } => {
            for arm in arms {
                for binder in arm.binders.iter().copied() {
                    push_unique_def(&mut summary.defs, binder);
                }
            }
        }
        StmtKind::Return(_)
        | StmtKind::If { .. }
        | StmtKind::Handle { .. }
        | StmtKind::Stage { .. }
        | StmtKind::Hole { .. }
        | StmtKind::Error(_) => {}
    }

    if let StmtKind::Resume { resume, .. } = stmt.kind {
        push_unique_use(&mut summary.uses, resume);
    }

    dedup_defs(&mut summary.defs);
    summary
}

fn collect_expr_uses(
    program: &CoreProgram,
    expr_id: ExprId,
    seen_exprs: &mut HashSet<ExprId>,
    out: &mut SmallVec<[VarId; 4]>,
) {
    if !seen_exprs.insert(expr_id) {
        return;
    }
    let Some(expr) = program.expr(expr_id) else {
        return;
    };
    match &expr.kind {
        ExprKind::Var(var) => push_unique_use(out, *var),
        ExprKind::Unary { expr, .. } => collect_expr_uses(program, *expr, seen_exprs, out),
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_expr_uses(program, *lhs, seen_exprs, out);
            collect_expr_uses(program, *rhs, seen_exprs, out);
        }
        ExprKind::PureCall { args, .. }
        | ExprKind::MakeStruct { fields: args, .. }
        | ExprKind::MakeEnum { fields: args, .. } => {
            for arg in args {
                collect_expr_uses(program, *arg, seen_exprs, out);
            }
        }
        ExprKind::Literal(_) | ExprKind::Error(_) => {}
    }
}

fn push_unique_use(out: &mut SmallVec<[VarId; 4]>, var: VarId) {
    if !out.contains(&var) {
        out.push(var);
    }
}

fn push_unique_def(out: &mut SmallVec<[VarId; 2]>, var: VarId) {
    if !out.contains(&var) {
        out.push(var);
    }
}

fn dedup_vars(out: &mut SmallVec<[VarId; 4]>) {
    out.sort_by_key(|var| var.index());
    out.dedup();
}

fn dedup_defs(out: &mut SmallVec<[VarId; 2]>) {
    out.sort_by_key(|var| var.index());
    out.dedup();
}
