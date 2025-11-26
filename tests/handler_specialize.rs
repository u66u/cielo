use std::collections::HashSet;

use cielo::common::ids::{FuncId, HandlerId, SourceId, StmtId};
use cielo::common::symbols::Interner;
use cielo::ir::core::{CoreProgram, StmtKind};
use cielo::{Compiler, CompilerConfig};

#[test]
fn specializes_handle_wrapped_recursive_calls_and_retargets_recursion() {
    let src = r#"
effect Console { fn print(s: String) -> () }

fn loop() -> Int with Console {
  loop()
}

fn main() -> Int {
  let x = handle { loop() } with Console {
    | print(s) => 0
  };
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    let loop_ids: Vec<FuncId> = compiled
        .residual
        .program()
        .functions()
        .iter()
        .enumerate()
        .filter_map(|(idx, function)| {
            (interner.resolve(function.name) == Some("loop")).then_some(FuncId::new(idx))
        })
        .collect();
    assert_eq!(loop_ids.len(), 2, "expected original + specialized loop copy");

    let specialized_id = loop_ids
        .iter()
        .copied()
        .max_by_key(|id| id.index())
        .expect("specialized loop id");
    let main = compiled
        .residual
        .program()
        .functions()
        .iter()
        .find(|function| interner.resolve(function.name) == Some("main"))
        .expect("main function");
    let handle_call_callee = first_handle_body_call_callee(compiled.residual.program(), main.body)
        .expect("handle-wrapped call in main");
    assert_eq!(
        handle_call_callee, specialized_id,
        "handle-wrapped call should target specialized copy"
    );

    let specialized = compiled
        .residual
        .program()
        .function(specialized_id)
        .expect("specialized function");
    let specialized_handler =
        first_handle_handler(compiled.residual.program(), specialized.body).expect("specialized handle");
    assert_eq!(
        specialized_handler,
        HandlerId::new(0),
        "specialized function should embed the handler from the callsite"
    );
    let recursive_callee = first_call_callee(compiled.residual.program(), specialized.body)
        .expect("recursive call in specialized body");
    assert_eq!(
        recursive_callee, specialized_id,
        "specialized recursive edge should retarget to specialized copy"
    );
}

#[test]
fn deduplicates_specialization_for_equivalent_handler_shapes() {
    let src = r#"
effect Console { fn print(s: String) -> () }

fn io() -> Int with Console {
  do Console.print("x");
  1
}

fn main() -> Int {
  let a = handle { io() } with Console {
    | print(s) => 0
  };
  let b = handle { io() } with Console {
    | print(s) => 0
  };
  a + b
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    let io_ids: Vec<FuncId> = compiled
        .residual
        .program()
        .functions()
        .iter()
        .enumerate()
        .filter_map(|(idx, function)| {
            (interner.resolve(function.name) == Some("io")).then_some(FuncId::new(idx))
        })
        .collect();
    assert_eq!(
        io_ids.len(),
        2,
        "equivalent handlers should reuse one specialized copy"
    );

    let specialized_id = io_ids
        .iter()
        .copied()
        .max_by_key(|id| id.index())
        .expect("specialized io id");
    let main = compiled
        .residual
        .program()
        .functions()
        .iter()
        .find(|function| interner.resolve(function.name) == Some("main"))
        .expect("main function");
    let callees = handle_body_call_callees(compiled.residual.program(), main.body);
    assert!(
        !callees.is_empty(),
        "expected at least one handle-wrapped call in main"
    );
    assert!(
        callees.iter().all(|callee| *callee == specialized_id),
        "all equivalent handle callsites should target the same specialized function: {callees:?}"
    );
}

fn first_handle_handler(program: &CoreProgram, root: StmtId) -> Option<HandlerId> {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let stmt = program.stmt(stmt_id)?;
        match &stmt.kind {
            StmtKind::Handle { handler, .. } => return Some(*handler),
            _ => {
                for child in stmt.child_stmts() {
                    stack.push(child);
                }
            }
        }
    }
    None
}

fn handle_body_call_callees(program: &CoreProgram, root: StmtId) -> Vec<FuncId> {
    let mut out = Vec::new();
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
            StmtKind::Handle { body, next, .. } => {
                if let Some(call_stmt) = first_call_stmt(program, *body) {
                    if let Some(call_node) = program.stmt(call_stmt) {
                        if let StmtKind::Call { callee, .. } = call_node.kind {
                            out.push(callee);
                        }
                    }
                }
                stack.push(*body);
                if let Some(next_stmt) = next {
                    stack.push(*next_stmt);
                }
            }
            StmtKind::Let { next, .. }
            | StmtKind::Call { next, .. }
            | StmtKind::Resume { next, .. }
            | StmtKind::Perform {
                next,
                ..
            } => stack.push(*next),
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
            StmtKind::Stage { body, next, .. } => {
                stack.push(*body);
                if let Some(next_stmt) = next {
                    stack.push(*next_stmt);
                }
            }
            StmtKind::Return(_) | StmtKind::Hole { .. } | StmtKind::Error(_) => {}
        }
    }
    out
}

fn first_handle_body_call_callee(program: &CoreProgram, root: StmtId) -> Option<FuncId> {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let stmt = program.stmt(stmt_id)?;
        match &stmt.kind {
            StmtKind::Handle { body, next, .. } => {
                if let Some(call_stmt) = first_call_stmt(program, *body) {
                    let body_stmt = program.stmt(call_stmt)?;
                    if let StmtKind::Call { callee, .. } = body_stmt.kind {
                        return Some(callee);
                    }
                }
                stack.push(*body);
                if let Some(next_stmt) = next {
                    stack.push(*next_stmt);
                }
            }
            StmtKind::Let { next, .. }
            | StmtKind::Call { next, .. }
            | StmtKind::Resume { next, .. }
            | StmtKind::Perform {
                next,
                ..
            } => stack.push(*next),
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
            StmtKind::Stage { body, next, .. } => {
                stack.push(*body);
                if let Some(next_stmt) = next {
                    stack.push(*next_stmt);
                }
            }
            StmtKind::Return(_) | StmtKind::Hole { .. } | StmtKind::Error(_) => {}
        }
    }
    None
}

fn first_call_callee(program: &CoreProgram, root: StmtId) -> Option<FuncId> {
    let call_stmt = first_call_stmt(program, root)?;
    let stmt = program.stmt(call_stmt)?;
    let StmtKind::Call { callee, .. } = stmt.kind else {
        return None;
    };
    Some(callee)
}

fn first_call_stmt(program: &CoreProgram, root: StmtId) -> Option<StmtId> {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let stmt = program.stmt(stmt_id)?;
        match &stmt.kind {
            StmtKind::Call { .. } => return Some(stmt_id),
            _ => {
                for child in stmt.child_stmts() {
                    stack.push(child);
                }
            }
        }
    }
    None
}
