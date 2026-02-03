// Pass 7/9: handler_specialize (bounded handler-call specialization groundwork)
//
// Inputs:
// - Residualized Core program
//
// Outputs:
// - Residualized Core program with specialized function copies for handle-wrapped direct calls
//
// Invariants:
// - A pair (callee, handler-shape) specializes at most once
// - Specialized functions preserve signatures and clone the original body graph
// - Recursive calls in specialized copies are retargeted to the specialized function id
// - Direct `handle { f(...) }` callsites are rewritten to direct calls to the specialized copy
// - Unreachable unspecialized copies are pruned after rewrite
//
// Complexity:
// - O(stmt_count + cloned_nodes + reachable_call_graph)

use crate::analysis::function_graph::{collect_reachable_functions, prune_unreachable_functions};
use std::collections::{HashMap, HashSet};

use crate::common::ids::{EffectLabelId, ExprId, FuncId, HandlerId, StmtId, SymbolId, VarId};
use crate::ir::core::{
    CoreProgram, ExprKind, ExprNode, FunctionDecl, HandlerDef, Literal, StmtKind, StmtNode,
};
use crate::pipeline::phases::{
    BtaTables, Reason, Residualized, SemanticTables, SpecializationStats, Stage,
};

const MAX_SPECIALIZATIONS_PER_CALLEE: usize = 8;
const MAX_TOTAL_SPECIALIZATIONS: usize = 256;

pub fn run(residual: Residualized) -> Residualized {
    let (mut program, diagnostics, mut sema, mut mono, ct, mut bta, mut residual_tables) =
        residual.into_parts();

    residual_tables.specialization_stats =
        specialize_handle_wrapped_calls(&mut program, &mut bta, &mut sema);

    let func_remap = prune_unreachable_functions(&mut program);
    mono.remap_func_ids(&func_remap);
    bta.remap_func_ids(&func_remap);
    residual_tables.remap_func_ids(&func_remap);
    assert_remap_integrity(&program, &mono, &bta, &residual_tables);
    Residualized::new(program, diagnostics, sema, mono, ct, bta, residual_tables)
}

#[derive(Clone)]
struct SpecializeCandidate {
    handler: HandlerId,
    shape: HandlerShapeKey,
    handle_stmt: StmtId,
    body_stmt: StmtId,
    callee: FuncId,
}

fn specialize_handle_wrapped_calls(
    program: &mut CoreProgram,
    bta: &mut BtaTables,
    sema: &mut SemanticTables,
) -> SpecializationStats {
    let candidates = collect_specialize_candidates(program);
    let mut specialized: HashMap<(FuncId, HandlerShapeKey), FuncId> = HashMap::new();
    let mut specialized_count_by_callee: HashMap<FuncId, usize> = HashMap::new();
    let mut total_specialized = 0usize;
    let mut stats = SpecializationStats::default();

    for candidate in candidates {
        stats.candidates_seen = stats.candidates_seen.saturating_add(1);
        let Some(specialized_callee) = ensure_specialized(
            program,
            bta,
            sema,
            &candidate,
            &mut specialized,
            &mut specialized_count_by_callee,
            &mut total_specialized,
            &mut stats,
        ) else {
            continue;
        };
        if rewrite_direct_handle_callsite(program, &candidate, specialized_callee) {
            stats.rewrites = stats.rewrites.saturating_add(1);
        }
    }
    stats
}

fn collect_specialize_candidates(program: &CoreProgram) -> Vec<SpecializeCandidate> {
    let mut out = Vec::new();
    let mut seen_stmts = HashSet::new();
    let mut shape_cache = HashMap::new();
    let mut wrapper_callee_cache = HashMap::new();
    let mut wrapper_callee_visiting = HashSet::new();
    for func_id in collect_reachable_functions(program) {
        let Some(function) = program.function(func_id) else {
            continue;
        };
        let mut stack = vec![function.body];
        while let Some(stmt_id) = stack.pop() {
            if !seen_stmts.insert(stmt_id) {
                continue;
            }
            let Some(stmt) = program.stmt(stmt_id) else {
                continue;
            };
            if let StmtKind::Handle {
                handler,
                body,
                next,
            } = &stmt.kind
                && next.is_none()
                && let Some(callee) = wrapper_call_callee(
                    program,
                    *body,
                    &mut wrapper_callee_cache,
                    &mut wrapper_callee_visiting,
                )
                && let Some(shape) = shape_for_handler(program, *handler, &mut shape_cache)
            {
                out.push(SpecializeCandidate {
                    handler: *handler,
                    shape,
                    handle_stmt: stmt_id,
                    body_stmt: *body,
                    callee,
                });
            }
            stack.extend(stmt.child_stmts());
        }
    }
    out
}

