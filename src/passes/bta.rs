// Pass 5/9: bta (binding-time analysis, v0 classifier)
//
// Inputs:
// - Ct-propagated program + ct literal cache
//
// Outputs:
// - Stage table for expressions (`Ct` / `Rt(reason)`)
//
// Invariants:
// - Stage is assigned for every ExprId
// - CT cache membership implies CT stage in v0
//
// Diagnostics:
// - `BTA_CT_ONLY_RUNTIME_ARG` errors when ct-only calls receive runtime args
//
// Complexity:
// - O(expr_count + stmt_count)

use std::collections::HashSet;

use crate::common::ids::{EffectLabelId, ExprId, StmtId};
use crate::ir::core::{CoreProgram, ExprKind, StageDirective, StmtKind};
use crate::pipeline::phases::{
    BtaClassified, BtaTables, CtPropagated, Reason, SemanticTables, Stage,
};
use crate::sema::effect::{
    EffectFlags, EffectProperties, SortedEffectRow, first_non_thunkable_effect, is_thunkable,
};

pub fn run(ct: CtPropagated) -> BtaClassified {
    let (program, mut diagnostics, sema, mono, ct_tables) = ct.into_parts();
    let mut bta = BtaTables::default();

    for idx in 0..program.exprs().len() {
        let expr_id = ExprId::new(idx);
        if ct_tables.ct_cache.contains_key(&expr_id) {
            bta.stage_of_expr.insert(expr_id, Stage::Ct);
        } else {
            bta.stage_of_expr
                .insert(expr_id, Stage::Rt(Reason::UnclassifiedRuntime));
        }
    }

    let mut visited = HashSet::new();
    for function in program.functions() {
        apply_stage_directives(&program, function.body, None, &mut bta, &mut visited);
    }
    propagate_runtime_reasons(&program, &mut bta);

    classify_non_thunkable_effects(&program, &sema, &mut bta);
    enforce_ct_only_calls(&program, &sema, &mut bta, &mut diagnostics);

    BtaClassified::new(program, diagnostics, sema, mono, ct_tables, bta)
}

#[derive(Clone, Copy)]
enum ForcedStage {
    Ct,
    Rt,
}

fn apply_stage_directives(
    program: &CoreProgram,
    stmt_id: StmtId,
    forced: Option<ForcedStage>,
    bta: &mut BtaTables,
    visited: &mut HashSet<(StmtId, u8)>,
) {
    let key = (
        stmt_id,
        match forced {
            None => 0,
            Some(ForcedStage::Ct) => 1,
            Some(ForcedStage::Rt) => 2,
        },
    );
    if !visited.insert(key) {
        return;
    }

    let Some(stmt) = program.stmt(stmt_id) else {
        return;
    };

    match &stmt.kind {
        StmtKind::Return(expr) => apply_forced_expr(*expr, forced, bta),
        StmtKind::Let { value, next, .. } => {
            apply_forced_expr(*value, forced, bta);
            apply_stage_directives(program, *next, forced, bta, visited);
        }
        StmtKind::Val { value, next, .. } => {
            apply_stage_directives(program, *value, forced, bta, visited);
            apply_stage_directives(program, *next, forced, bta, visited);
        }
        StmtKind::Call { args, next, .. } => {
            for arg in args {
                apply_forced_expr(*arg, forced, bta);
            }
            apply_stage_directives(program, *next, forced, bta, visited);
        }
        StmtKind::Resume { arg, next, .. } => {
            apply_forced_expr(*arg, forced, bta);
            apply_stage_directives(program, *next, forced, bta, visited);
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            apply_forced_expr(*cond, forced, bta);
            apply_stage_directives(program, *then_branch, forced, bta, visited);
            apply_stage_directives(program, *else_branch, forced, bta, visited);
        }
        StmtKind::Match {
            scrutinee,
            arms,
            default,
        } => {
            apply_forced_expr(*scrutinee, forced, bta);
            for arm in arms {
                apply_stage_directives(program, arm.body, forced, bta, visited);
            }
            if let Some(default_stmt) = default {
                apply_stage_directives(program, *default_stmt, forced, bta, visited);
            }
        }
        StmtKind::Perform { args, next, .. } => {
            for arg in args {
                apply_forced_expr(*arg, forced, bta);
            }
            apply_stage_directives(program, *next, forced, bta, visited);
        }
        StmtKind::Handle { body, next, .. } => {
            apply_stage_directives(program, *body, forced, bta, visited);
            if let Some(next_stmt) = next {
                apply_stage_directives(program, *next_stmt, forced, bta, visited);
            }
        }
        StmtKind::Stage { stage, body, next } => {
            let inner = Some(match stage {
                StageDirective::Comptime => ForcedStage::Ct,
                StageDirective::Runtime => ForcedStage::Rt,
            });
            apply_stage_directives(program, *body, inner, bta, visited);
            if let Some(next_stmt) = next {
                apply_stage_directives(program, *next_stmt, forced, bta, visited);
            }
        }
        StmtKind::Hole { .. } | StmtKind::Error(_) => {}
    }
}

