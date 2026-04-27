use std::collections::HashSet;

use cielo_base::ids::{ExprId, FuncId, StmtId};

use crate::core::{CoreProgram, ExprKind, StmtKind};

pub fn collect_reachable_functions(program: &CoreProgram) -> Vec<FuncId> {
    let mut seen = HashSet::new();
    let mut stack = program.entrypoints().to_vec();
    while let Some(func_id) = stack.pop() {
        if !seen.insert(func_id) {
            continue;
        }
        let Some(function) = program.function(func_id) else {
            continue;
        };
        for callee in collect_stmt_callees(program, function.body) {
            stack.push(callee);
        }
    }
    let mut out = seen.into_iter().collect::<Vec<_>>();
    out.sort_by_key(|id| id.index());
    out
}

fn collect_stmt_callees(program: &CoreProgram, root: StmtId) -> Vec<FuncId> {
    let mut callees = HashSet::new();
    let mut seen_stmts = HashSet::new();
    let mut seen_exprs = HashSet::new();
    let mut stack = vec![root];
    while let Some(stmt_id) = stack.pop() {
        if !seen_stmts.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if let StmtKind::Call { callee, .. } = stmt.kind {
            callees.insert(callee);
        }
        for expr_id in stmt.child_exprs() {
            collect_expr_callees(program, expr_id, &mut seen_exprs, &mut callees);
        }
        for child in stmt.child_stmts() {
            stack.push(child);
        }
    }
    let mut out = callees.into_iter().collect::<Vec<_>>();
    out.sort_by_key(|id| id.index());
    out
}

fn collect_expr_callees(
    program: &CoreProgram,
    expr_id: ExprId,
    seen_exprs: &mut HashSet<ExprId>,
    out: &mut HashSet<FuncId>,
) {
    if !seen_exprs.insert(expr_id) {
        return;
    }
    let Some(expr) = program.expr(expr_id) else {
        return;
    };
    match &expr.kind {
        ExprKind::Unary { expr, .. } | ExprKind::Field { base: expr, .. } => {
            collect_expr_callees(program, *expr, seen_exprs, out)
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_expr_callees(program, *lhs, seen_exprs, out);
            collect_expr_callees(program, *rhs, seen_exprs, out);
        }
        ExprKind::PureCall { callee, args } => {
            out.insert(*callee);
            for arg in args {
                collect_expr_callees(program, *arg, seen_exprs, out);
            }
        }
        ExprKind::MakeStruct { fields, .. } | ExprKind::MakeEnum { fields, .. } => {
            for field in fields {
                collect_expr_callees(program, *field, seen_exprs, out);
            }
        }
        ExprKind::Var(_) | ExprKind::Literal(_) | ExprKind::Error(_) => {}
    }
}

pub fn dense_remap(total_functions: usize, reachable: &[FuncId]) -> Vec<Option<FuncId>> {
    let mut remap = vec![None; total_functions];
    for (dense_idx, source_id) in reachable.iter().copied().enumerate() {
        remap[source_id.index()] = Some(FuncId::new(dense_idx));
    }
    remap
}

pub fn prune_unreachable_functions(program: &mut CoreProgram) -> Vec<Option<FuncId>> {
    let reachable = collect_reachable_functions(program);
    let remap = dense_remap(program.functions().len(), &reachable);
    if reachable.len() == program.functions().len() {
        return remap;
    }

    remap_program_function_ids(program, &remap);
    let compacted_functions = reachable
        .iter()
        .copied()
        .filter_map(|source_id| program.function(source_id).cloned())
        .collect::<Vec<_>>();
    let compacted_entrypoints = program
        .entrypoints()
        .iter()
        .filter_map(|entry| remap_func_id(&remap, *entry))
        .collect::<Vec<_>>();
    program.replace_functions(compacted_functions);
    program.set_entrypoints(compacted_entrypoints);
    remap
}

pub fn remap_func_id(remap: &[Option<FuncId>], source: FuncId) -> Option<FuncId> {
    remap.get(source.index()).copied().flatten()
}

pub fn remap_program_function_ids(program: &mut CoreProgram, remap: &[Option<FuncId>]) {
    let stmt_count = program.stmts().len();
    for stmt_idx in 0..stmt_count {
        let stmt_id = StmtId::new(stmt_idx);
        let Some(stmt) = program.stmt_mut(stmt_id) else {
            continue;
        };
        if let StmtKind::Call { callee, .. } = &mut stmt.kind
            && let Some(mapped) = remap_func_id(remap, *callee)
        {
            *callee = mapped;
        }
    }

    let expr_count = program.exprs().len();
    for expr_idx in 0..expr_count {
        let expr_id = ExprId::new(expr_idx);
        let Some(expr) = program.expr_mut(expr_id) else {
            continue;
        };
        if let ExprKind::PureCall { callee, .. } = &mut expr.kind
            && let Some(mapped) = remap_func_id(remap, *callee)
        {
            *callee = mapped;
        }
    }
}