fn wrapper_call_callee(
    program: &CoreProgram,
    stmt_id: StmtId,
    cache: &mut HashMap<StmtId, Option<FuncId>>,
    visiting: &mut HashSet<StmtId>,
) -> Option<FuncId> {
    if let Some(cached) = cache.get(&stmt_id).copied() {
        return cached;
    }
    // cycle => not a valid direct wrapper shape in v1.
    if !visiting.insert(stmt_id) {
        cache.insert(stmt_id, None);
        return None;
    }

    let resolved = match program.stmt(stmt_id) {
        Some(stmt) => match &stmt.kind {
            StmtKind::Call { callee, .. } => Some(*callee),
            StmtKind::Let { next, .. } => wrapper_call_callee(program, *next, cache, visiting),
            StmtKind::Val {
                binding,
                value,
                next,
            } if is_forwarding_tail_of_var(program, *next, *binding) => {
                wrapper_call_callee(program, *value, cache, visiting)
            }
            StmtKind::If {
                then_branch,
                else_branch,
                ..
            } => same_callee(
                wrapper_call_callee(program, *then_branch, cache, visiting),
                wrapper_call_callee(program, *else_branch, cache, visiting),
            ),
            StmtKind::Match { arms, default, .. } => {
                let mut callee = None;
                for arm in arms {
                    let Some(arm_callee) = wrapper_call_callee(program, arm.body, cache, visiting)
                    else {
                        return finish_wrapper_callee(cache, visiting, stmt_id, None);
                    };
                    callee = merge_wrapper_callee(callee, arm_callee);
                    if callee.is_none() {
                        return finish_wrapper_callee(cache, visiting, stmt_id, None);
                    }
                }
                if let Some(default_stmt) = default {
                    let Some(default_callee) =
                        wrapper_call_callee(program, *default_stmt, cache, visiting)
                    else {
                        return finish_wrapper_callee(cache, visiting, stmt_id, None);
                    };
                    callee = merge_wrapper_callee(callee, default_callee);
                }
                callee
            }
            StmtKind::Stage { body, next, .. } => {
                let body_callee = wrapper_call_callee(program, *body, cache, visiting)?;
                let mut callee = Some(body_callee);
                if let Some(next_stmt) = next {
                    let next_callee = wrapper_call_callee(program, *next_stmt, cache, visiting)?;
                    callee = merge_wrapper_callee(callee, next_callee);
                }
                callee
            }
            _ => None,
        },
        None => None,
    };
    finish_wrapper_callee(cache, visiting, stmt_id, resolved)
}

fn finish_wrapper_callee(
    cache: &mut HashMap<StmtId, Option<FuncId>>,
    visiting: &mut HashSet<StmtId>,
    stmt_id: StmtId,
    resolved: Option<FuncId>,
) -> Option<FuncId> {
    visiting.remove(&stmt_id);
    cache.insert(stmt_id, resolved);
    resolved
}

fn same_callee(lhs: Option<FuncId>, rhs: Option<FuncId>) -> Option<FuncId> {
    let lhs = lhs?;
    let rhs = rhs?;
    if lhs == rhs { Some(lhs) } else { None }
}

fn merge_wrapper_callee(current: Option<FuncId>, next: FuncId) -> Option<FuncId> {
    match current {
        Some(existing) if existing != next => None,
        Some(existing) => Some(existing),
        None => Some(next),
    }
}

fn is_forwarding_tail_of_var(program: &CoreProgram, stmt_id: StmtId, source_var: VarId) -> bool {
    fn recurse(
        program: &CoreProgram,
        stmt_id: StmtId,
        source_var: VarId,
        visiting: &mut HashSet<StmtId>,
    ) -> bool {
        if !visiting.insert(stmt_id) {
            return false;
        }
        let result = match program.stmt(stmt_id).map(|stmt| &stmt.kind) {
            Some(StmtKind::Return(expr_id)) => program.expr(*expr_id).is_some_and(
                |expr| matches!(expr.kind, ExprKind::Var(bound) if bound == source_var),
            ),
            Some(StmtKind::Let {
                binding,
                value,
                next,
            }) => {
                program.expr(*value).is_some_and(
                    |expr| matches!(expr.kind, ExprKind::Var(var) if var == source_var),
                ) && recurse(program, *next, *binding, visiting)
            }
            Some(StmtKind::Val {
                binding,
                value,
                next,
            }) => {
                recurse(program, *value, source_var, visiting)
                    && recurse(program, *next, *binding, visiting)
            }
            Some(StmtKind::If {
                then_branch,
                else_branch,
                ..
            }) => {
                recurse(program, *then_branch, source_var, visiting)
                    && recurse(program, *else_branch, source_var, visiting)
            }
            Some(StmtKind::Match { arms, default, .. }) => {
                let Some(default_stmt) = default else {
                    return false;
                };
                arms.iter()
                    .all(|arm| recurse(program, arm.body, source_var, visiting))
                    && recurse(program, *default_stmt, source_var, visiting)
            }
            Some(StmtKind::Stage { body, next, .. }) => {
                recurse(program, *body, source_var, visiting)
                    && next.as_ref().map_or(true, |next_stmt| {
                        recurse(program, *next_stmt, source_var, visiting)
                    })
            }
            _ => false,
        };
        visiting.remove(&stmt_id);
        result
    }

    let mut visiting = HashSet::new();
    recurse(program, stmt_id, source_var, &mut visiting)
}

fn shape_for_handler(
    program: &CoreProgram,
    handler_id: HandlerId,
    cache: &mut HashMap<HandlerId, HandlerShapeKey>,
) -> Option<HandlerShapeKey> {
    if let Some(shape) = cache.get(&handler_id).cloned() {
        return Some(shape);
    }
    let handler = program.handlers().get(handler_id.index())?;
    let shape = HandlerShapeKey::build(program, handler);
    cache.insert(handler_id, shape.clone());
    Some(shape)
}

