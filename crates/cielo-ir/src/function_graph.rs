use std::collections::HashSet;

use cielo_base::ids::{ExprId, FuncId, HandlerId, StmtId};

use crate::core::{CoreProgram, ExprKind, StmtKind};
use crate::walk::{Walk, walk_exprs_from};

/// A handler's clause bodies and return body are ordinary reachable code, so a
/// `Handle` in a walked body pulls them into the walk. Omitting them lets
/// `prune_unreachable_functions` drop a callee that only a clause body calls;
/// the stale `FuncId` left behind in that clause then silently aliases
/// whichever function lands in the freed dense slot.
pub fn collect_reachable_functions(program: &CoreProgram) -> Vec<FuncId> {
    let mut seen_funcs = HashSet::new();
    let mut seen_handlers = HashSet::new();
    let mut func_stack = program.entrypoints().to_vec();
    let mut body_stack: Vec<StmtId> = Vec::new();

    loop {
        if let Some(func_id) = func_stack.pop() {
            if seen_funcs.insert(func_id)
                && let Some(function) = program.function(func_id)
            {
                body_stack.push(function.body);
            }
            continue;
        }
        let Some(body) = body_stack.pop() else {
            break;
        };
        let refs = collect_stmt_refs(program, body);
        func_stack.extend(refs.callees);
        for handler_id in refs.handlers {
            if !seen_handlers.insert(handler_id) {
                continue;
            }
            let Some(handler) = program.handlers().get(handler_id.index()) else {
                continue;
            };
            body_stack.push(handler.return_body);
            body_stack.extend(handler.clauses.iter().map(|clause| clause.body));
        }
    }

    let mut out = seen_funcs.into_iter().collect::<Vec<_>>();
    out.sort_by_key(|id| id.index());
    out
}

struct StmtRefs {
    callees: Vec<FuncId>,
    handlers: Vec<HandlerId>,
}

fn collect_stmt_refs(program: &CoreProgram, root: StmtId) -> StmtRefs {
    let mut callees = HashSet::new();
    let mut handlers = HashSet::new();
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
        match stmt.kind {
            StmtKind::Call { callee, .. } => {
                callees.insert(callee);
            }
            StmtKind::Handle { handler, .. } => {
                handlers.insert(handler);
            }
            _ => {}
        }
        for expr_id in stmt.child_exprs() {
            collect_expr_callees(program, expr_id, &mut seen_exprs, &mut callees);
        }
        for child in stmt.child_stmts() {
            stack.push(child);
        }
    }
    let mut callees = callees.into_iter().collect::<Vec<_>>();
    callees.sort_by_key(|id| id.index());
    let mut handlers = handlers.into_iter().collect::<Vec<_>>();
    handlers.sort_by_key(|id| id.index());
    StmtRefs { callees, handlers }
}

fn collect_expr_callees(
    program: &CoreProgram,
    expr_id: ExprId,
    seen_exprs: &mut HashSet<ExprId>,
    out: &mut HashSet<FuncId>,
) {
    walk_exprs_from(program, expr_id, seen_exprs, &mut |_, expr| {
        if let ExprKind::PureCall { callee, .. } = &expr.kind {
            out.insert(*callee);
        }
        Walk::Descend
    });
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

/// The id a dropped callee is rewritten to. `remap` is indexed by the old
/// function count, which is never below the new one, so this is out of range in
/// the compacted program.
fn poison_func_id(remap: &[Option<FuncId>]) -> FuncId {
    FuncId::new(remap.len())
}

/// Rewrites every call site in the arena, live or dead, because the arena keeps
/// the bodies of pruned functions and nothing distinguishes them here.
///
/// A callee with no mapping is poisoned rather than left alone. Leaving it
/// stale makes it alias whichever function took the freed dense slot, which is
/// a wrong answer with nothing to point at -- CIELO-52 was exactly that, from a
/// reachability walk that missed handler clause bodies. Dead call sites are
/// never read again, so poisoning them costs nothing; a live one now fails to
/// resolve and the backend emits a name that does not link.
pub fn remap_program_function_ids(program: &mut CoreProgram, remap: &[Option<FuncId>]) {
    let poison = poison_func_id(remap);
    let stmt_count = program.stmts().len();
    for stmt_idx in 0..stmt_count {
        let stmt_id = StmtId::new(stmt_idx);
        let Some(stmt) = program.stmt_mut(stmt_id) else {
            continue;
        };
        if let StmtKind::Call { callee, .. } = &mut stmt.kind {
            *callee = remap_func_id(remap, *callee).unwrap_or(poison);
        }
    }

    let expr_count = program.exprs().len();
    for expr_idx in 0..expr_count {
        let expr_id = ExprId::new(expr_idx);
        let Some(expr) = program.expr_mut(expr_id) else {
            continue;
        };
        if let ExprKind::PureCall { callee, .. } = &mut expr.kind {
            *callee = remap_func_id(remap, *callee).unwrap_or(poison);
        }
    }
}
