use std::collections::HashMap;

use crate::analysis::arc_cfg::ArcCfg;
use crate::common::diagnostics::DiagnosticBag;
use crate::common::ids::{ExprId, StmtId, VarId};
use crate::common::span::Span;
use crate::ir::core::{CoreProgram, ExprKind, StmtKind};
use crate::pipeline::phases::SemanticTables;
use crate::sema::ownership::OwnershipClass;

#[derive(Clone, Debug, Default)]
pub struct BorrowHazardReport {
    pub alias_fanout_count: u32,
    pub projection_count: u32,
    pub call_escape_count: u32,
    pub alias_fanout_sites: Vec<StmtId>,
    pub projection_sites: Vec<StmtId>,
    pub call_escape_sites: Vec<StmtId>,
}

pub fn analyze(program: &CoreProgram, sema: &SemanticTables) -> BorrowHazardReport {
    let cfg = ArcCfg::build(program);
    let mut use_counts = HashMap::<VarId, u32>::new();
    for stmt_id in cfg.reachable() {
        if let Some(summary) = cfg.summary(*stmt_id) {
            for used in &summary.uses {
                let count = use_counts.entry(*used).or_insert(0);
                *count = count.saturating_add(1);
            }
        }
    }

    let mut report = BorrowHazardReport::default();
    for stmt_id in cfg.reachable().iter().copied() {
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        match &stmt.kind {
            StmtKind::Let { binding, value, .. } => {
                if let Some(ExprKind::Var(source)) = program.expr(*value).map(|expr| &expr.kind)
                    && is_managed(sema, *source)
                    && is_managed(sema, *binding)
                    && use_counts.get(source).copied().unwrap_or(0) > 1
                {
                    push_site(&mut report.alias_fanout_sites, stmt_id);
                }
            }
            StmtKind::Match {
                scrutinee, arms, ..
            } => {
                if let Some(ExprKind::Var(source)) = program.expr(*scrutinee).map(|expr| &expr.kind)
                    && is_managed(sema, *source)
                    && arms
                        .iter()
                        .flat_map(|arm| arm.binders.iter())
                        .copied()
                        .any(|binder| is_managed(sema, binder))
                {
                    push_site(&mut report.projection_sites, stmt_id);
                }
            }
            StmtKind::Call { args, .. } | StmtKind::Perform { args, .. } => {
                let mut seen_exprs = std::collections::HashSet::new();
                let mut hazard = false;
                for arg in args {
                    let vars = collect_managed_vars(program, sema, *arg, &mut seen_exprs);
                    if vars
                        .iter()
                        .any(|var| use_counts.get(var).copied().unwrap_or(0) > 1)
                    {
                        hazard = true;
                    }
                }
                if hazard {
                    push_site(&mut report.call_escape_sites, stmt_id);
                }
            }
            StmtKind::Return(_)
            | StmtKind::If { .. }
            | StmtKind::Resume { .. }
            | StmtKind::Val { .. }
            | StmtKind::Handle { .. }
            | StmtKind::Stage { .. }
            | StmtKind::Hole { .. }
            | StmtKind::Error(_) => {}
        }
    }

    report.alias_fanout_sites.sort_by_key(|id| id.index());
    report.alias_fanout_sites.dedup();
    report.projection_sites.sort_by_key(|id| id.index());
    report.projection_sites.dedup();
    report.call_escape_sites.sort_by_key(|id| id.index());
    report.call_escape_sites.dedup();
    report.alias_fanout_count = report.alias_fanout_sites.len() as u32;
    report.projection_count = report.projection_sites.len() as u32;
    report.call_escape_count = report.call_escape_sites.len() as u32;
    report
}

pub fn emit_diagnostics(
    program: &CoreProgram,
    report: &BorrowHazardReport,
    diagnostics: &mut DiagnosticBag,
) {
    let alias_count = emit_hazard_sites(
        program,
        &report.alias_fanout_sites,
        diagnostics,
        "BORROW_HAZARD_ALIAS_FANOUT",
        "managed alias fanout may conflict with future borrow/cursor exclusivity",
    );
    let projection_count = emit_hazard_sites(
        program,
        &report.projection_sites,
        diagnostics,
        "BORROW_HAZARD_PROJECTION",
        "managed match projection introduces borrow/cursor mutation hazard potential",
    );
    let call_escape_count = emit_hazard_sites(
        program,
        &report.call_escape_sites,
        diagnostics,
        "BORROW_HAZARD_CALL_ESCAPE",
        "managed value escapes through call boundary with potential borrow hazard",
    );

    let total = alias_count
        .saturating_add(projection_count)
        .saturating_add(call_escape_count);
    if total > 0 {
        diagnostics.note(
            "BORROW_HAZARD_SUMMARY",
            format!(
                "borrow hazard groundwork flagged {} site(s): alias_fanout={}, projection={}, call_escape={}",
                total, alias_count, projection_count, call_escape_count
            ),
            Span::synthetic(),
        );
    }
}

fn emit_hazard_sites(
    program: &CoreProgram,
    sites: &[StmtId],
    diagnostics: &mut DiagnosticBag,
    code: &'static str,
    message: &str,
) -> u32 {
    let mut emitted = 0u32;
    for stmt_id in sites {
        let span = program
            .stmt(*stmt_id)
            .map(|stmt| stmt.span)
            .unwrap_or_else(Span::synthetic);
        diagnostics.warning(
            code,
            format!("{message} (stmt s{})", stmt_id.as_u32()),
            span,
        );
        emitted = emitted.saturating_add(1);
    }
    emitted
}

fn collect_managed_vars(
    program: &CoreProgram,
    sema: &SemanticTables,
    expr_id: ExprId,
    seen: &mut std::collections::HashSet<ExprId>,
) -> Vec<VarId> {
    if !seen.insert(expr_id) {
        return Vec::new();
    }
    let Some(expr) = program.expr(expr_id) else {
        return Vec::new();
    };
    match &expr.kind {
        ExprKind::Var(var) if is_managed(sema, *var) => vec![*var],
        ExprKind::Unary { expr, .. } => collect_managed_vars(program, sema, *expr, seen),
        ExprKind::Binary { lhs, rhs, .. } => {
            let mut out = collect_managed_vars(program, sema, *lhs, seen);
            for var in collect_managed_vars(program, sema, *rhs, seen) {
                if !out.contains(&var) {
                    out.push(var);
                }
            }
            out
        }
        ExprKind::PureCall { args, .. }
        | ExprKind::MakeStruct { fields: args, .. }
        | ExprKind::MakeEnum { fields: args, .. } => {
            let mut out = Vec::new();
            for arg in args {
                for var in collect_managed_vars(program, sema, *arg, seen) {
                    if !out.contains(&var) {
                        out.push(var);
                    }
                }
            }
            out
        }
        ExprKind::Literal(_) | ExprKind::Error(_) | ExprKind::Var(_) => Vec::new(),
    }
}

fn is_managed(sema: &SemanticTables, var: VarId) -> bool {
    sema.ownership_of_var
        .get(&var)
        .copied()
        .unwrap_or(OwnershipClass::BorrowedView)
        == OwnershipClass::RcManaged
}

fn push_site(out: &mut Vec<StmtId>, stmt_id: StmtId) {
    if !out.contains(&stmt_id) {
        out.push(stmt_id);
    }
}