fn rewrite_direct_handle_callsite(
    program: &mut CoreProgram,
    candidate: &SpecializeCandidate,
    specialized_callee: FuncId,
) -> bool {
    let mut rewritten_cache = HashMap::new();
    let mut rewritten_visiting = HashSet::new();
    let Some(replacement) = build_rewritten_body(
        program,
        candidate.body_stmt,
        specialized_callee,
        &mut rewritten_cache,
        &mut rewritten_visiting,
    ) else {
        return false;
    };

    let Some(handle_stmt) = program.stmt_mut(candidate.handle_stmt) else {
        return false;
    };
    if matches!(handle_stmt.kind, StmtKind::Handle { next: None, .. }) {
        handle_stmt.kind = replacement;
        return true;
    }
    false
}

fn build_rewritten_body(
    program: &mut CoreProgram,
    body_stmt: StmtId,
    specialized_callee: FuncId,
    cache: &mut HashMap<StmtId, Option<StmtId>>,
    visiting: &mut HashSet<StmtId>,
) -> Option<StmtKind> {
    let stmt = program.stmt(body_stmt)?.clone();
    match stmt.kind {
        StmtKind::Call {
            result,
            args,
            effects,
            next,
            ..
        } => Some(StmtKind::Call {
            result,
            callee: specialized_callee,
            args,
            effects,
            next,
        }),
        StmtKind::Let {
            binding,
            value,
            next,
        } => {
            let rewritten_next =
                build_rewritten_stmt(program, next, specialized_callee, cache, visiting)?;
            Some(StmtKind::Let {
                binding,
                value,
                next: rewritten_next,
            })
        }
        StmtKind::Val {
            binding,
            value,
            next,
        } if is_forwarding_tail_of_var(program, next, binding) => {
            let rewritten_call =
                build_rewritten_stmt(program, value, specialized_callee, cache, visiting)?;
            Some(StmtKind::Val {
                binding,
                value: rewritten_call,
                next,
            })
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            let rewritten_then =
                build_rewritten_stmt(program, then_branch, specialized_callee, cache, visiting)?;
            let rewritten_else =
                build_rewritten_stmt(program, else_branch, specialized_callee, cache, visiting)?;
            Some(StmtKind::If {
                cond,
                then_branch: rewritten_then,
                else_branch: rewritten_else,
            })
        }
        StmtKind::Match {
            scrutinee,
            arms,
            default,
        } => {
            let mut rewritten_arms = Vec::with_capacity(arms.len());
            for arm in arms {
                rewritten_arms.push(crate::ir::core::MatchArm {
                    tag: arm.tag,
                    binders: arm.binders,
                    body: build_rewritten_stmt(
                        program,
                        arm.body,
                        specialized_callee,
                        cache,
                        visiting,
                    )?,
                    span: arm.span,
                });
            }
            let rewritten_default = if let Some(default_stmt) = default {
                Some(build_rewritten_stmt(
                    program,
                    default_stmt,
                    specialized_callee,
                    cache,
                    visiting,
                )?)
            } else {
                None
            };
            Some(StmtKind::Match {
                scrutinee,
                arms: rewritten_arms,
                default: rewritten_default,
            })
        }
        StmtKind::Stage { stage, body, next } => {
            let rewritten_body =
                build_rewritten_stmt(program, body, specialized_callee, cache, visiting)?;
            let rewritten_next = if let Some(next_stmt) = next {
                Some(build_rewritten_stmt(
                    program,
                    next_stmt,
                    specialized_callee,
                    cache,
                    visiting,
                )?)
            } else {
                None
            };
            Some(StmtKind::Stage {
                stage,
                body: rewritten_body,
                next: rewritten_next,
            })
        }
        _ => None,
    }
}

fn build_rewritten_stmt(
    program: &mut CoreProgram,
    stmt_id: StmtId,
    specialized_callee: FuncId,
    cache: &mut HashMap<StmtId, Option<StmtId>>,
    visiting: &mut HashSet<StmtId>,
) -> Option<StmtId> {
    if let Some(cached) = cache.get(&stmt_id).copied() {
        return cached;
    }
    if !visiting.insert(stmt_id) {
        cache.insert(stmt_id, None);
        return None;
    }

    let resolved = match program.stmt(stmt_id) {
        Some(stmt) => {
            let span = stmt.span;
            let kind = build_rewritten_body(program, stmt_id, specialized_callee, cache, visiting)?;
            Some(program.push_stmt(StmtNode { span, kind }))
        }
        None => None,
    };
    visiting.remove(&stmt_id);
    cache.insert(stmt_id, resolved);
    resolved
}

