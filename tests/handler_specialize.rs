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
    assert_eq!(
        loop_ids.len(),
        2,
        "expected original + specialized loop copy"
    );

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
    assert!(
        !contains_handle_stmt(compiled.residual.program(), main.body),
        "direct handle-wrapped callsites should be rewritten to plain calls"
    );
    let handle_call_callee = first_call_callee(compiled.residual.program(), main.body)
        .expect("specialized call in main");
    assert_eq!(
        handle_call_callee, specialized_id,
        "handle-wrapped call should target specialized copy"
    );

    let specialized = compiled
        .residual
        .program()
        .function(specialized_id)
        .expect("specialized function");
    let specialized_handler = first_handle_handler(compiled.residual.program(), specialized.body)
        .expect("specialized handle");
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
    assert!(
        !contains_handle_stmt(compiled.residual.program(), main.body),
        "equivalent direct handle wrappers should be removed after specialization"
    );
    let callees = collect_call_callees(compiled.residual.program(), main.body);
    assert!(
        callees.len() >= 2,
        "expected both direct callsites in main to remain as calls"
    );
    assert_eq!(
        callees
            .into_iter()
            .collect::<std::collections::HashSet<_>>(),
        std::collections::HashSet::from([specialized_id]),
        "all equivalent handle callsites should target the same specialized function"
    );
}

#[test]
fn specializes_handle_wrapped_call_through_let_wrapper_chain() {
    let src = r#"
effect Console { fn print(s: String) -> () }

fn io(v: Int) -> Int with Console {
  do Console.print("x");
  v
}

fn main() -> Int {
  let y = handle {
    let seed = 7;
    io(seed)
  } with Console {
    | print(s) => 0
  };
  y
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
    assert_eq!(io_ids.len(), 2, "expected original + specialized io copy");

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
    assert!(
        !contains_handle_stmt(compiled.residual.program(), main.body),
        "let-wrapped direct handle callsites should be specialized and rewritten"
    );
    let handle_call_callee = first_call_callee(compiled.residual.program(), main.body)
        .expect("specialized call in main");
    assert_eq!(
        handle_call_callee, specialized_id,
        "specialized call under let-wrapper should target specialized copy"
    );
}

#[test]
fn does_not_specialize_handle_body_with_non_wrapper_post_call_work() {
    let src = r#"
effect Console { fn print(s: String) -> () }

fn io() -> Int with Console {
  do Console.print("x");
  1
}

fn main() -> Int {
  let y = handle {
    let v = io();
    v + 1
  } with Console {
    | print(s) => 0
  };
  y
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
        1,
        "non-wrapper post-call work must not trigger specialization copies"
    );

    let main = compiled
        .residual
        .program()
        .functions()
        .iter()
        .find(|function| interner.resolve(function.name) == Some("main"))
        .expect("main function");
    assert!(
        contains_handle_stmt(compiled.residual.program(), main.body),
        "non-wrapper handle body should remain unspecialized"
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

fn contains_handle_stmt(program: &CoreProgram, root: StmtId) -> bool {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if matches!(stmt.kind, StmtKind::Handle { .. }) {
            return true;
        }
        stack.extend(stmt.child_stmts());
    }
    false
}

fn collect_call_callees(program: &CoreProgram, root: StmtId) -> Vec<FuncId> {
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
        if let StmtKind::Call { callee, .. } = stmt.kind {
            out.push(callee);
        }
        stack.extend(stmt.child_stmts());
    }
    out
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
