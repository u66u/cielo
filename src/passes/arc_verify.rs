use std::collections::{HashMap, HashSet, VecDeque};

use crate::common::diagnostics::DiagnosticBag;
use crate::common::ids::{LinearStmtId, VarId};
use crate::common::span::Span;
use crate::ir::linear::{LinearProgram, LinearStmt};
use crate::passes::arc_emit::ArcEmitPlan;
use crate::pipeline::phases::SemanticTables;
use crate::sema::ownership::OwnershipClass;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ArcVerifyStats {
    pub checked_stmts: u32,
    pub checked_retain_ops: u32,
    pub checked_release_ops: u32,
    pub state_edges_checked: u32,
    pub errors: u32,
}

pub fn verify(
    program: &LinearProgram,
    sema: &SemanticTables,
    plan: &ArcEmitPlan,
    diagnostics: &mut DiagnosticBag,
) -> ArcVerifyStats {
    let mut stats = ArcVerifyStats::default();
    let mut conflicting_ops = HashSet::new();
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
            if conflicting_ops.insert((stmt_id, *var)) {
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
        }

        stats.checked_retain_ops = stats
            .checked_retain_ops
            .saturating_add(retains.len() as u32);
        stats.checked_release_ops = stats
            .checked_release_ops
            .saturating_add(releases.len() as u32);
    }
    verify_state_transitions(program, sema, plan, diagnostics, &mut stats);
    stats
}

fn is_managed_var(sema: &SemanticTables, var: VarId) -> bool {
    sema.ownership_of_var
        .get(&var)
        .copied()
        .unwrap_or(OwnershipClass::BorrowedView)
        == OwnershipClass::RcManaged
}

fn verify_state_transitions(
    program: &LinearProgram,
    sema: &SemanticTables,
    plan: &ArcEmitPlan,
    diagnostics: &mut DiagnosticBag,
    stats: &mut ArcVerifyStats,
) {
    let stmt_count = program.stmts().len();
    if stmt_count == 0 {
        return;
    }

    let mut in_states = vec![None::<HashMap<VarId, i8>>; stmt_count];
    let mut worklist = VecDeque::new();
    for function in &program.functions {
        let mut seed = HashMap::new();
        for param in &function.params {
            if is_managed_var(sema, *param) {
                seed.insert(*param, 1);
            }
        }
        if merge_in_state(&mut in_states[function.body.index()], &seed) {
            worklist.push_back(function.body);
        }
    }

    let mut underflow_reported = HashSet::new();
    while let Some(stmt_id) = worklist.pop_front() {
        let Some(mut state) = in_states[stmt_id.index()].clone() else {
            continue;
        };

        for var in plan.pre_retain_vars(stmt_id) {
            apply_delta(&mut state, *var, 1);
        }
        for var in managed_defs_for_stmt(program, sema, stmt_id) {
            apply_delta(&mut state, var, 1);
        }
        for var in plan.post_release_vars(stmt_id) {
            let next = apply_delta(&mut state, *var, -1);
            if next < 0 && underflow_reported.insert((stmt_id, *var)) {
                diagnostics.error(
                    "ARC_VERIFY_RELEASE_UNDERFLOW",
                    format!(
                        "possible release-underflow: linear stmt s{} releases var v{} without ownership credit on some path",
                        stmt_id.as_u32(),
                        var.as_u32()
                    ),
                    Span::synthetic(),
                );
                stats.errors = stats.errors.saturating_add(1);
            }
        }

        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        for succ in stmt.child_stmts() {
            stats.state_edges_checked = stats.state_edges_checked.saturating_add(1);
            if merge_in_state(&mut in_states[succ.index()], &state) {
                worklist.push_back(succ);
            }
        }
    }
}

fn managed_defs_for_stmt(
    program: &LinearProgram,
    sema: &SemanticTables,
    stmt_id: LinearStmtId,
) -> Vec<VarId> {
    let Some(stmt) = program.stmt(stmt_id) else {
        return Vec::new();
    };
    let mut defs = Vec::new();
    match &stmt.kind {
        LinearStmt::Let { binding, .. } | LinearStmt::Val { binding, .. } => {
            if is_managed_var(sema, *binding) {
                defs.push(*binding);
            }
        }
        LinearStmt::PureCall { result, .. }
        | LinearStmt::DirectCall { result, .. }
        | LinearStmt::ControlCall { result, .. } => {
            if is_managed_var(sema, *result) {
                defs.push(*result);
            }
        }
        LinearStmt::Perform {
            result: Some(result),
            ..
        } => {
            if is_managed_var(sema, *result) {
                defs.push(*result);
            }
        }
        LinearStmt::Match { arms, .. } => {
            for arm in arms {
                for binder in arm.binders.iter().copied() {
                    if is_managed_var(sema, binder) && !defs.contains(&binder) {
                        defs.push(binder);
                    }
                }
            }
        }
        LinearStmt::Perform { result: None, .. }
        | LinearStmt::Return(_)
        | LinearStmt::If { .. }
        | LinearStmt::Handle { .. }
        | LinearStmt::Stage { .. }
        | LinearStmt::Hole
        | LinearStmt::Error => {}
    }
    defs
}

fn merge_in_state(target: &mut Option<HashMap<VarId, i8>>, incoming: &HashMap<VarId, i8>) -> bool {
    match target {
        None => {
            *target = Some(incoming.clone());
            true
        }
        Some(current) => {
            let mut keys = current.keys().copied().collect::<HashSet<_>>();
            keys.extend(incoming.keys().copied());
            let mut changed = false;
            for key in keys {
                let current_credit = *current.get(&key).unwrap_or(&0);
                let incoming_credit = *incoming.get(&key).unwrap_or(&0);
                let merged_credit = current_credit.min(incoming_credit);
                if merged_credit != current_credit {
                    changed = true;
                    if merged_credit == 0 {
                        current.remove(&key);
                    } else {
                        current.insert(key, merged_credit);
                    }
                }
            }
            changed
        }
    }
}

fn apply_delta(state: &mut HashMap<VarId, i8>, var: VarId, delta: i8) -> i8 {
    let current = *state.get(&var).unwrap_or(&0);
    let next_i16 = current as i16 + delta as i16;
    let next = if next_i16 < -1 {
        -1
    } else if next_i16 > i8::MAX as i16 {
        i8::MAX
    } else {
        next_i16 as i8
    };
    if next == 0 {
        state.remove(&var);
    } else {
        state.insert(var, next);
    }
    next
}