fn apply_forced_expr(expr_id: ExprId, forced: Option<ForcedStage>, bta: &mut BtaTables) {
    match forced {
        Some(ForcedStage::Ct) => {
            bta.stage_of_expr.insert(expr_id, Stage::Ct);
        }
        Some(ForcedStage::Rt) => {
            bta.stage_of_expr
                .insert(expr_id, Stage::Rt(Reason::UserForcedRuntime));
        }
        None => {}
    }
}

fn propagate_runtime_reasons(program: &CoreProgram, bta: &mut BtaTables) {
    let limit = program
        .exprs()
        .len()
        .saturating_add(program.stmts().len())
        .max(1);
    for _ in 0..limit {
        let mut changed = false;
        changed |= propagate_var_reasons(program, bta);
        changed |= propagate_expr_reasons(program, bta);
        if !changed {
            break;
        }
    }
}

fn propagate_var_reasons(program: &CoreProgram, bta: &mut BtaTables) -> bool {
    let mut changed = false;
    for stmt in program.stmts() {
        match &stmt.kind {
            StmtKind::Let { binding, value, .. } => {
                let Some(Stage::Rt(reason)) = bta.stage_of_expr.get(value).copied() else {
                    continue;
                };
                changed |= refine_var_stage(*binding, reason, bta);
            }
            StmtKind::Val { binding, value, .. } => {
                let Some(reason) = find_stmt_runtime_reason(program, bta, *value) else {
                    continue;
                };
                changed |= refine_var_stage(*binding, reason, bta);
            }
            StmtKind::Call { result, args, .. }
            | StmtKind::Perform {
                result: Some(result),
                args,
                ..
            } => {
                let Some(reason) = args.iter().find_map(|arg| {
                    bta.stage_of_expr.get(arg).and_then(|stage| match stage {
                        Stage::Rt(reason) => Some(*reason),
                        Stage::Ct => None,
                    })
                }) else {
                    continue;
                };
                changed |= refine_var_stage(*result, reason, bta);
            }
            StmtKind::Resume { result, arg, .. } => {
                let Some(Stage::Rt(reason)) = bta.stage_of_expr.get(arg).copied() else {
                    continue;
                };
                changed |= refine_var_stage(*result, reason, bta);
            }
            StmtKind::Return(_)
            | StmtKind::If { .. }
            | StmtKind::Match { .. }
            | StmtKind::Perform { result: None, .. }
            | StmtKind::Handle { .. }
            | StmtKind::Stage { .. }
            | StmtKind::Hole { .. }
            | StmtKind::Error(_) => {}
        }
    }
    changed
}

fn propagate_expr_reasons(program: &CoreProgram, bta: &mut BtaTables) -> bool {
    let mut changed = false;
    for (idx, expr) in program.exprs().iter().enumerate() {
        let expr_id = ExprId::new(idx);
        if matches!(bta.stage_of_expr.get(&expr_id), Some(Stage::Ct)) {
            continue;
        }
        let Some(reason) = infer_expr_runtime_reason(program, bta, expr_id, &expr.kind) else {
            continue;
        };
        changed |= refine_expr_stage(expr_id, reason, bta);
    }
    changed
}

