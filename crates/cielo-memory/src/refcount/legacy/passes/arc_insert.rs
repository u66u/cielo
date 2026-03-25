use std::collections::{HashMap, HashSet};

use crate::analysis::arc_alias::ArcAliasTables;
use crate::analysis::arc_cfg::ArcCfg;
use crate::analysis::arc_last_use::ArcLastUseTables;
use crate::common::gc::ArcInsertRule;
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
    pub decision_trace: Vec<ArcDecisionTraceEntry>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArcDecisionSite {
    CallArg { var: VarId },
    AliasCopy { source: VarId, binding: VarId },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArcDecisionOutcome {
    Noop,
    MoveToCallee,
    RetainCopy,
    DropDeadBinding,
    MoveSource,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArcDecisionReason {
    NotManaged,
    MoveRuleDisabled,
    MultipleCallArgUses,
    MultipleStmtUses,
    NotLastUse,
    LiveAliasOut,
    EligibleMove,
    DeadBindingDrop,
    MoveSourceLastUse,
    AliasRuleDisabledFallback,
    RetainCopyFallback,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ArcDecisionTraceEntry {
    pub stmt: StmtId,
    pub site: ArcDecisionSite,
    pub outcome: ArcDecisionOutcome,
    pub reason: ArcDecisionReason,
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StmtVarUseContext {
    CallArgDirect,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct StmtVarUseEvent {
    var: VarId,
    context: StmtVarUseContext,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct CallArgUseAfter {
    has_non_call_use_before_call: bool,
    has_use_after_call: bool,
    has_non_call_use_after_call: bool,
}

pub fn plan(program: &CoreProgram, sema: &SemanticTables) -> ArcInsertionPlan {
    plan_with_rules(program, sema, ArcInsertRule::all())
}

pub fn plan_with_rules(
    program: &CoreProgram,
    sema: &SemanticTables,
    rules: ArcInsertRule,
) -> ArcInsertionPlan {
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
        let mut moved_call_args_with_keepalive = HashSet::new();
        let mut moved_alias_sources = HashSet::new();
        let mut dropped_alias_bindings = HashSet::new();
        let mut stmt_var_use_events = Vec::<StmtVarUseEvent>::new();
        collect_stmt_var_use_events(program, stmt, &mut stmt_var_use_events);

        let mut call_arg_uses = HashMap::new();
        let mut first_call_arg_event = HashMap::<VarId, usize>::new();
        for (idx, event) in stmt_var_use_events.iter().enumerate() {
            if event.context == StmtVarUseContext::CallArgDirect {
                bump_var_count(&mut call_arg_uses, event.var);
                first_call_arg_event.entry(event.var).or_insert(idx);
            }
        }
        let mut use_after_call = HashMap::<VarId, CallArgUseAfter>::new();
        for (var, first_idx) in first_call_arg_event {
            let mut facts = CallArgUseAfter::default();
            for event in stmt_var_use_events.iter().take(first_idx) {
                if event.var == var && event.context == StmtVarUseContext::Other {
                    facts.has_non_call_use_before_call = true;
                }
            }
            for event in stmt_var_use_events.iter().skip(first_idx.saturating_add(1)) {
                if event.var != var {
                    continue;
                }
                facts.has_use_after_call = true;
                if event.context == StmtVarUseContext::Other {
                    facts.has_non_call_use_after_call = true;
                }
            }
            use_after_call.insert(var, facts);
        }

        for (var, use_count) in call_arg_uses {
            let ownership = ownership_of_var(sema, var);
            if ownership != OwnershipClass::RcManaged {
                push_decision(
                    &mut plan,
                    ArcDecisionTraceEntry {
                        stmt: stmt_id,
                        site: ArcDecisionSite::CallArg { var },
                        outcome: ArcDecisionOutcome::Noop,
                        reason: ArcDecisionReason::NotManaged,
                    },
                );
                continue;
            }
            let has_live_alias = has_live_alias_after_stmt(var, &live_out_set, &alias_graph, sema);
            let use_after = use_after_call.get(&var).copied().unwrap_or_default();
            let call_decision = decide_call_arg_transfer(
                rules,
                ownership,
                use_count,
                use_after.has_use_after_call,
                use_after.has_non_call_use_after_call,
                last_use_set.contains(&var),
                has_live_alias,
            );
            push_decision(
                &mut plan,
                ArcDecisionTraceEntry {
                    stmt: stmt_id,
                    site: ArcDecisionSite::CallArg { var },
                    outcome: match call_decision.outcome {
                        CallArgTransferDecision::MoveToCallee => ArcDecisionOutcome::MoveToCallee,
                        CallArgTransferDecision::RetainCopy => ArcDecisionOutcome::RetainCopy,
                    },
                    reason: call_decision.reason,
                },
            );
            match call_decision.outcome {
                CallArgTransferDecision::MoveToCallee => {
                    moved_call_args.insert(var);
                    if use_after.has_non_call_use_before_call {
                        moved_call_args_with_keepalive.insert(var);
                        push_op(
                            &mut plan,
                            ArcPlannedOp {
                                stmt: stmt_id,
                                kind: ArcOpKind::Retain { var },
                            },
                        );
                    }
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
            let alias_decision = decide_alias_copy(
                rules,
                source,
                *binding,
                source_ownership,
                binding_ownership,
                &last_use_set,
            );
            push_decision(
                &mut plan,
                ArcDecisionTraceEntry {
                    stmt: stmt_id,
                    site: ArcDecisionSite::AliasCopy {
                        source,
                        binding: *binding,
                    },
                    outcome: match alias_decision.outcome {
                        AliasCopyDecision::Noop => ArcDecisionOutcome::Noop,
                        AliasCopyDecision::DropDeadBinding { .. } => {
                            ArcDecisionOutcome::DropDeadBinding
                        }
                        AliasCopyDecision::MoveSource { .. } => ArcDecisionOutcome::MoveSource,
                        AliasCopyDecision::RetainCopy { .. } => ArcDecisionOutcome::RetainCopy,
                    },
                    reason: alias_decision.reason,
                },
            );
            match alias_decision.outcome {
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
            if moved_alias_sources.contains(var) || dropped_alias_bindings.contains(var) {
                continue;
            }
            if moved_call_args.contains(var) && !moved_call_args_with_keepalive.contains(var) {
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct CallArgDecision {
    outcome: CallArgTransferDecision,
    reason: ArcDecisionReason,
}

fn decide_call_arg_transfer(
    rules: ArcInsertRule,
    ownership: OwnershipClass,
    call_arg_uses: u8,
    has_use_after_call: bool,
    has_non_call_use_after_call: bool,
    is_last_use: bool,
    has_live_alias: bool,
) -> CallArgDecision {
    if ownership != OwnershipClass::RcManaged {
        return CallArgDecision {
            outcome: CallArgTransferDecision::RetainCopy,
            reason: ArcDecisionReason::NotManaged,
        };
    }
    if !rules.contains(ArcInsertRule::CALL_ARG_LAST_USE_MOVE) {
        return CallArgDecision {
            outcome: CallArgTransferDecision::RetainCopy,
            reason: ArcDecisionReason::MoveRuleDisabled,
        };
    }
    if call_arg_uses != 1 {
        return CallArgDecision {
            outcome: CallArgTransferDecision::RetainCopy,
            reason: ArcDecisionReason::MultipleCallArgUses,
        };
    }
    if has_use_after_call {
        return CallArgDecision {
            outcome: CallArgTransferDecision::RetainCopy,
            reason: if has_non_call_use_after_call {
                ArcDecisionReason::MultipleStmtUses
            } else {
                ArcDecisionReason::NotLastUse
            },
        };
    }
    if !is_last_use {
        return CallArgDecision {
            outcome: CallArgTransferDecision::RetainCopy,
            reason: ArcDecisionReason::NotLastUse,
        };
    }
    if rules.contains(ArcInsertRule::CALL_ARG_ALIAS_LIVE_OUT_GUARD) && has_live_alias {
        return CallArgDecision {
            outcome: CallArgTransferDecision::RetainCopy,
            reason: ArcDecisionReason::LiveAliasOut,
        };
    }
    CallArgDecision {
        outcome: CallArgTransferDecision::MoveToCallee,
        reason: ArcDecisionReason::EligibleMove,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct AliasCopyResult {
    outcome: AliasCopyDecision,
    reason: ArcDecisionReason,
}

fn decide_alias_copy(
    rules: ArcInsertRule,
    source: VarId,
    binding: VarId,
    source_ownership: OwnershipClass,
    binding_ownership: OwnershipClass,
    last_use_set: &HashSet<VarId>,
) -> AliasCopyResult {
    let source_managed = source_ownership == OwnershipClass::RcManaged;
    let binding_managed = binding_ownership == OwnershipClass::RcManaged;
    if !source_managed && !binding_managed {
        return AliasCopyResult {
            outcome: AliasCopyDecision::Noop,
            reason: ArcDecisionReason::NotManaged,
        };
    }

    let dead_binding = binding_managed && last_use_set.contains(&binding);
    if dead_binding {
        if rules.contains(ArcInsertRule::ALIAS_COPY_DROP_DEAD_BINDING) {
            return AliasCopyResult {
                outcome: AliasCopyDecision::DropDeadBinding { binding },
                reason: ArcDecisionReason::DeadBindingDrop,
            };
        }
        if source_managed {
            return AliasCopyResult {
                outcome: AliasCopyDecision::RetainCopy { source },
                reason: ArcDecisionReason::AliasRuleDisabledFallback,
            };
        }
        return AliasCopyResult {
            outcome: AliasCopyDecision::Noop,
            reason: ArcDecisionReason::AliasRuleDisabledFallback,
        };
    }
    let move_source_candidate = source_managed && binding_managed && last_use_set.contains(&source);
    if move_source_candidate {
        if rules.contains(ArcInsertRule::ALIAS_COPY_MOVE_SOURCE) {
            return AliasCopyResult {
                outcome: AliasCopyDecision::MoveSource { source },
                reason: ArcDecisionReason::MoveSourceLastUse,
            };
        }
        return AliasCopyResult {
            outcome: AliasCopyDecision::RetainCopy { source },
            reason: ArcDecisionReason::AliasRuleDisabledFallback,
        };
    }
    if source_managed {
        AliasCopyResult {
            outcome: AliasCopyDecision::RetainCopy { source },
            reason: ArcDecisionReason::RetainCopyFallback,
        }
    } else {
        AliasCopyResult {
            outcome: AliasCopyDecision::Noop,
            reason: ArcDecisionReason::NotManaged,
        }
    }
}

fn collect_stmt_var_use_events(
    program: &CoreProgram,
    stmt: &crate::ir::core::StmtNode,
    out: &mut Vec<StmtVarUseEvent>,
) {
    if let StmtKind::Call { args, .. } = &stmt.kind {
        for arg in args {
            collect_call_arg_var_use_events(program, *arg, out);
        }
        return;
    }
    for expr_id in stmt.child_exprs() {
        collect_expr_var_use_events(program, expr_id, out);
    }
}

fn collect_call_arg_var_use_events(
    program: &CoreProgram,
    expr_id: ExprId,
    out: &mut Vec<StmtVarUseEvent>,
) {
    let Some(expr) = program.expr(expr_id) else {
        return;
    };
    if let ExprKind::Var(var) = expr.kind {
        out.push(StmtVarUseEvent {
            var,
            context: StmtVarUseContext::CallArgDirect,
        });
        return;
    }
    collect_expr_var_use_events(program, expr_id, out);
}

fn collect_expr_var_use_events(
    program: &CoreProgram,
    expr_id: ExprId,
    out: &mut Vec<StmtVarUseEvent>,
) {
    let Some(expr) = program.expr(expr_id) else {
        return;
    };
    match &expr.kind {
        ExprKind::Var(var) => {
            out.push(StmtVarUseEvent {
                var: *var,
                context: StmtVarUseContext::Other,
            });
        }
        ExprKind::Unary { expr, .. } => collect_expr_var_use_events(program, *expr, out),
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_expr_var_use_events(program, *lhs, out);
            collect_expr_var_use_events(program, *rhs, out);
        }
        ExprKind::PureCall { args, .. } => {
            for arg in args {
                collect_call_arg_var_use_events(program, *arg, out);
            }
        }
        ExprKind::MakeStruct { fields: args, .. } | ExprKind::MakeEnum { fields: args, .. } => {
            for arg in args {
                collect_expr_var_use_events(program, *arg, out);
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

fn push_decision(plan: &mut ArcInsertionPlan, entry: ArcDecisionTraceEntry) {
    plan.decision_trace.push(entry);
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