fn ensure_specialized(
    program: &mut CoreProgram,
    bta: &mut BtaTables,
    sema: &mut SemanticTables,
    candidate: &SpecializeCandidate,
    specialized: &mut HashMap<(FuncId, HandlerShapeKey), FuncId>,
    specialized_count_by_callee: &mut HashMap<FuncId, usize>,
    total_specialized: &mut usize,
    stats: &mut SpecializationStats,
) -> Option<FuncId> {
    let key = (candidate.callee, candidate.shape.clone());
    if let Some(existing) = specialized.get(&key).copied() {
        stats.reused_existing = stats.reused_existing.saturating_add(1);
        return Some(existing);
    }
    // v1 guard: mixed recursive wrapper shapes stay unspecialized.
    if has_varying_recursive_wrapper_shapes(program, candidate.callee, &candidate.shape) {
        stats.skipped_varying_shapes = stats.skipped_varying_shapes.saturating_add(1);
        return None;
    }
    if *total_specialized >= MAX_TOTAL_SPECIALIZATIONS {
        stats.skipped_limits = stats.skipped_limits.saturating_add(1);
        return None;
    }
    if specialized_count_by_callee
        .get(&candidate.callee)
        .copied()
        .unwrap_or(0)
        >= MAX_SPECIALIZATIONS_PER_CALLEE
    {
        stats.skipped_limits = stats.skipped_limits.saturating_add(1);
        return None;
    }

    let Some(source_decl) = program.function(candidate.callee).cloned() else {
        return None;
    };

    // Add a placeholder copy first to obtain the stable specialized FuncId.
    let placeholder = FunctionDecl {
        body: source_decl.body,
        ..source_decl.clone()
    };
    let specialized_id = program.add_function(placeholder);

    let cloned_body = {
        let mut cloner = GraphCloner::new(program, bta, sema, candidate.callee, specialized_id);
        cloner.clone_stmt(source_decl.body)
    };
    let wrapped_body = program.push_stmt(StmtNode {
        span: source_decl.span,
        kind: StmtKind::Handle {
            handler: candidate.handler,
            body: cloned_body,
            next: None,
        },
    });

    if sema.effects_of_stmt.len() <= wrapped_body.index() {
        sema.effects_of_stmt.resize(
            wrapped_body.index() + 1,
            crate::sema::effect::SortedEffectRow::empty(),
        );
    }

    if let Some(function) = program.function_mut(specialized_id) {
        function.body = wrapped_body;
    }

    specialized.insert(key, specialized_id);
    *total_specialized += 1;
    specialized_count_by_callee
        .entry(candidate.callee)
        .and_modify(|count| *count += 1)
        .or_insert(1);
    stats.created = stats.created.saturating_add(1);
    Some(specialized_id)
}

fn has_varying_recursive_wrapper_shapes(
    program: &CoreProgram,
    callee: FuncId,
    expected_shape: &HandlerShapeKey,
) -> bool {
    let Some(function) = program.function(callee) else {
        return false;
    };

    let mut stack = vec![(function.body, None::<HandlerId>)];
    let mut seen_stmts = HashSet::new();
    let mut shape_cache = HashMap::new();

    while let Some((stmt_id, nearest_handler)) = stack.pop() {
        if !seen_stmts.insert((stmt_id, nearest_handler)) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };

        match &stmt.kind {
            StmtKind::Call { callee: called, .. } => {
                if *called == callee
                    && let Some(handler) = nearest_handler
                {
                    let Some(shape) = shape_for_handler(program, handler, &mut shape_cache) else {
                        continue;
                    };
                    if &shape != expected_shape {
                        return true;
                    }
                }
            }
            StmtKind::Handle {
                handler,
                body,
                next,
            } => {
                // body runs under new handler; next keeps prior handler context.
                stack.push((*body, Some(*handler)));
                if let Some(next_stmt) = next {
                    stack.push((*next_stmt, nearest_handler));
                }
                continue;
            }
            _ => {}
        }

        for child in stmt.child_stmts() {
            stack.push((child, nearest_handler));
        }
    }

    false
}

fn assert_remap_integrity(
    program: &CoreProgram,
    mono: &crate::pipeline::phases::MonomorphizationSummary,
    bta: &crate::pipeline::phases::BtaTables,
    residual: &crate::pipeline::phases::ResidualTables,
) {
    let function_count = program.functions().len();
    for (source, monos) in &mono.source_to_mono {
        assert_func_id_in_bounds(*source, function_count, "monomorphization source");
        for mono_id in monos {
            assert_func_id_in_bounds(*mono_id, function_count, "monomorphization instance");
        }
    }

    for stage in bta.stage_of_expr.values() {
        assert_stage_reason_in_bounds(*stage, function_count, "bta.stage_of_expr");
    }
    for stage in bta.stage_of_var.values() {
        assert_stage_reason_in_bounds(*stage, function_count, "bta.stage_of_var");
    }
    for discharge in bta.handler_discharge.values() {
        if let Some(reason) = discharge.reason {
            assert_reason_in_bounds(reason, function_count, "bta.handler_discharge");
        }
    }
    for clauses in bta.clause_discharge.values() {
        for clause in clauses {
            if let Some(reason) = clause.reason {
                assert_reason_in_bounds(reason, function_count, "bta.clause_discharge");
            }
        }
    }

    for func_id in residual.function_effect_summary.keys() {
        assert_func_id_in_bounds(*func_id, function_count, "residual function_effect_summary");
    }
    assert!(
        residual.specialization_stats.created <= residual.specialization_stats.candidates_seen,
        "compiler bug: specialization created count exceeds candidates seen"
    );
    assert!(
        residual.specialization_stats.rewrites <= residual.specialization_stats.candidates_seen,
        "compiler bug: specialization rewrite count exceeds candidates seen"
    );
}

fn assert_stage_reason_in_bounds(stage: Stage, function_count: usize, context: &str) {
    if let Stage::Rt(reason) = stage {
        assert_reason_in_bounds(reason, function_count, context);
    }
}

fn assert_reason_in_bounds(reason: Reason, function_count: usize, context: &str) {
    match reason {
        Reason::Parameter { func, .. } | Reason::CtOnlyWithRuntimeArgs(func) => {
            assert_func_id_in_bounds(func, function_count, context);
        }
        Reason::UnclassifiedRuntime
        | Reason::DependsOnVar(_)
        | Reason::EffectNotDischarged(_)
        | Reason::HandlerIsRuntime(_)
        | Reason::BranchOnRuntime(_)
        | Reason::NotPersistable(_)
        | Reason::UserForcedRuntime => {}
    }
}

