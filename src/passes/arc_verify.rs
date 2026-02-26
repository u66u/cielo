use std::collections::HashSet;

use crate::common::diagnostics::DiagnosticBag;
use crate::common::ids::{LinearStmtId, VarId};
use crate::common::span::Span;
use crate::ir::linear::LinearProgram;
use crate::passes::arc_emit::ArcEmitPlan;
use crate::pipeline::phases::SemanticTables;
use crate::sema::ownership::OwnershipClass;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ArcVerifyStats {
    pub checked_stmts: u32,
    pub checked_retain_ops: u32,
    pub checked_release_ops: u32,
    pub errors: u32,
}

pub fn verify(
    program: &LinearProgram,
    sema: &SemanticTables,
    plan: &ArcEmitPlan,
    diagnostics: &mut DiagnosticBag,
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
            if !retain_set.insert(*var) {
                diagnostics.error(
                    "ARC_VERIFY_DUP_RETAIN",
                    format!(
                        "duplicate ARC retain op planned for linear stmt s{} var v{}",
                        stmt_id.as_u32(),
                        var.as_u32()
                    ),
                    Span::synthetic(),
                );
                stats.errors = stats.errors.saturating_add(1);
                continue;
            }
            if !is_managed_var(sema, *var) {
                diagnostics.error(
                    "ARC_VERIFY_NON_MANAGED_OP",
                    format!(
                        "ARC retain planned for non-managed var v{} at linear stmt s{}",
                        var.as_u32(),
                        stmt_id.as_u32()
                    ),
                    Span::synthetic(),
                );
                stats.errors = stats.errors.saturating_add(1);
            }
        }
        for var in releases {
            if !release_set.insert(*var) {
                diagnostics.error(
                    "ARC_VERIFY_DUP_RELEASE",
                    format!(
                        "duplicate ARC release op planned for linear stmt s{} var v{}",
                        stmt_id.as_u32(),
                        var.as_u32()
                    ),
                    Span::synthetic(),
                );
                stats.errors = stats.errors.saturating_add(1);
                continue;
            }
            if !is_managed_var(sema, *var) {
                diagnostics.error(
                    "ARC_VERIFY_NON_MANAGED_OP",
                    format!(
                        "ARC release planned for non-managed var v{} at linear stmt s{}",
                        var.as_u32(),
                        stmt_id.as_u32()
                    ),
                    Span::synthetic(),
                );
                stats.errors = stats.errors.saturating_add(1);
            }
        }
        for var in retain_set.intersection(&release_set) {
            diagnostics.error(
                "ARC_VERIFY_CONFLICTING_OPS",
                format!(
                    "ARC retain+release both planned for linear stmt s{} var v{} after optimization",
                    stmt_id.as_u32(),
                    var.as_u32()
                ),
                Span::synthetic(),
            );
            stats.errors = stats.errors.saturating_add(1);
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

fn is_managed_var(sema: &SemanticTables, var: VarId) -> bool {
    sema.ownership_of_var
        .get(&var)
        .copied()
        .unwrap_or(OwnershipClass::BorrowedView)
        == OwnershipClass::RcManaged
}
