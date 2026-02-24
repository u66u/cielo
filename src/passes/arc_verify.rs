use std::collections::HashSet;

use crate::common::ids::{LinearStmtId, VarId};
use crate::ir::linear::LinearProgram;
use crate::passes::arc_emit::ArcEmitPlan;
use crate::pipeline::phases::SemanticTables;
use crate::sema::ownership::OwnershipClass;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ArcVerifyStats {
    pub checked_stmts: u32,
    pub checked_retain_ops: u32,
    pub checked_release_ops: u32,
}

pub fn assert_valid(
    program: &LinearProgram,
    sema: &SemanticTables,
    plan: &ArcEmitPlan,
) -> ArcVerifyStats {
    let mut stats = ArcVerifyStats::default();
    for idx in 0..program.stmts().len() {
        let stmt_id = LinearStmtId::new(idx);
        stats.checked_stmts = stats.checked_stmts.saturating_add(1);
        let retains = plan.pre_retain_vars(stmt_id);
        let releases = plan.post_release_vars(stmt_id);
        let mut retain_set = HashSet::new();
        let mut release_set = HashSet::new();

        for var in retains {
            assert!(
                retain_set.insert(*var),
                "compiler bug: duplicate ARC retain op planned for stmt s{} var v{}",
                stmt_id.as_u32(),
                var.as_u32()
            );
            assert_managed_var(sema, *var, stmt_id, "retain");
        }
        for var in releases {
            assert!(
                release_set.insert(*var),
                "compiler bug: duplicate ARC release op planned for stmt s{} var v{}",
                stmt_id.as_u32(),
                var.as_u32()
            );
            assert_managed_var(sema, *var, stmt_id, "release");
        }
        for var in retain_set.intersection(&release_set) {
            panic!(
                "compiler bug: ARC retain+release both planned on stmt s{} var v{} after optimization",
                stmt_id.as_u32(),
                var.as_u32()
            );
        }

        stats.checked_retain_ops = stats
            .checked_retain_ops
            .saturating_add(retains.len() as u32);
        stats.checked_release_ops = stats
            .checked_release_ops
            .saturating_add(releases.len() as u32);
    }
    stats
}

fn assert_managed_var(sema: &SemanticTables, var: VarId, stmt_id: LinearStmtId, op: &str) {
    let ownership = sema
        .ownership_of_var
        .get(&var)
        .copied()
        .unwrap_or(OwnershipClass::BorrowedView);
    assert!(
        ownership == OwnershipClass::RcManaged,
        "compiler bug: ARC {} op planned for non-managed var v{} at stmt s{}",
        op,
        var.as_u32(),
        stmt_id.as_u32()
    );
}