fn assert_func_id_in_bounds(func_id: FuncId, function_count: usize, context: &str) {
    assert!(
        func_id.index() < function_count,
        "compiler bug: {context} references out-of-bounds function f{} (functions={function_count})",
        func_id.as_u32()
    );
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct HandlerShapeKey {
    effect: EffectLabelId,
    return_body: Vec<ShapeToken>,
    clauses: Vec<ClauseShapeKey>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ClauseShapeKey {
    operation: SymbolId,
    param_count: usize,
    has_resume: bool,
    body: Vec<ShapeToken>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum ShapeToken {
    Tag(ShapeTag),
    Count(u32),
    BoundVar(u32),
    FreeVar(u32),
    Func(u32),
    Handler(u32),
    Effect(u32),
    Symbol(u32),
    Type(u32),
    Bool(bool),
    Int(i64),
    FloatBits(u64),
    Char(char),
    String(String),
}

macro_rules! define_shape_tags {
    (
        fixed { $($fixed:ident),* $(,)? }
        unary { $( $uop:path => $uvariant:ident ),* $(,)? }
        binary { $( $bop:path => $bvariant:ident ),* $(,)? }
    ) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash)]
        enum ShapeTag {
            $($fixed,)*
            $($uvariant,)*
            $($bvariant,)*
            LiteralUnit,
        }

        fn unary_shape_tag(op: crate::ir::core::UnaryOp) -> ShapeTag {
            match op {
                $($uop => ShapeTag::$uvariant,)*
            }
        }

        fn binary_shape_tag(op: crate::ir::core::BinaryOp) -> ShapeTag {
            match op {
                $($bop => ShapeTag::$bvariant,)*
            }
        }
    };
}

define_shape_tags! {
    fixed {
        StmtMissing,
        StmtReturn,
        StmtLet,
        StmtVal,
        StmtCall,
        StmtIf,
        StmtMatch,
        MatchArm,
        StmtPerform,
        OptionalResultSome,
        OptionalResultNone,
        OptionalStmtSome,
        OptionalStmtNone,
        StmtResume,
        StmtHandle,
        StmtStage,
        StageComptime,
        StageRuntime,
        StmtHole,
        StmtError,
        ExprMissing,
        ExprVar,
        ExprLiteral,
        ExprUnary,
        ExprBinary,
        ExprPureCall,
        ExprMakeStruct,
        ExprMakeEnum,
        ExprError,
    }
    unary {
        crate::ir::core::UnaryOp::Neg => UnaryNeg,
        crate::ir::core::UnaryOp::Not => UnaryNot,
    }
    binary {
        crate::ir::core::BinaryOp::Add => BinaryAdd,
        crate::ir::core::BinaryOp::Sub => BinarySub,
        crate::ir::core::BinaryOp::Mul => BinaryMul,
        crate::ir::core::BinaryOp::Div => BinaryDiv,
        crate::ir::core::BinaryOp::Mod => BinaryMod,
        crate::ir::core::BinaryOp::Eq => BinaryEq,
        crate::ir::core::BinaryOp::Ne => BinaryNe,
        crate::ir::core::BinaryOp::Lt => BinaryLt,
        crate::ir::core::BinaryOp::Le => BinaryLe,
        crate::ir::core::BinaryOp::Gt => BinaryGt,
        crate::ir::core::BinaryOp::Ge => BinaryGe,
        crate::ir::core::BinaryOp::And => BinaryAnd,
        crate::ir::core::BinaryOp::Or => BinaryOr,
    }
}

impl HandlerShapeKey {
    fn build(program: &CoreProgram, handler: &HandlerDef) -> Self {
        let mut ret_scope = ScopeCanon::default();
        let _ = ret_scope.bind(handler.return_param);
        let mut return_body = Vec::new();
        push_stmt_shape(
            program,
            handler.return_body,
            &mut ret_scope,
            &mut return_body,
        );

        let clauses = handler
            .clauses
            .iter()
            .map(|clause| {
                let mut clause_scope = ScopeCanon::default();
                for param in &clause.params {
                    let _ = clause_scope.bind(*param);
                }
                if let Some(resume) = clause.resume_param {
                    let _ = clause_scope.bind(resume);
                }
                let mut body = Vec::new();
                push_stmt_shape(program, clause.body, &mut clause_scope, &mut body);
                ClauseShapeKey {
                    operation: clause.operation,
                    param_count: clause.params.len(),
                    has_resume: clause.resume_param.is_some(),
                    body,
                }
            })
            .collect();

        Self {
            effect: handler.effect,
            return_body,
            clauses,
        }
    }
}

#[derive(Default)]
struct ScopeCanon {
    map: HashMap<VarId, u32>,
    next: u32,
}

impl ScopeCanon {
    fn bind(&mut self, var: VarId) -> u32 {
        if let Some(existing) = self.map.get(&var).copied() {
            return existing;
        }
        let id = self.next;
        self.next += 1;
        self.map.insert(var, id);
        id
    }
}