fn infer_expr_runtime_reason(
    _program: &CoreProgram,
    bta: &BtaTables,
    _expr_id: ExprId,
    kind: &ExprKind,
) -> Option<Reason> {
    match kind {
        ExprKind::Var(var) => {
            if matches!(bta.stage_of_var.get(var), Some(Stage::Rt(_))) {
                Some(Reason::DependsOnVar(*var))
            } else {
                None
            }
        }
        ExprKind::Unary { expr, .. } => stage_reason_of_expr(bta, *expr),
        ExprKind::Binary { lhs, rhs, .. } => {
            stage_reason_of_expr(bta, *lhs).or_else(|| stage_reason_of_expr(bta, *rhs))
        }
        ExprKind::PureCall { args, .. } => {
            args.iter().find_map(|arg| stage_reason_of_expr(bta, *arg))
        }
        ExprKind::MakeStruct { fields, .. } | ExprKind::MakeEnum { fields, .. } => fields
            .iter()
            .find_map(|field| stage_reason_of_expr(bta, *field)),
        ExprKind::Literal(_) | ExprKind::Error(_) => None,
    }
}

fn stage_reason_of_expr(bta: &BtaTables, expr: ExprId) -> Option<Reason> {
    bta.stage_of_expr.get(&expr).and_then(|stage| match stage {
        Stage::Ct => None,
        Stage::Rt(reason) => Some(*reason),
    })
}

fn find_stmt_runtime_reason(
    program: &CoreProgram,
    bta: &BtaTables,
    root: StmtId,
) -> Option<Reason> {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        match &stmt.kind {
            StmtKind::Return(expr) => {
                if let Some(reason) = stage_reason_of_expr(bta, *expr) {
                    return Some(reason);
                }
            }
            StmtKind::Let { value, next, .. } => {
                if let Some(reason) = stage_reason_of_expr(bta, *value) {
                    return Some(reason);
                }
                stack.push(*next);
            }
            StmtKind::Val { value, next, .. } => {
                stack.push(*next);
                stack.push(*value);
            }
            StmtKind::Call { args, next, .. } | StmtKind::Perform { args, next, .. } => {
                if let Some(reason) = args.iter().find_map(|arg| stage_reason_of_expr(bta, *arg)) {
                    return Some(reason);
                }
                stack.push(*next);
            }
            StmtKind::Resume { arg, next, .. } => {
                if let Some(reason) = stage_reason_of_expr(bta, *arg) {
                    return Some(reason);
                }
                stack.push(*next);
            }
            StmtKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
                if stage_reason_of_expr(bta, *cond).is_some() {
                    return Some(Reason::BranchOnRuntime(*cond));
                }
                stack.push(*else_branch);
                stack.push(*then_branch);
            }
            StmtKind::Match {
                scrutinee,
                arms,
                default,
            } => {
                if let Some(reason) = stage_reason_of_expr(bta, *scrutinee) {
                    return Some(reason);
                }
                if let Some(default_stmt) = default {
                    stack.push(*default_stmt);
                }
                for arm in arms {
                    stack.push(arm.body);
                }
            }
            StmtKind::Handle { body, next, .. } | StmtKind::Stage { body, next, .. } => {
                if let Some(next_stmt) = next {
                    stack.push(*next_stmt);
                }
                stack.push(*body);
            }
            StmtKind::Hole { .. } | StmtKind::Error(_) => {}
        }
    }
    None
}

fn refine_expr_stage(expr_id: ExprId, reason: Reason, bta: &mut BtaTables) -> bool {
    match bta.stage_of_expr.get(&expr_id).copied() {
        Some(Stage::Ct) => false,
        Some(Stage::Rt(Reason::UnclassifiedRuntime)) => {
            bta.stage_of_expr.insert(expr_id, Stage::Rt(reason));
            true
        }
        Some(Stage::Rt(_)) => false,
        None => {
            bta.stage_of_expr.insert(expr_id, Stage::Rt(reason));
            true
        }
    }
}

