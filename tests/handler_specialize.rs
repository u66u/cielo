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

#[test]
fn specializes_handle_wrapped_call_through_if_forwarding_wrapper() {
    let src = r#"
effect Console { fn print(s: String) -> () }

fn io(v: Int) -> Int with Console {
  do Console.print("x");
  v
}

fn entry(flag: Bool) -> Int {
  let y = handle {
    if flag {
      io(7)
    } else {
      io(8)
    }
  } with Console {
    | print(s) => 0
  };
  y
}

fn main() -> Int {
  entry(true)
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    let specialized_id = specialized_copy_named(compiled.residual.program(), &interner, "io")
        .expect("if-forwarding wrapper should produce one specialized io copy");

    let entry = function_named(compiled.residual.program(), &interner, "entry").expect("entry");
    assert!(
        !contains_handle_stmt(compiled.residual.program(), entry.body),
        "if-forwarding wrapper should be rewritten to direct calls"
    );
    let callees = collect_call_callees(compiled.residual.program(), entry.body);
    assert_eq!(
        callees
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>(),
        std::collections::HashSet::from([specialized_id]),
        "all if branches should target the same specialized callee"
    );
    assert_eq!(
        callees.len(),
        2,
        "if-forwarding wrapper should preserve both branch-local direct callsites"
    );
}

#[test]
fn does_not_specialize_if_wrapper_with_branch_callee_mismatch() {
    let src = r#"
effect Console { fn print(s: String) -> () }

fn io() -> Int with Console {
  do Console.print("x");
  1
}

fn alt() -> Int with Console {
  do Console.print("y");
  2
}

fn entry(flag: Bool) -> Int {
  let y = handle {
    if flag {
      io()
    } else {
      alt()
    }
  } with Console {
    | print(s) => 0
  };
  y
}

fn main() -> Int {
  entry(true)
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    assert!(
        specialized_copy_named(compiled.residual.program(), &interner, "io").is_none(),
        "mismatched if-branch callees must not trigger io specialization"
    );
    assert!(
        specialized_copy_named(compiled.residual.program(), &interner, "alt").is_none(),
        "mismatched if-branch callees must not trigger alt specialization"
    );

    let entry = function_named(compiled.residual.program(), &interner, "entry").expect("entry");
    assert!(
        contains_handle_stmt(compiled.residual.program(), entry.body),
        "mismatched if-branch wrappers should remain unspecialized"
    );
}

#[test]
fn specializes_handle_wrapped_call_through_match_forwarding_wrapper() {
    let src = r#"
effect Console { fn print(s: String) -> () }
enum Tag { A, B }

fn io(v: Int) -> Int with Console {
  do Console.print("x");
  v
}

fn entry(tag: Tag) -> Int {
  let y = handle {
    match tag {
      | A => io(7)
      | _ => io(8)
    }
  } with Console {
    | print(s) => 0
  };
  y
}

fn main() -> Int {
  entry(A())
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    let specialized_id = specialized_copy_named(compiled.residual.program(), &interner, "io")
        .expect("match-forwarding wrapper should produce one specialized io copy");

    let entry = function_named(compiled.residual.program(), &interner, "entry").expect("entry");
    assert!(
        !contains_handle_stmt(compiled.residual.program(), entry.body),
        "match-forwarding wrapper should be rewritten to direct calls"
    );
    let callees = collect_call_callees(compiled.residual.program(), entry.body);
    assert_eq!(
        callees
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>(),
        std::collections::HashSet::from([specialized_id]),
        "all match branches should target the same specialized callee"
    );
    assert_eq!(
        callees.len(),
        2,
        "match-forwarding wrapper should preserve one direct callsite per arm/default path"
    );
}

#[test]
fn does_not_specialize_match_wrapper_without_default_branch() {
    let src = r#"
effect Console { fn print(s: String) -> () }
enum Tag { A, B }

fn io(v: Int) -> Int with Console {
  do Console.print("x");
  v
}

fn entry(tag: Tag) -> Int {
  let y = handle {
    match tag {
      | A => io(7)
    }
  } with Console {
    | print(s) => 0
  };
  y
}

fn main() -> Int {
  entry(A())
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    assert!(
        specialized_copy_named(compiled.residual.program(), &interner, "io").is_none(),
        "match wrappers without default branch should conservatively skip specialization"
    );
    let entry = function_named(compiled.residual.program(), &interner, "entry").expect("entry");
    assert!(
        contains_handle_stmt(compiled.residual.program(), entry.body),
        "missing default match wrapper should remain unspecialized"
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

fn specialized_copy_named(
    program: &CoreProgram,
    interner: &Interner,
    name: &str,
) -> Option<FuncId> {
    let mut out = None;
    for (idx, function) in program.functions().iter().enumerate() {
        if interner.resolve(function.name) != Some(name) {
            continue;
        }
        if first_handle_handler(program, function.body).is_none() {
            continue;
        }
        let id = FuncId::new(idx);
        assert!(
            out.is_none(),
            "expected at most one specialized copy named `{name}`, found another at id {}",
            id.as_u32()
        );
        out = Some(id);
    }
    out
}

fn function_named<'a>(
    program: &'a CoreProgram,
    interner: &Interner,
    name: &str,
) -> Option<&'a cielo::ir::core::FunctionDecl> {
    program
        .functions()
        .iter()
        .find(|function| interner.resolve(function.name) == Some(name))
}
