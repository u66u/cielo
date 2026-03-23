use std::collections::HashMap;

use crate::common::diagnostics::DiagnosticBag;
use crate::common::ids::{ExprId, StmtId, VarId};
use crate::common::span::Span;
use crate::ir::core::{CoreProgram, ExprKind, StmtKind};
use crate::pipeline::phases::SemanticTables;
use crate::sema::ownership::OwnershipClass;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum BorrowHazardKind {
    AliasFanout,
    Projection,
    CallEscape,
}

impl BorrowHazardKind {
    fn diagnostic_code(self) -> &'static str {
        match self {
            Self::AliasFanout => "BORROW_HAZARD_ALIAS_FANOUT",
            Self::Projection => "BORROW_HAZARD_PROJECTION",
            Self::CallEscape => "BORROW_HAZARD_CALL_ESCAPE",
        }
    }

    fn diagnostic_message(self) -> &'static str {
        match self {
            Self::AliasFanout => {
                "managed alias fanout may conflict with future borrow/cursor exclusivity"
            }
            Self::Projection => {
                "managed match projection introduces borrow/cursor mutation hazard potential"
            }
            Self::CallEscape => {
                "managed value escapes through call boundary with potential borrow hazard"
            }
        }
    }

    fn repro_tag(self) -> &'static str {
        match self {
            Self::AliasFanout => "alias-fanout",
            Self::Projection => "projection",
            Self::CallEscape => "call-escape",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct BorrowHazardHotspot {
    pub kind: BorrowHazardKind,
    pub stmt: StmtId,
}

impl BorrowHazardHotspot {
    pub fn repro_key(self) -> String {
        format!("{}-s{}", self.kind.repro_tag(), self.stmt.as_u32())
    }
}

#[derive(Clone, Debug, Default)]
pub struct BorrowHazardReport {
    pub alias_fanout_count: u32,
    pub projection_count: u32,
    pub call_escape_count: u32,
    pub alias_fanout_sites: Vec<StmtId>,
    pub projection_sites: Vec<StmtId>,
    pub call_escape_sites: Vec<StmtId>,
    pub hotspots: Vec<BorrowHazardHotspot>,
}

pub fn analyze(program: &CoreProgram, sema: &SemanticTables) -> BorrowHazardReport {
    let reachable = collect_reachable(program);
    let mut use_counts = HashMap::<VarId, u32>::new();
    for stmt_id in &reachable {
        if let Some(stmt) = program.stmt(*stmt_id) {
            let mut seen = std::collections::HashSet::new();
            for expression in stmt.child_exprs() {
                collect_var_uses(program, expression, &mut seen, &mut use_counts);
            }
        }
    }

    let mut report = BorrowHazardReport::default();
    for stmt_id in reachable {
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
    report.hotspots = collect_hotspots(&report);
    report
}

fn collect_reachable(program: &CoreProgram) -> Vec<StmtId> {
    let mut stack = program
        .functions()
        .iter()
        .map(|function| function.body)
        .collect::<Vec<_>>();
    let mut seen = std::collections::HashSet::new();
    while let Some(stmt) = stack.pop() {
        if seen.insert(stmt)
            && let Some(node) = program.stmt(stmt)
        {
            stack.extend(node.child_stmts());
        }
    }
    let mut result = seen.into_iter().collect::<Vec<_>>();
    result.sort_by_key(|stmt| stmt.index());
    result
}

fn collect_var_uses(
    program: &CoreProgram,
    expression: ExprId,
    seen: &mut std::collections::HashSet<ExprId>,
    counts: &mut HashMap<VarId, u32>,
) {
    if !seen.insert(expression) {
        return;
    }
    let Some(expression) = program.expr(expression) else {
        return;
    };
    match &expression.kind {
        ExprKind::Var(var) => {
            let count = counts.entry(*var).or_default();
            *count = count.saturating_add(1);
        }
        ExprKind::Unary { expr, .. } => collect_var_uses(program, *expr, seen, counts),
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_var_uses(program, *lhs, seen, counts);
            collect_var_uses(program, *rhs, seen, counts);
        }
        ExprKind::PureCall { args, .. }
        | ExprKind::MakeStruct { fields: args, .. }
        | ExprKind::MakeEnum { fields: args, .. } => {
            for arg in args {
                collect_var_uses(program, *arg, seen, counts);
            }
        }
        ExprKind::Literal(_) | ExprKind::Error(_) => {}
    }
}

pub fn emit_diagnostics(
    program: &CoreProgram,
    report: &BorrowHazardReport,
    diagnostics: &mut DiagnosticBag,
) {
    let mut alias_count = 0u32;
    let mut projection_count = 0u32;
    let mut call_escape_count = 0u32;
    for hotspot in &report.hotspots {
        match hotspot.kind {
            BorrowHazardKind::AliasFanout => alias_count = alias_count.saturating_add(1),
            BorrowHazardKind::Projection => projection_count = projection_count.saturating_add(1),
            BorrowHazardKind::CallEscape => call_escape_count = call_escape_count.saturating_add(1),
        }
        let span = program
            .stmt(hotspot.stmt)
            .map(|stmt| stmt.span)
            .unwrap_or_else(Span::synthetic);
        diagnostics.warning(
            hotspot.kind.diagnostic_code(),
            format!(
                "{} (stmt s{}, repro={})",
                hotspot.kind.diagnostic_message(),
                hotspot.stmt.as_u32(),
                hotspot.repro_key()
            ),
            span,
        );
    }

    let total = report.hotspots.len() as u32;
    if total > 0 {
        let repro_keys = report
            .hotspots
            .iter()
            .take(8)
            .map(|hotspot| hotspot.repro_key())
            .collect::<Vec<_>>()
            .join(", ");
        diagnostics.note(
            "BORROW_HAZARD_SUMMARY",
            format!(
                "borrow hazard groundwork flagged {} site(s): alias_fanout={}, projection={}, call_escape={}, repro_keys=[{}]",
                total, alias_count, projection_count, call_escape_count, repro_keys
            ),
            Span::synthetic(),
        );
    }
}

fn collect_hotspots(report: &BorrowHazardReport) -> Vec<BorrowHazardHotspot> {
    let mut hotspots = Vec::new();
    for stmt in &report.alias_fanout_sites {
        hotspots.push(BorrowHazardHotspot {
            kind: BorrowHazardKind::AliasFanout,
            stmt: *stmt,
        });
    }
    for stmt in &report.projection_sites {
        hotspots.push(BorrowHazardHotspot {
            kind: BorrowHazardKind::Projection,
            stmt: *stmt,
        });
    }
    for stmt in &report.call_escape_sites {
        hotspots.push(BorrowHazardHotspot {
            kind: BorrowHazardKind::CallEscape,
            stmt: *stmt,
        });
    }
    hotspots.sort_by_key(|site| (site.stmt.index(), site.kind));
    hotspots.dedup();
    hotspots
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