fn refine_var_stage(
    var_id: crate::common::ids::VarId,
    reason: Reason,
    bta: &mut BtaTables,
) -> bool {
    match bta.stage_of_var.get(&var_id).copied() {
        Some(Stage::Ct) => false,
        Some(Stage::Rt(Reason::UnclassifiedRuntime)) => {
            bta.stage_of_var.insert(var_id, Stage::Rt(reason));
            true
        }
        Some(Stage::Rt(_)) => false,
        None => {
            bta.stage_of_var.insert(var_id, Stage::Rt(reason));
            true
        }
    }
}

fn is_runtime_expr(expr_id: ExprId, bta: &BtaTables) -> bool {
    !matches!(bta.stage_of_expr.get(&expr_id), Some(Stage::Ct))
}

fn enforce_ct_only_calls(
    program: &CoreProgram,
    sema: &SemanticTables,
    bta: &mut BtaTables,
    diagnostics: &mut crate::common::diagnostics::DiagnosticBag,
) {
    for (idx, expr) in program.exprs().iter().enumerate() {
        let ExprKind::PureCall { callee, args } = &expr.kind else {
            continue;
        };
        let is_ct_only = program
            .function(*callee)
            .is_some_and(|function| is_ct_only_function(function, sema));
        if !is_ct_only || !args.iter().copied().any(|arg| is_runtime_expr(arg, bta)) {
            continue;
        }

        let expr_id = ExprId::new(idx);
        bta.stage_of_expr
            .insert(expr_id, Stage::Rt(Reason::CtOnlyWithRuntimeArgs(*callee)));
        diagnostics.error(
            "BTA_CT_ONLY_RUNTIME_ARG",
            format!(
                "ct-only function call f{} has runtime arguments; this call cannot be residualized",
                callee.as_u32()
            ),
            expr.span,
        );
    }

    for stmt in program.stmts() {
        let StmtKind::Call {
            result,
            callee,
            args,
            ..
        } = &stmt.kind
        else {
            continue;
        };

        let is_ct_only = program
            .function(*callee)
            .is_some_and(|function| is_ct_only_function(function, sema));
        if !is_ct_only || !args.iter().copied().any(|arg| is_runtime_expr(arg, bta)) {
            continue;
        }

        bta.stage_of_var
            .insert(*result, Stage::Rt(Reason::CtOnlyWithRuntimeArgs(*callee)));
        diagnostics.error(
            "BTA_CT_ONLY_RUNTIME_ARG",
            format!(
                "ct-only function call f{} has runtime arguments; this call cannot be residualized",
                callee.as_u32()
            ),
            stmt.span,
        );
    }
}

fn is_ct_only_function(function: &crate::ir::core::FunctionDecl, sema: &SemanticTables) -> bool {
    function.ct_only
        || function.declared_effects.iter().any(|effect| {
            sema.effect_properties
                .get(&effect)
                .is_some_and(|props| props.flags.contains(EffectFlags::CT_ONLY))
        })
}

fn classify_non_thunkable_effects(
    program: &CoreProgram,
    sema: &SemanticTables,
    bta: &mut BtaTables,
) {
    for stmt in program.stmts() {
        match &stmt.kind {
            StmtKind::Call {
                result, effects, ..
            } => {
                if let Some(effect) = blocking_effect(effects, &sema.effect_properties) {
                    bta.stage_of_var
                        .insert(*result, Stage::Rt(Reason::EffectNotDischarged(effect)));
                }
            }
            StmtKind::Perform { result, effect, .. } => {
                let Some(result) = result else {
                    continue;
                };
                let row = SortedEffectRow::singleton(*effect);
                if let Some(blocking) = blocking_effect(&row, &sema.effect_properties) {
                    bta.stage_of_var
                        .insert(*result, Stage::Rt(Reason::EffectNotDischarged(blocking)));
                }
            }
            _ => {}
        }
    }
}

fn blocking_effect(
    row: &SortedEffectRow,
    effect_props: &std::collections::HashMap<EffectLabelId, EffectProperties>,
) -> Option<EffectLabelId> {
    if is_thunkable(row, effect_props) {
        return None;
    }
    first_non_thunkable_effect(row, effect_props)
}