fn push_stmt_shape(
    program: &CoreProgram,
    stmt_id: StmtId,
    scope: &mut ScopeCanon,
    out: &mut Vec<ShapeToken>,
) {
    let Some(stmt) = program.stmt(stmt_id) else {
        out.push(ShapeToken::Tag(ShapeTag::StmtMissing));
        return;
    };

    match &stmt.kind {
        StmtKind::Return(expr) => {
            out.push(ShapeToken::Tag(ShapeTag::StmtReturn));
            push_expr_shape(program, *expr, scope, out);
        }
        StmtKind::Let {
            binding,
            value,
            next,
        } => {
            out.push(ShapeToken::Tag(ShapeTag::StmtLet));
            out.push(ShapeToken::BoundVar(scope.bind(*binding)));
            push_expr_shape(program, *value, scope, out);
            push_stmt_shape(program, *next, scope, out);
        }
        StmtKind::Val {
            binding,
            value,
            next,
        } => {
            out.push(ShapeToken::Tag(ShapeTag::StmtVal));
            out.push(ShapeToken::BoundVar(scope.bind(*binding)));
            push_stmt_shape(program, *value, scope, out);
            push_stmt_shape(program, *next, scope, out);
        }
        StmtKind::Call {
            result,
            callee,
            args,
            effects,
            next,
        } => {
            out.push(ShapeToken::Tag(ShapeTag::StmtCall));
            out.push(ShapeToken::Func(callee.as_u32()));
            out.push(ShapeToken::BoundVar(scope.bind(*result)));
            push_count(args.len(), out);
            for arg in args {
                push_expr_shape(program, *arg, scope, out);
            }
            push_count(effects.len(), out);
            for effect in effects.iter() {
                out.push(ShapeToken::Effect(effect.as_u32()));
            }
            push_stmt_shape(program, *next, scope, out);
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            out.push(ShapeToken::Tag(ShapeTag::StmtIf));
            push_expr_shape(program, *cond, scope, out);
            push_stmt_shape(program, *then_branch, scope, out);
            push_stmt_shape(program, *else_branch, scope, out);
        }
        StmtKind::Match {
            scrutinee,
            arms,
            default,
        } => {
            out.push(ShapeToken::Tag(ShapeTag::StmtMatch));
            push_expr_shape(program, *scrutinee, scope, out);
            push_count(arms.len(), out);
            for arm in arms {
                out.push(ShapeToken::Tag(ShapeTag::MatchArm));
                out.push(ShapeToken::Symbol(arm.tag.as_u32()));
                let mut arm_scope = ScopeCanon {
                    map: scope.map.clone(),
                    next: scope.next,
                };
                push_count(arm.binders.len(), out);
                for binder in &arm.binders {
                    out.push(ShapeToken::BoundVar(arm_scope.bind(*binder)));
                }
                push_stmt_shape(program, arm.body, &mut arm_scope, out);
            }
            if let Some(default_stmt) = default {
                out.push(ShapeToken::Tag(ShapeTag::OptionalStmtSome));
                push_stmt_shape(program, *default_stmt, scope, out);
            } else {
                out.push(ShapeToken::Tag(ShapeTag::OptionalStmtNone));
            }
        }
        StmtKind::Perform {
            result,
            effect,
            operation,
            args,
            next,
        } => {
            out.push(ShapeToken::Tag(ShapeTag::StmtPerform));
            out.push(ShapeToken::Effect(effect.as_u32()));
            out.push(ShapeToken::Symbol(operation.as_u32()));
            if let Some(result) = result {
                out.push(ShapeToken::Tag(ShapeTag::OptionalResultSome));
                out.push(ShapeToken::BoundVar(scope.bind(*result)));
            } else {
                out.push(ShapeToken::Tag(ShapeTag::OptionalResultNone));
            }
            push_count(args.len(), out);
            for arg in args {
                push_expr_shape(program, *arg, scope, out);
            }
            push_stmt_shape(program, *next, scope, out);
        }
        StmtKind::Resume {
            result,
            resume,
            arg,
            next,
        } => {
            out.push(ShapeToken::Tag(ShapeTag::StmtResume));
            out.push(ShapeToken::BoundVar(scope.bind(*result)));
            out.push(ShapeToken::FreeVar(resume.as_u32()));
            push_expr_shape(program, *arg, scope, out);
            push_stmt_shape(program, *next, scope, out);
        }
        StmtKind::Handle {
            handler,
            body,
            next,
        } => {
            out.push(ShapeToken::Tag(ShapeTag::StmtHandle));
            out.push(ShapeToken::Handler(handler.as_u32()));
            push_stmt_shape(program, *body, scope, out);
            if let Some(next_stmt) = next {
                out.push(ShapeToken::Tag(ShapeTag::OptionalStmtSome));
                push_stmt_shape(program, *next_stmt, scope, out);
            } else {
                out.push(ShapeToken::Tag(ShapeTag::OptionalStmtNone));
            }
        }
        StmtKind::Stage { stage, body, next } => {
            out.push(ShapeToken::Tag(ShapeTag::StmtStage));
            out.push(ShapeToken::Tag(stage_shape_tag(*stage)));
            push_stmt_shape(program, *body, scope, out);
            if let Some(next_stmt) = next {
                out.push(ShapeToken::Tag(ShapeTag::OptionalStmtSome));
                push_stmt_shape(program, *next_stmt, scope, out);
            } else {
                out.push(ShapeToken::Tag(ShapeTag::OptionalStmtNone));
            }
        }
        StmtKind::Hole { ty } => {
            out.push(ShapeToken::Tag(ShapeTag::StmtHole));
            out.push(ShapeToken::Type(ty.as_u32()));
        }
        StmtKind::Error(_) => out.push(ShapeToken::Tag(ShapeTag::StmtError)),
    }
}

