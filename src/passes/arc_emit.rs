use std::collections::HashMap;

use crate::common::ids::{LinearStmtId, VarId};
use crate::ir::linear::LinearProgram;

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

    pub fn build(program: &LinearProgram) -> Self {
        let mut plan = ArcEmitPlan::default();
        for idx in 0..program.stmts().len() {
            let stmt_id = LinearStmtId::new(idx);
            let Some(stmt) = program.stmt(stmt_id) else {
                continue;
            };

            if !stmt.arc_ops.pre_retain.is_empty() {
                let mut vars = stmt.arc_ops.pre_retain.iter().copied().collect::<Vec<_>>();
                vars.sort_by_key(|var| var.index());
                vars.dedup();
                if !vars.is_empty() {
                    plan.stats.planned_retain_ops = plan
                        .stats
                        .planned_retain_ops
                        .saturating_add(vars.len() as u32);
                    plan.stats.optimized_retain_ops = plan
                        .stats
                        .optimized_retain_ops
                        .saturating_add(vars.len() as u32);
                    plan.pre_retain_by_stmt.insert(stmt_id, vars);
                }
            }

            if !stmt.arc_ops.post_release.is_empty() {
                let mut vars = stmt
                    .arc_ops
                    .post_release
                    .iter()
                    .copied()
                    .collect::<Vec<_>>();
                vars.sort_by_key(|var| var.index());
                vars.dedup();
                if !vars.is_empty() {
                    plan.stats.planned_release_ops = plan
                        .stats
                        .planned_release_ops
                        .saturating_add(vars.len() as u32);
                    plan.stats.optimized_release_ops = plan
                        .stats
                        .optimized_release_ops
                        .saturating_add(vars.len() as u32);
                    plan.post_release_by_stmt.insert(stmt_id, vars);
                }
            }
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
