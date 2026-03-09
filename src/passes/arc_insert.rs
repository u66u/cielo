use std::collections::{HashMap, HashSet};

use crate::analysis::arc_alias::ArcAliasTables;
use crate::analysis::arc_cfg::ArcCfg;
use crate::analysis::arc_last_use::ArcLastUseTables;
use crate::common::ids::{ExprId, StmtId, VarId};
use crate::ir::core::{CoreProgram, ExprKind, StmtKind};
use crate::pipeline::phases::{ArcResidualOp, ArcResidualOpKind, ArcResidualPlan, SemanticTables};
use crate::sema::ownership::OwnershipClass;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArcOpKind {
    Retain { var: VarId },
    Release { var: VarId },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ArcPlannedOp {
    pub stmt: StmtId,
    pub kind: ArcOpKind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ArcInsertStats {
    pub retain_ops: u32,
    pub release_ops: u32,
}

#[derive(Clone, Debug, Default)]
pub struct ArcInsertionPlan {
    pub ops: Vec<ArcPlannedOp>,
    pub stats: ArcInsertStats,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CallArgTransferDecision {
    MoveToCallee,
    RetainCopy,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AliasCopyDecision {
    Noop,
    DropDeadBinding { binding: VarId },
    MoveSource { source: VarId },
    RetainCopy { source: VarId },
}

pub fn plan(program: &CoreProgram, sema: &SemanticTables) -> ArcInsertionPlan {
    let cfg = ArcCfg::build(program);
    let alias = ArcAliasTables::analyze(program, &cfg);
    let last_use = ArcLastUseTables::analyze(&cfg);
    let alias_graph = build_alias_graph(&alias);
    let mut plan = ArcInsertionPlan::default();

    for stmt_id in cfg.reachable().iter().copied() {
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };

        let last_uses = last_use.last_uses(stmt_id);
        let last_use_set = last_uses.iter().copied().collect::<HashSet<_>>();
        let live_out_set = last_use.live_out(stmt_id).cloned().unwrap_or_default();
        let mut moved_call_args = HashSet::new();
        let mut moved_alias_sources = HashSet::new();
        let mut dropped_alias_bindings = HashSet::new();
        let mut stmt_var_uses = HashMap::new();
        for expr_id in stmt.child_exprs() {
            collect_expr_var_counts(program, expr_id, &mut stmt_var_uses);
        }
        let mut call_arg_uses = HashMap::new();
        for expr_id in stmt.child_exprs() {
            collect_call_arg_var_counts(program, expr_id, &mut call_arg_uses);
        }
        if let StmtKind::Call { args, .. } = &stmt.kind {
            for arg in args {
                if let Some(ExprKind::Var(var)) = program.expr(*arg).map(|expr| &expr.kind) {
                    bump_var_count(&mut call_arg_uses, *var);
                }
            }
        }
        for (var, use_count) in call_arg_uses {
            let ownership = ownership_of_var(sema, var);
            if ownership != OwnershipClass::RcManaged {
                continue;
            }
            let total_stmt_uses = stmt_var_uses.get(&var).copied().unwrap_or(0);
            let has_live_alias = has_live_alias_after_stmt(var, &live_out_set, &alias_graph, sema);
            match decide_call_arg_transfer(
                ownership,
                use_count,
                total_stmt_uses,
                last_use_set.contains(&var),
                has_live_alias,
            ) {
                CallArgTransferDecision::MoveToCallee => {
                    moved_call_args.insert(var);
                }
                CallArgTransferDecision::RetainCopy => {
                    push_op(
                        &mut plan,
                        ArcPlannedOp {
                            stmt: stmt_id,
                            kind: ArcOpKind::Retain { var },
                        },
                    );
                }
            }
        }

        if let StmtKind::Let {
            binding,
            value,
            next: _,
        } = &stmt.kind
            && let Some(expr) = program.expr(*value)
            && let ExprKind::Var(source) = expr.kind
        {
            let source_ownership = ownership_of_var(sema, source);
            let binding_ownership = ownership_of_var(sema, *binding);
            match decide_alias_copy(
                source,
                *binding,
                source_ownership,
                binding_ownership,
                &last_use_set,
            ) {
                AliasCopyDecision::Noop => {}
                AliasCopyDecision::DropDeadBinding { binding } => {
                    dropped_alias_bindings.insert(binding);
                }
                AliasCopyDecision::MoveSource { source } => {
                    moved_alias_sources.insert(source);
                }
                AliasCopyDecision::RetainCopy { source } => {
                    push_op(
                        &mut plan,
                        ArcPlannedOp {
                            stmt: stmt_id,
                            kind: ArcOpKind::Retain { var: source },
                        },
                    );
                }
            }
        }

        for var in last_uses {
            if moved_call_args.contains(var)
                || moved_alias_sources.contains(var)
                || dropped_alias_bindings.contains(var)
            {
                continue;
            }
            let ownership = ownership_of_var(sema, *var);
            if ownership == OwnershipClass::RcManaged {
                push_op(
                    &mut plan,
                    ArcPlannedOp {
                        stmt: stmt_id,
                        kind: ArcOpKind::Release { var: *var },
                    },
                );
            }
        }
    }

    plan
}

fn ownership_of_var(sema: &SemanticTables, var: VarId) -> OwnershipClass {
    sema.ownership_of_var
        .get(&var)
        .copied()
        .unwrap_or(OwnershipClass::BorrowedView)
}

fn decide_call_arg_transfer(
    ownership: OwnershipClass,
    call_arg_uses: u8,
    stmt_uses: u8,
    is_last_use: bool,
    has_live_alias: bool,
) -> CallArgTransferDecision {
    if ownership == OwnershipClass::RcManaged
        && call_arg_uses == 1
        && stmt_uses == 1
        && is_last_use
        && !has_live_alias
    {
        CallArgTransferDecision::MoveToCallee
    } else {
        CallArgTransferDecision::RetainCopy
    }
}

fn decide_alias_copy(
    source: VarId,
    binding: VarId,
    source_ownership: OwnershipClass,
    binding_ownership: OwnershipClass,
    last_use_set: &HashSet<VarId>,
) -> AliasCopyDecision {
    let is_managed_copy = source_ownership == OwnershipClass::RcManaged
        || binding_ownership == OwnershipClass::RcManaged;
    if !is_managed_copy {
        return AliasCopyDecision::Noop;
    }
    if binding_ownership == OwnershipClass::RcManaged && last_use_set.contains(&binding) {
        return AliasCopyDecision::DropDeadBinding { binding };
    }
    if source_ownership == OwnershipClass::RcManaged
        && binding_ownership == OwnershipClass::RcManaged
        && last_use_set.contains(&source)
    {
        return AliasCopyDecision::MoveSource { source };
    }
    if source_ownership == OwnershipClass::RcManaged {
        AliasCopyDecision::RetainCopy { source }
    } else {
        AliasCopyDecision::Noop
    }
}

fn collect_call_arg_var_counts(
    program: &CoreProgram,
    expr_id: ExprId,
    out: &mut HashMap<VarId, u8>,
) {
    let Some(expr) = program.expr(expr_id) else {
        return;
    };
    match &expr.kind {
        ExprKind::PureCall { args, .. } => {
            for arg in args {
                if let Some(ExprKind::Var(var)) = program.expr(*arg).map(|expr| &expr.kind) {
                    bump_var_count(out, *var);
                }
                collect_call_arg_var_counts(program, *arg, out);
            }
        }
        ExprKind::Unary { expr, .. } => collect_call_arg_var_counts(program, *expr, out),
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_call_arg_var_counts(program, *lhs, out);
            collect_call_arg_var_counts(program, *rhs, out);
        }
        ExprKind::MakeStruct { fields, .. } | ExprKind::MakeEnum { fields, .. } => {
            for field in fields {
                collect_call_arg_var_counts(program, *field, out);
            }
        }
        ExprKind::Var(_) | ExprKind::Literal(_) | ExprKind::Error(_) => {}
    }
}

fn collect_expr_var_counts(program: &CoreProgram, expr_id: ExprId, out: &mut HashMap<VarId, u8>) {
    let Some(expr) = program.expr(expr_id) else {
        return;
    };
    match &expr.kind {
        ExprKind::Var(var) => {
            bump_var_count(out, *var);
        }
        ExprKind::Unary { expr, .. } => collect_expr_var_counts(program, *expr, out),
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_expr_var_counts(program, *lhs, out);
            collect_expr_var_counts(program, *rhs, out);
        }
        ExprKind::PureCall { args, .. }
        | ExprKind::MakeStruct { fields: args, .. }
        | ExprKind::MakeEnum { fields: args, .. } => {
            for arg in args {
                collect_expr_var_counts(program, *arg, out);
            }
        }
        ExprKind::Literal(_) | ExprKind::Error(_) => {}
    }
}

fn bump_var_count(out: &mut HashMap<VarId, u8>, var: VarId) {
    let count = out.entry(var).or_insert(0);
    *count = count.saturating_add(1);
}

fn build_alias_graph(alias: &ArcAliasTables) -> HashMap<VarId, Vec<VarId>> {
    let mut out = HashMap::<VarId, Vec<VarId>>::new();
    for (lhs, rhs) in &alias.copy_alias_edges {
        push_alias_neighbor(&mut out, *lhs, *rhs);
        push_alias_neighbor(&mut out, *rhs, *lhs);
    }
    out
}

fn push_alias_neighbor(out: &mut HashMap<VarId, Vec<VarId>>, var: VarId, neighbor: VarId) {
    let neighbors = out.entry(var).or_default();
    if !neighbors.contains(&neighbor) {
        neighbors.push(neighbor);
    }
}

fn has_live_alias_after_stmt(
    var: VarId,
    live_out_set: &HashSet<VarId>,
    alias_graph: &HashMap<VarId, Vec<VarId>>,
    sema: &SemanticTables,
) -> bool {
    let mut stack = vec![var];
    let mut seen = HashSet::new();
    seen.insert(var);
    while let Some(current) = stack.pop() {
        let Some(neighbors) = alias_graph.get(&current) else {
            continue;
        };
        for neighbor in neighbors {
            if !seen.insert(*neighbor) {
                continue;
            }
            if *neighbor != var
                && live_out_set.contains(neighbor)
                && ownership_of_var(sema, *neighbor) == OwnershipClass::RcManaged
            {
                return true;
            }
            stack.push(*neighbor);
        }
    }
    false
}

fn push_op(plan: &mut ArcInsertionPlan, op: ArcPlannedOp) {
    if plan.ops.contains(&op) {
        return;
    }
    match op.kind {
        ArcOpKind::Retain { .. } => plan.stats.retain_ops = plan.stats.retain_ops.saturating_add(1),
        ArcOpKind::Release { .. } => {
            plan.stats.release_ops = plan.stats.release_ops.saturating_add(1)
        }
    }
    plan.ops.push(op);
    plan.ops.sort_by_key(|entry| {
        (
            entry.stmt.index(),
            arc_kind_order(entry.kind),
            arc_var(entry.kind).index(),
        )
    });
}

fn arc_kind_order(kind: ArcOpKind) -> u8 {
    match kind {
        ArcOpKind::Retain { .. } => 0,
        ArcOpKind::Release { .. } => 1,
    }
}

fn arc_var(kind: ArcOpKind) -> VarId {
    match kind {
        ArcOpKind::Retain { var } | ArcOpKind::Release { var } => var,
    }
}

pub fn to_residual_plan(plan: &ArcInsertionPlan) -> ArcResidualPlan {
    ArcResidualPlan {
        ops: plan
            .ops
            .iter()
            .map(|op| ArcResidualOp {
                stmt: op.stmt,
                kind: match op.kind {
                    ArcOpKind::Retain { var } => ArcResidualOpKind::Retain { var },
                    ArcOpKind::Release { var } => ArcResidualOpKind::Release { var },
                },
            })
            .collect(),
    }
}