fn push_expr_shape(
    program: &CoreProgram,
    expr_id: ExprId,
    scope: &ScopeCanon,
    out: &mut Vec<ShapeToken>,
) {
    let Some(expr) = program.expr(expr_id) else {
        out.push(ShapeToken::Tag(ShapeTag::ExprMissing));
        return;
    };

    match &expr.kind {
        ExprKind::Var(var) => {
            out.push(ShapeToken::Tag(ShapeTag::ExprVar));
            if let Some(bound) = scope.map.get(var).copied() {
                out.push(ShapeToken::BoundVar(bound));
            } else {
                out.push(ShapeToken::FreeVar(var.as_u32()));
            }
        }
        ExprKind::Literal(literal) => {
            out.push(ShapeToken::Tag(ShapeTag::ExprLiteral));
            push_literal_shape(literal, out);
        }
        ExprKind::Unary { op, expr } => {
            out.push(ShapeToken::Tag(ShapeTag::ExprUnary));
            out.push(ShapeToken::Tag(unary_shape_tag(*op)));
            push_expr_shape(program, *expr, scope, out);
        }
        ExprKind::Binary { op, lhs, rhs } => {
            out.push(ShapeToken::Tag(ShapeTag::ExprBinary));
            out.push(ShapeToken::Tag(binary_shape_tag(*op)));
            push_expr_shape(program, *lhs, scope, out);
            push_expr_shape(program, *rhs, scope, out);
        }
        ExprKind::PureCall { callee, args } => {
            out.push(ShapeToken::Tag(ShapeTag::ExprPureCall));
            out.push(ShapeToken::Func(callee.as_u32()));
            push_count(args.len(), out);
            for arg in args {
                push_expr_shape(program, *arg, scope, out);
            }
        }
        ExprKind::MakeStruct { ty, fields } => {
            out.push(ShapeToken::Tag(ShapeTag::ExprMakeStruct));
            out.push(ShapeToken::Type(ty.as_u32()));
            push_count(fields.len(), out);
            for field in fields {
                push_expr_shape(program, *field, scope, out);
            }
        }
        ExprKind::MakeEnum {
            ty,
            variant,
            fields,
        } => {
            out.push(ShapeToken::Tag(ShapeTag::ExprMakeEnum));
            out.push(ShapeToken::Type(ty.as_u32()));
            out.push(ShapeToken::Symbol(variant.as_u32()));
            push_count(fields.len(), out);
            for field in fields {
                push_expr_shape(program, *field, scope, out);
            }
        }
        ExprKind::Error(_) => out.push(ShapeToken::Tag(ShapeTag::ExprError)),
    }
}

fn push_literal_shape(literal: &Literal, out: &mut Vec<ShapeToken>) {
    match literal {
        Literal::Unit => out.push(ShapeToken::Tag(ShapeTag::LiteralUnit)),
        Literal::Bool(value) => out.push(ShapeToken::Bool(*value)),
        Literal::Int(value) => out.push(ShapeToken::Int(*value)),
        Literal::Float(value) => out.push(ShapeToken::FloatBits(value.to_bits())),
        Literal::Char(value) => out.push(ShapeToken::Char(*value)),
        Literal::String(value) => out.push(ShapeToken::String(value.clone())),
    }
}

fn stage_shape_tag(stage: crate::ir::core::StageDirective) -> ShapeTag {
    match stage {
        crate::ir::core::StageDirective::Comptime => ShapeTag::StageComptime,
        crate::ir::core::StageDirective::Runtime => ShapeTag::StageRuntime,
    }
}

fn push_count(len: usize, out: &mut Vec<ShapeToken>) {
    out.push(ShapeToken::Count(u32::try_from(len).unwrap_or(u32::MAX)));
}

struct GraphCloner<'a> {
    program: &'a mut CoreProgram,
    bta: &'a mut BtaTables,
    sema: &'a mut SemanticTables,
    source_func: FuncId,
    specialized_func: FuncId,
    expr_map: HashMap<ExprId, ExprId>,
    stmt_map: HashMap<StmtId, StmtId>,
}

impl<'a> GraphCloner<'a> {
    fn new(
        program: &'a mut CoreProgram,
        bta: &'a mut BtaTables,
        sema: &'a mut SemanticTables,
        source_func: FuncId,
        specialized_func: FuncId,
    ) -> Self {
        Self {
            program,
            bta,
            sema,
            source_func,
            specialized_func,
            expr_map: HashMap::new(),
            stmt_map: HashMap::new(),
        }
    }

    fn remap_callee(&self, callee: FuncId) -> FuncId {
        if callee == self.source_func {
            self.specialized_func
        } else {
            callee
        }
    }

    fn clone_expr_list(&mut self, exprs: Vec<ExprId>) -> Vec<ExprId> {
        exprs
            .into_iter()
            .map(|expr| self.clone_expr(expr))
            .collect()
    }

    fn clone_optional_stmt(&mut self, stmt: Option<StmtId>) -> Option<StmtId> {
        stmt.map(|stmt_id| self.clone_stmt(stmt_id))
    }

