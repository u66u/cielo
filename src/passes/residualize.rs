// Pass 6/8: residualize (stage-erasure + residual metadata)
//
// Inputs:
// - BTA-classified program
//
// Outputs:
// - Residualized wrapper + residual side tables
//
// Invariants:
// - Function-level effect annotations are erased from function decls
// - Call sites keep effect rows via residual summaries keyed by callee FuncId
// - Residual metadata object always exists
//
// Diagnostics:
// - None in v0
//
// Complexity:
// - O(functions + reachable statements)

use std::collections::{HashMap, HashSet};

use crate::common::ids::{FuncId, HandlerId, StmtId};
use crate::ir::core::{CoreProgram, StmtKind};
use crate::pipeline::phases::{BtaClassified, ResidualTables, Residualized};
use crate::sema::effect::SortedEffectRow;

pub fn run(mut bta: BtaClassified) -> Residualized {
    let function_effect_summary = collect_function_effect_summary(&bta.program, &bta.sema.effects_of_stmt);
    rewrite_call_effect_rows(&mut bta.program, &function_effect_summary);
    erase_function_effect_annotations(&mut bta.program);
    bta.into_residualized(ResidualTables {
        function_effect_summary,
    })
}

fn collect_function_effect_summary(
    program: &CoreProgram,
    stmt_effects: &[SortedEffectRow],
) -> HashMap<FuncId, SortedEffectRow> {
    program
        .functions()
        .iter()
        .enumerate()
        .map(|(idx, function)| {
            let row = stmt_effects
                .get(function.body.index())
                .cloned()
                .unwrap_or_else(SortedEffectRow::empty);
            (FuncId::new(idx), row)
        })
        .collect()
}

fn rewrite_call_effect_rows(program: &mut CoreProgram, summaries: &HashMap<FuncId, SortedEffectRow>) {
    let reachable = collect_reachable_stmts(program);
    for stmt_id in reachable {
        let Some(stmt) = program.stmt_mut(stmt_id) else {
            continue;
        };
        if let StmtKind::Call { callee, effects, .. } = &mut stmt.kind {
            *effects = summaries
                .get(callee)
                .cloned()
                .unwrap_or_else(SortedEffectRow::empty);
        }
    }
}

fn erase_function_effect_annotations(program: &mut CoreProgram) {
    for function in program.functions_mut() {
        function.declared_effects = SortedEffectRow::empty();
    }
}

fn collect_reachable_stmts(program: &CoreProgram) -> Vec<StmtId> {
    let mut seen_stmts = HashSet::new();
    let mut seen_handlers = HashSet::new();
    let mut stack = program.functions().iter().map(|f| f.body).collect::<Vec<_>>();

    while let Some(stmt_id) = stack.pop() {
        if !seen_stmts.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        match &stmt.kind {
            StmtKind::Return(_) | StmtKind::Hole { .. } | StmtKind::Error(_) => {}
            StmtKind::Let { next, .. }
            | StmtKind::Val { next, .. }
            | StmtKind::Call { next, .. }
            | StmtKind::Perform { next, .. } => stack.push(*next),
            StmtKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                stack.push(*then_branch);
                stack.push(*else_branch);
            }
            StmtKind::Match { arms, default, .. } => {
                for arm in arms {
                    stack.push(arm.body);
                }
                if let Some(default_stmt) = default {
                    stack.push(*default_stmt);
                }
            }
            StmtKind::Handle {
                handler,
                body,
                next,
            } => {
                stack.push(*body);
                if let Some(next_stmt) = next {
                    stack.push(*next_stmt);
                }
                push_handler_bodies(program, *handler, &mut seen_handlers, &mut stack);
            }
            StmtKind::Stage { body, next, .. } => {
                stack.push(*body);
                if let Some(next_stmt) = next {
                    stack.push(*next_stmt);
                }
            }
        }
    }

    let mut out = seen_stmts.into_iter().collect::<Vec<_>>();
    out.sort_by_key(|id| id.index());
    out
}

fn push_handler_bodies(
    program: &CoreProgram,
    handler: HandlerId,
    seen_handlers: &mut HashSet<HandlerId>,
    stack: &mut Vec<StmtId>,
) {
    if !seen_handlers.insert(handler) {
        return;
    }
    let Some(def) = program.handlers().get(handler.index()) else {
        return;
    };
    stack.push(def.return_body);
    for clause in &def.clauses {
        stack.push(clause.body);
    }
}
