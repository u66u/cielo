#![allow(dead_code)]

use std::collections::HashSet;

use cielo::common::ids::{ExprId, FuncId, StmtId};
use cielo::ir::core::{CoreProgram, StmtKind};

pub fn first_return_expr(program: &CoreProgram, root: StmtId) -> Option<ExprId> {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let stmt = program.stmt(stmt_id)?;
        if let StmtKind::Return(expr_id) = stmt.kind {
            return Some(expr_id);
        }
        stack.extend(stmt.child_stmts());
    }
    None
}

pub fn first_return_expr_linear(program: &CoreProgram, root: StmtId) -> Option<ExprId> {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let stmt = program.stmt(stmt_id)?;
        match &stmt.kind {
            StmtKind::Return(expr_id) => return Some(*expr_id),
            StmtKind::Let { next, .. }
            | StmtKind::Call { next, .. }
            | StmtKind::Resume { next, .. }
            | StmtKind::Perform { next, .. } => stack.push(*next),
            StmtKind::Val { value, next, .. } => {
                stack.push(*next);
                stack.push(*value);
            }
            StmtKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                stack.push(*else_branch);
                stack.push(*then_branch);
            }
            StmtKind::Match { arms, default, .. } => {
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

pub fn reachable_stmt_count(program: &CoreProgram) -> usize {
    let mut seen_stmts = HashSet::new();
    let mut stack = program
        .functions()
        .iter()
        .map(|function| function.body)
        .collect::<Vec<_>>();
    while let Some(stmt_id) = stack.pop() {
        if !seen_stmts.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        stack.extend(stmt.child_stmts());
    }
    seen_stmts.len()
}

pub fn reachable_stmt_count_from_root(program: &CoreProgram, root: StmtId) -> usize {
    let mut seen = HashSet::new();
    let mut stack = vec![root];
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        stack.extend(stmt.child_stmts());
    }
    seen.len()
}

pub fn contains_if_stmt(program: &CoreProgram, root: StmtId) -> bool {
    let mut seen_stmts = HashSet::new();
    let mut stack = vec![root];
    while let Some(stmt_id) = stack.pop() {
        if !seen_stmts.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if matches!(stmt.kind, StmtKind::If { .. }) {
            return true;
        }
        stack.extend(stmt.child_stmts());
    }
    false
}

pub fn contains_call_to(program: &CoreProgram, root: StmtId, target: FuncId) -> bool {
    let mut seen_stmts = HashSet::new();
    let mut stack = vec![root];
    while let Some(stmt_id) = stack.pop() {
        if !seen_stmts.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if matches!(stmt.kind, StmtKind::Call { callee, .. } if callee == target) {
            return true;
        }
        stack.extend(stmt.child_stmts());
    }
    false
}