    fn clone_stmt(&mut self, source: StmtId) -> StmtId {
        if let Some(existing) = self.stmt_map.get(&source).copied() {
            return existing;
        }

        let Some(node) = self.program.stmt(source).cloned() else {
            return source;
        };

        let kind = match node.kind {
            StmtKind::Return(expr) => StmtKind::Return(self.clone_expr(expr)),
            StmtKind::Let {
                binding,
                value,
                next,
            } => StmtKind::Let {
                binding,
                value: self.clone_expr(value),
                next: self.clone_stmt(next),
            },
            StmtKind::Val {
                binding,
                value,
                next,
            } => StmtKind::Val {
                binding,
                value: self.clone_stmt(value),
                next: self.clone_stmt(next),
            },
            StmtKind::Call {
                result,
                callee,
                args,
                effects,
                next,
            } => StmtKind::Call {
                result,
                callee: self.remap_callee(callee),
                args: self.clone_expr_list(args),
                effects,
                next: self.clone_stmt(next),
            },
            StmtKind::If {
                cond,
                then_branch,
                else_branch,
            } => StmtKind::If {
                cond: self.clone_expr(cond),
                then_branch: self.clone_stmt(then_branch),
                else_branch: self.clone_stmt(else_branch),
            },
            StmtKind::Match {
                scrutinee,
                arms,
                default,
            } => StmtKind::Match {
                scrutinee: self.clone_expr(scrutinee),
                arms: arms
                    .into_iter()
                    .map(|arm| crate::ir::core::MatchArm {
                        tag: arm.tag,
                        binders: arm.binders,
                        body: self.clone_stmt(arm.body),
                        span: arm.span,
                    })
                    .collect(),
                default: self.clone_optional_stmt(default),
            },
            StmtKind::Perform {
                result,
                effect,
                operation,
                args,
                next,
            } => StmtKind::Perform {
                result,
                effect,
                operation,
                args: self.clone_expr_list(args),
                next: self.clone_stmt(next),
            },
            StmtKind::Resume {
                result,
                resume,
                arg,
                next,
            } => StmtKind::Resume {
                result,
                resume,
                arg: self.clone_expr(arg),
                next: self.clone_stmt(next),
            },
            StmtKind::Handle {
                handler,
                body,
                next,
            } => StmtKind::Handle {
                handler,
                body: self.clone_stmt(body),
                next: self.clone_optional_stmt(next),
            },
            StmtKind::Stage { stage, body, next } => StmtKind::Stage {
                stage,
                body: self.clone_stmt(body),
                next: self.clone_optional_stmt(next),
            },
            StmtKind::Hole { ty } => StmtKind::Hole { ty },
            StmtKind::Error(error) => StmtKind::Error(error),
        };

        let cloned = self.program.push_stmt(StmtNode {
            span: node.span,
            kind,
        });

        if let Some(effects) = self.sema.effects_of_stmt.get(source.index()).cloned() {
            if self.sema.effects_of_stmt.len() <= cloned.index() {
                self.sema.effects_of_stmt.resize(
                    cloned.index() + 1,
                    crate::sema::effect::SortedEffectRow::empty(),
                );
            }
            self.sema.effects_of_stmt[cloned.index()] = effects;
        }

        self.stmt_map.insert(source, cloned);
        cloned
    }

    fn clone_expr(&mut self, source: ExprId) -> ExprId {
        if let Some(existing) = self.expr_map.get(&source).copied() {
            return existing;
        }

        let Some(node) = self.program.expr(source).cloned() else {
            return source;
        };

        let kind = match node.kind {
            ExprKind::Var(var) => ExprKind::Var(var),
            ExprKind::Literal(literal) => ExprKind::Literal(literal),
            ExprKind::Unary { op, expr } => ExprKind::Unary {
                op,
                expr: self.clone_expr(expr),
            },
            ExprKind::Binary { op, lhs, rhs } => ExprKind::Binary {
                op,
                lhs: self.clone_expr(lhs),
                rhs: self.clone_expr(rhs),
            },
            ExprKind::PureCall { callee, args } => ExprKind::PureCall {
                callee: self.remap_callee(callee),
                args: self.clone_expr_list(args),
            },
            ExprKind::MakeStruct { ty, fields } => ExprKind::MakeStruct {
                ty,
                fields: self.clone_expr_list(fields),
            },
            ExprKind::MakeEnum {
                ty,
                variant,
                fields,
            } => ExprKind::MakeEnum {
                ty,
                variant,
                fields: self.clone_expr_list(fields),
            },
            ExprKind::Error(error) => ExprKind::Error(error),
        };

        let cloned = self.program.push_expr(ExprNode {
            span: node.span,
            kind,
        });

        // deep copy tables to guarantee provenance survival
        if let Some(stage) = self.bta.stage_of_expr.get(&source).copied() {
            self.bta.stage_of_expr.insert(cloned, stage);
        }
        if let Some(known) = self.bta.knownness_of_expr.get(&source).copied() {
            self.bta.knownness_of_expr.insert(cloned, known);
        }
        if let Some(ty) = self
            .sema
            .type_of_expr
            .get(source.index())
            .copied()
            .flatten()
        {
            if self.sema.type_of_expr.len() <= cloned.index() {
                self.sema.type_of_expr.resize(cloned.index() + 1, None);
            }
            self.sema.type_of_expr[cloned.index()] = Some(ty);
        }
        if let Some(effects) = self.sema.effects_of_expr.get(source.index()).cloned() {
            if self.sema.effects_of_expr.len() <= cloned.index() {
                self.sema.effects_of_expr.resize(
                    cloned.index() + 1,
                    crate::sema::effect::SortedEffectRow::empty(),
                );
            }
            self.sema.effects_of_expr[cloned.index()] = effects;
        }

        self.expr_map.insert(source, cloned);
        cloned
    }
}
