use std::collections::HashSet;

use crate::common::ids::{ExprId, StmtId, VarId};
use crate::ir::core::{CoreProgram, ExprKind, StmtKind};
use crate::pipeline::phases::SemanticTables;
use crate::sema::ownership::OwnershipClass;

#[derive(Clone, Debug, Default)]
pub struct OrcFoundationTables {
    pub managed_binding_count: u32,
    pub candidate_root_count: u32,
    pub candidate_link_count: u32,
    pub candidate_roots: Vec<VarId>,
    pub candidate_links: Vec<(VarId, VarId)>,
}

pub fn analyze(program: &CoreProgram, sema: &SemanticTables) -> OrcFoundationTables {
    let reachable = collect_reachable_stmts(program);
    let mut tables = OrcFoundationTables::default();

    for stmt_id in reachable {
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if let StmtKind::Let { binding, value, .. } = &stmt.kind {
            if is_managed_var(sema, *binding) {
                tables.managed_binding_count = tables.managed_binding_count.saturating_add(1);
            }
            collect_candidate_links_for_let(program, sema, *binding, *value, &mut tables);
        }
        if let StmtKind::Match {
            scrutinee, arms, ..
        } = &stmt.kind
            && let Some(ExprKind::Var(scrutinee_var)) =
                program.expr(*scrutinee).map(|expr| &expr.kind)
            && is_managed_var(sema, *scrutinee_var)
        {
            for arm in arms {
                for binder in arm.binders.iter().copied() {
                    if is_managed_var(sema, binder) {
                        push_candidate_link(&mut tables, binder, *scrutinee_var);
                    }
                }
            }
        }
    }

    tables.candidate_roots.sort_by_key(|var| var.index());
    tables.candidate_roots.dedup();
    tables.candidate_links.sort_unstable_by_key(|(lhs, rhs)| {
        (lhs.index().min(rhs.index()), lhs.index().max(rhs.index()))
    });
    tables.candidate_links.dedup();
    tables.candidate_root_count = tables.candidate_roots.len() as u32;
    tables.candidate_link_count = tables.candidate_links.len() as u32;
    tables
}

fn collect_candidate_links_for_let(
    program: &CoreProgram,
    sema: &SemanticTables,
    binding: VarId,
    value: ExprId,
    tables: &mut OrcFoundationTables,
) {
    if !is_managed_var(sema, binding) {
        return;
    }
    let Some(expr) = program.expr(value) else {
        return;
    };
    match &expr.kind {
        ExprKind::Var(source) if is_managed_var(sema, *source) => {
            push_candidate_link(tables, binding, *source);
        }
        ExprKind::MakeStruct { fields, .. } | ExprKind::MakeEnum { fields, .. } => {
            let mut seen_exprs = HashSet::new();
            let mut linked = false;
            for field in fields {
                linked |=
                    collect_field_links(program, sema, binding, *field, tables, &mut seen_exprs);
            }
            if linked {
                push_candidate_root(tables, binding);
            }
        }
        ExprKind::Unary { expr, .. } => {
            let mut seen_exprs = HashSet::new();
            if collect_field_links(program, sema, binding, *expr, tables, &mut seen_exprs) {
                push_candidate_root(tables, binding);
            }
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            let mut seen_exprs = HashSet::new();
            let lhs_linked =
                collect_field_links(program, sema, binding, *lhs, tables, &mut seen_exprs);
            let rhs_linked =
                collect_field_links(program, sema, binding, *rhs, tables, &mut seen_exprs);
            if lhs_linked || rhs_linked {
                push_candidate_root(tables, binding);
            }
        }
        ExprKind::PureCall { args, .. } => {
            let mut seen_exprs = HashSet::new();
            let mut linked = false;
            for arg in args {
                linked |=
                    collect_field_links(program, sema, binding, *arg, tables, &mut seen_exprs);
            }
            if linked {
                push_candidate_root(tables, binding);
            }
        }
        ExprKind::Literal(_) | ExprKind::Error(_) => {}
        ExprKind::Var(_) => {}
    }
}

fn collect_field_links(
    program: &CoreProgram,
    sema: &SemanticTables,
    binding: VarId,
    expr_id: ExprId,
    tables: &mut OrcFoundationTables,
    seen_exprs: &mut HashSet<ExprId>,
) -> bool {
    if !seen_exprs.insert(expr_id) {
        return false;
    }
    let Some(expr) = program.expr(expr_id) else {
        return false;
    };
    match &expr.kind {
        ExprKind::Var(var) if is_managed_var(sema, *var) => {
            push_candidate_link(tables, binding, *var);
            true
        }
        ExprKind::Unary { expr, .. } => {
            collect_field_links(program, sema, binding, *expr, tables, seen_exprs)
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            let lhs = collect_field_links(program, sema, binding, *lhs, tables, seen_exprs);
            let rhs = collect_field_links(program, sema, binding, *rhs, tables, seen_exprs);
            lhs || rhs
        }
        ExprKind::PureCall { args, .. }
        | ExprKind::MakeStruct { fields: args, .. }
        | ExprKind::MakeEnum { fields: args, .. } => {
            let mut linked = false;
            for arg in args {
                linked |= collect_field_links(program, sema, binding, *arg, tables, seen_exprs);
            }
            linked
        }
        ExprKind::Literal(_) | ExprKind::Error(_) | ExprKind::Var(_) => false,
    }
}

fn collect_reachable_stmts(program: &CoreProgram) -> Vec<StmtId> {
    let mut stack = Vec::new();
    stack.extend(program.functions().iter().map(|function| function.body));
    for handler in program.handlers() {
        stack.push(handler.return_body);
        stack.extend(handler.clauses.iter().map(|clause| clause.body));
    }

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

fn push_candidate_root(tables: &mut OrcFoundationTables, var: VarId) {
    if !tables.candidate_roots.contains(&var) {
        tables.candidate_roots.push(var);
    }
}

fn push_candidate_link(tables: &mut OrcFoundationTables, lhs: VarId, rhs: VarId) {
    push_candidate_root(tables, lhs);
    let link = (lhs, rhs);
    if !tables.candidate_links.contains(&link) {
        tables.candidate_links.push(link);
    }
}
