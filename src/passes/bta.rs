// Pass 5/8: bta (binding-time analysis, v0 classifier)
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

use crate::common::ids::{ExprId, StmtId};
use crate::ir::core::{CoreProgram, ExprKind, StageDirective, StmtKind};
use crate::pipeline::phases::{BtaClassified, BtaTables, CtPropagated, Reason, Stage};

pub fn run(ct: CtPropagated) -> BtaClassified {
    let (program, mut diagnostics, sema, mono, ct_tables) = ct.into_parts();
    let mut bta = BtaTables::default();

    for idx in 0..program.exprs().len() {
        let expr_id = ExprId::new(idx);
        if ct_tables.ct_cache.contains_key(&expr_id) {
            bta.stage_of_expr.insert(expr_id, Stage::Ct);
        } else {
            bta.stage_of_expr
                .insert(expr_id, Stage::Rt(Reason::UserForcedRuntime));
        }
    }

    let mut visited = HashSet::new();
    for function in program.functions() {
        apply_stage_directives(&program, function.body, None, &mut bta, &mut visited);
    }

    enforce_ct_only_calls(&program, &mut bta, &mut diagnostics);

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

fn is_runtime_expr(expr_id: ExprId, bta: &BtaTables) -> bool {
    !matches!(bta.stage_of_expr.get(&expr_id), Some(Stage::Ct))
}

fn enforce_ct_only_calls(
    program: &CoreProgram,
    bta: &mut BtaTables,
    diagnostics: &mut crate::common::diagnostics::DiagnosticBag,
) {
    for (idx, expr) in program.exprs().iter().enumerate() {
        let ExprKind::PureCall { callee, args } = &expr.kind else {
            continue;
        };
        let is_ct_only = program
            .function(*callee)
            .is_some_and(|function| function.ct_only);
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
            .is_some_and(|function| function.ct_only);
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
