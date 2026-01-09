use std::collections::HashSet;

use cielo::common::ids::{FuncId, HandlerId, SourceId, StmtId};
use cielo::common::symbols::Interner;
use cielo::ir::core::{CoreProgram, StmtKind};
use cielo::pipeline::phases::{Reason, Stage};
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

    assert_eq!(
        function_count_named(compiled.residual.program(), &interner, "loop"),
        1,
        "unreachable unspecialized loop copy should be pruned after specialization"
    );
    let specialized_id = specialized_copy_named(compiled.residual.program(), &interner, "loop")
        .expect("specialized loop id");
    assert_phase_func_ids_in_bounds(&compiled);

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

    assert_eq!(
        function_count_named(compiled.residual.program(), &interner, "io"),
        1,
        "equivalent wrappers should compact to one reachable specialized io copy"
    );

    let specialized_id = specialized_copy_named(compiled.residual.program(), &interner, "io")
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

    assert_eq!(
        function_count_named(compiled.residual.program(), &interner, "io"),
        1,
        "let-forwarding wrappers should leave only one reachable specialized io copy"
    );
    let specialized_id = specialized_copy_named(compiled.residual.program(), &interner, "io")
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

    assert_eq!(
        function_count_named(compiled.residual.program(), &interner, "io"),
        1,
        "non-wrapper post-call work must not trigger specialization copies"
    );
    assert!(
        specialized_copy_named(compiled.residual.program(), &interner, "io").is_none(),
        "non-wrapper post-call work should keep only the original io function"
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
fn specializes_match_wrapper_without_default_when_arms_forward_same_callee() {
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

    let specialized_id = specialized_copy_named(compiled.residual.program(), &interner, "io")
        .expect("match wrapper without default should still specialize for single-callee arms");
    let entry = function_named(compiled.residual.program(), &interner, "entry").expect("entry");
    assert!(
        !contains_handle_stmt(compiled.residual.program(), entry.body),
        "single-callee match wrapper without default should be rewritten after specialization"
    );
    let callees = collect_call_callees(compiled.residual.program(), entry.body);
    assert_eq!(
        callees
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>(),
        std::collections::HashSet::from([specialized_id]),
        "all rewritten match-arm calls should target the specialized callee"
    );
    assert_eq!(
        callees.len(),
        1,
        "single-arm match wrapper should preserve one direct callsite"
    );
}

#[test]
fn does_not_specialize_match_wrapper_without_default_when_arms_disagree() {
    let src = r#"
effect Console { fn print(s: String) -> () }
enum Tag { A, B }

fn io(v: Int) -> Int with Console {
  do Console.print("x");
  v
}

fn alt(v: Int) -> Int with Console {
  do Console.print("y");
  v
}

fn entry(tag: Tag) -> Int {
  let y = handle {
    match tag {
      | A => io(7)
      | B => alt(8)
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
        "mismatched no-default match arms must not specialize io"
    );
    assert!(
        specialized_copy_named(compiled.residual.program(), &interner, "alt").is_none(),
        "mismatched no-default match arms must not specialize alt"
    );
    let entry = function_named(compiled.residual.program(), &interner, "entry").expect("entry");
    assert!(
        contains_handle_stmt(compiled.residual.program(), entry.body),
        "mismatched no-default match wrapper should remain unspecialized"
    );
}

#[test]
fn keeps_unspecialized_copy_when_original_still_reachable() {
    let src = r#"
effect Console { fn print(s: String) -> () }

fn io(v: Int) -> Int with Console {
  do Console.print("x");
  v
}

fn main() -> Int {
  let fast = handle { io(1) } with Console {
    | print(s) => 0
  };
  let slow = handle {
    let v = io(2);
    v + 1
  } with Console {
    | print(s) => 0
  };
  fast + slow
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    let io_ids = function_ids_named(compiled.residual.program(), &interner, "io");
    assert_eq!(
        io_ids.len(),
        2,
        "when another path still uses original io, pruning must keep both original and specialized copies"
    );
    let specialized_id = specialized_copy_named(compiled.residual.program(), &interner, "io")
        .expect("specialized io id");
    let original_id = io_ids
        .iter()
        .copied()
        .find(|id| *id != specialized_id)
        .expect("original io id");

    let main = function_named(compiled.residual.program(), &interner, "main").expect("main");
    assert!(
        contains_handle_stmt(compiled.residual.program(), main.body),
        "slow path should remain as an unspecialized handle wrapper"
    );
    let callees = collect_call_callees(compiled.residual.program(), main.body)
        .into_iter()
        .collect::<HashSet<_>>();
    assert!(
        callees.contains(&specialized_id),
        "fast wrapper path should call specialized io"
    );
    assert!(
        callees.contains(&original_id),
        "slow unspecialized path should keep calling original io"
    );
    assert_phase_func_ids_in_bounds(&compiled);
}

#[test]
fn skips_specialization_for_unreachable_wrapper_only_handles() {
    let src = r#"
effect Console { fn print(s: String) -> () }

fn io() -> Int with Console {
  do Console.print("x");
  1
}

fn dead_wrapper() -> Int {
  let y = handle { io() } with Console {
    | print(s) => 0
  };
  y
}

fn main() -> Int {
  io()
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    assert!(
        specialized_copy_named(compiled.residual.program(), &interner, "io").is_none(),
        "unreachable wrapper-only handles must not trigger specialization copies"
    );
    assert_eq!(
        function_count_named(compiled.residual.program(), &interner, "io"),
        1,
        "only the original reachable io should remain"
    );
}

#[test]
fn specializes_reachable_wrapper_only_handles() {
    let src = r#"
effect Console { fn print(s: String) -> () }

fn io() -> Int with Console {
  do Console.print("x");
  1
}

fn main() -> Int {
  let y = handle { io() } with Console {
    | print(s) => 0
  };
  y
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    let specialized_id = specialized_copy_named(compiled.residual.program(), &interner, "io")
        .expect("reachable wrapper-only handle should produce a specialized io copy");
    let main = function_named(compiled.residual.program(), &interner, "main").expect("main");
    assert!(
        !contains_handle_stmt(compiled.residual.program(), main.body),
        "reachable wrapper-only handle should be rewritten to direct call"
    );
    assert_eq!(
        first_call_callee(compiled.residual.program(), main.body),
        Some(specialized_id),
        "rewritten main path should call the specialized io copy"
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

fn function_ids_named(program: &CoreProgram, interner: &Interner, name: &str) -> Vec<FuncId> {
    program
        .functions()
        .iter()
        .enumerate()
        .filter_map(|(idx, function)| {
            (interner.resolve(function.name) == Some(name)).then_some(FuncId::new(idx))
        })
        .collect()
}

fn function_count_named(program: &CoreProgram, interner: &Interner, name: &str) -> usize {
    function_ids_named(program, interner, name).len()
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

fn assert_phase_func_ids_in_bounds(compiled: &cielo::CompiledC) {
    let func_count = compiled.residual.program().functions().len();
    assert!(
        compiled
            .residual
            .residual()
            .function_effect_summary
            .keys()
            .all(|id| id.index() < func_count),
        "residual function-effect summary keys must stay within compacted function id bounds"
    );
    assert!(
        compiled
            .residual
            .mono()
            .source_to_mono
            .iter()
            .all(|(source, monos)| {
                source.index() < func_count && monos.iter().all(|mono| mono.index() < func_count)
            }),
        "monomorphization summary ids must stay within compacted function id bounds"
    );
    assert!(
        compiled
            .residual
            .bta()
            .stage_of_expr
            .values()
            .all(|stage| stage_has_valid_func_ids(*stage, func_count))
            && compiled
                .residual
                .bta()
                .stage_of_var
                .values()
                .all(|stage| stage_has_valid_func_ids(*stage, func_count)),
        "BTA stage reasons must not retain stale function ids after specialization pruning"
    );
    assert!(
        compiled
            .residual
            .bta()
            .handler_discharge
            .values()
            .all(|discharge| {
                discharge
                    .reason
                    .is_none_or(|reason| reason_has_valid_func_ids(reason, func_count))
            }),
        "handler discharge reasons must not retain stale function ids after specialization pruning"
    );
}

fn stage_has_valid_func_ids(stage: Stage, func_count: usize) -> bool {
    match stage {
        Stage::Ct => true,
        Stage::Rt(reason) => reason_has_valid_func_ids(reason, func_count),
    }
}

fn reason_has_valid_func_ids(reason: Reason, func_count: usize) -> bool {
    match reason {
        Reason::Parameter { func, .. } | Reason::CtOnlyWithRuntimeArgs(func) => {
            func.index() < func_count
        }
        _ => true,
    }
}
