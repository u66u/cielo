use cielo::common::ids::SourceId;
use cielo::common::symbols::Interner;
use cielo::ir::core::{ExprKind, Literal, StmtKind};
use cielo::pipeline::compiler::Endianness;
use cielo::pipeline::phases::{Knownness, Stage};
use cielo::sema::effect::SortedEffectRow;
use cielo::{Compiler, CompilerConfig};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn ct_and_bta_tables_are_populated() {
    let src = r#"
fn main() -> Int {
  let x = 1 + 2;
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);

    assert!(!residual.ct().ct_cache.is_empty());
    assert!(
        residual
            .bta()
            .stage_of_expr
            .values()
            .any(|stage| matches!(stage, Stage::Ct))
    );
}

#[test]
fn runtime_directive_forces_runtime_stage_inside_block() {
    let src = r#"
fn main() -> Int {
  let y = @runtime { 1 + 2 };
  y
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);

    let main = residual.program().functions().first().expect("main");
    let mut cursor = main.body;
    let mut stage_body = None;
    while let Some(stmt) = residual.program().stmt(cursor) {
        match &stmt.kind {
            StmtKind::Val { value, next, .. } => {
                if let Some(StmtKind::Stage { body, .. }) =
                    residual.program().stmt(*value).map(|n| &n.kind)
                {
                    stage_body = Some(*body);
                    break;
                }
                cursor = *next;
            }
            StmtKind::Let { next, .. } => cursor = *next,
            StmtKind::Return(_) => break,
            _ => break,
        }
    }

    let stage_body = stage_body.expect("expected stage body");
    let staged_return_expr = match residual.program().stmt(stage_body).map(|n| &n.kind) {
        Some(StmtKind::Return(expr)) => *expr,
        _ => panic!("expected return in stage body"),
    };

    assert!(matches!(
        residual.bta().stage_of_expr.get(&staged_return_expr),
        Some(Stage::Rt(_))
    ));
}

#[test]
fn comptime_directive_forces_ct_stage_inside_block() {
    let src = r#"
fn main() -> Int {
  let x = 1;
  let y = @comptime { x };
  y
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);

    let main = residual.program().functions().first().expect("main");
    let mut cursor = main.body;
    let mut stage_body = None;
    while let Some(stmt) = residual.program().stmt(cursor) {
        match &stmt.kind {
            StmtKind::Val { value, next, .. } => {
                if let Some(StmtKind::Stage { body, .. }) =
                    residual.program().stmt(*value).map(|n| &n.kind)
                {
                    stage_body = Some(*body);
                    break;
                }
                cursor = *next;
            }
            StmtKind::Let { next, .. } => cursor = *next,
            StmtKind::Return(_) => break,
            _ => break,
        }
    }

    let stage_body = stage_body.expect("expected stage body");
    let staged_return_expr = match residual.program().stmt(stage_body).map(|n| &n.kind) {
        Some(StmtKind::Return(expr)) => *expr,
        _ => panic!("expected return in stage body"),
    };

    assert!(matches!(
        residual.program().expr(staged_return_expr).map(|e| &e.kind),
        Some(ExprKind::Var(_))
    ));
    assert!(matches!(
        residual.bta().stage_of_expr.get(&staged_return_expr),
        Some(Stage::Ct)
    ));
}

#[test]
fn runtime_directive_and_handler_discharge_interact_consistently() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  let y = @runtime { handle { do Console.print("x"); 7 } with Console {
    | print(s) => 0
  } };
  y
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);

    let main = residual.program().functions().first().expect("main");
    assert_eq!(
        residual.sema().effects_of_stmt[main.body.index()],
        SortedEffectRow::empty()
    );

    let mut cursor = main.body;
    let mut stage_body = None;
    while let Some(stmt) = residual.program().stmt(cursor) {
        match &stmt.kind {
            StmtKind::Val { value, next, .. } => {
                if let Some(StmtKind::Stage { body, .. }) =
                    residual.program().stmt(*value).map(|n| &n.kind)
                {
                    stage_body = Some(*body);
                    break;
                }
                cursor = *next;
            }
            StmtKind::Let { next, .. } => cursor = *next,
            _ => break,
        }
    }

    let stage_body = stage_body.expect("expected runtime stage");
    let mut stack = vec![stage_body];
    let mut saw_runtime_expr = false;
    while let Some(stmt_id) = stack.pop() {
        let Some(stmt) = residual.program().stmt(stmt_id) else {
            continue;
        };
        match &stmt.kind {
            StmtKind::Return(expr) => {
                if matches!(residual.bta().stage_of_expr.get(expr), Some(Stage::Rt(_))) {
                    saw_runtime_expr = true;
                }
            }
            StmtKind::Let { value, next, .. } => {
                if matches!(residual.bta().stage_of_expr.get(value), Some(Stage::Rt(_))) {
                    saw_runtime_expr = true;
                }
                stack.push(*next);
            }
            StmtKind::Val { value, next, .. } => {
                stack.push(*value);
                stack.push(*next);
            }
            StmtKind::Call { args, next, .. } => {
                if args
                    .iter()
                    .any(|arg| matches!(residual.bta().stage_of_expr.get(arg), Some(Stage::Rt(_))))
                {
                    saw_runtime_expr = true;
                }
                stack.push(*next);
            }
            StmtKind::Perform { args, next, .. } => {
                if args
                    .iter()
                    .any(|arg| matches!(residual.bta().stage_of_expr.get(arg), Some(Stage::Rt(_))))
                {
                    saw_runtime_expr = true;
                }
                stack.push(*next);
            }
            StmtKind::Resume { arg, next, .. } => {
                if matches!(residual.bta().stage_of_expr.get(arg), Some(Stage::Rt(_))) {
                    saw_runtime_expr = true;
                }
                stack.push(*next);
            }
            StmtKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
                if matches!(residual.bta().stage_of_expr.get(cond), Some(Stage::Rt(_))) {
                    saw_runtime_expr = true;
                }
                stack.push(*then_branch);
                stack.push(*else_branch);
            }
            StmtKind::Match {
                scrutinee,
                arms,
                default,
            } => {
                if matches!(
                    residual.bta().stage_of_expr.get(scrutinee),
                    Some(Stage::Rt(_))
                ) {
                    saw_runtime_expr = true;
                }
                for arm in arms {
                    stack.push(arm.body);
                }
                if let Some(default_stmt) = default {
                    stack.push(*default_stmt);
                }
            }
            StmtKind::Handle { body, next, .. } | StmtKind::Stage { body, next, .. } => {
                stack.push(*body);
                if let Some(next_stmt) = next {
                    stack.push(*next_stmt);
                }
            }
            StmtKind::Hole { .. } | StmtKind::Error(_) => {}
        }
    }

    assert!(
        saw_runtime_expr,
        "expected at least one runtime-classified expr in runtime stage block"
    );
}

#[test]
fn ct_only_function_rejects_runtime_arguments() {
    let src = r#"
@comptime fn add1(x: Int) -> Int {
  x + 1
}
fn main() -> Int {
  let y = @runtime { 1 + 2 };
  add1(y)
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);

    assert!(
        residual
            .diagnostics()
            .entries()
            .iter()
            .any(|diag| diag.code == "BTA_CT_ONLY_RUNTIME_ARG")
    );
}

#[test]
fn ct_only_function_accepts_compile_time_arguments() {
    let src = r#"
@comptime fn add1(x: Int) -> Int {
  x + 1
}
fn main() -> Int {
  add1(2)
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);

    assert!(
        !residual
            .diagnostics()
            .entries()
            .iter()
            .any(|diag| diag.code == "BTA_CT_ONLY_RUNTIME_ARG")
    );
}

#[test]
fn ct_only_effect_marks_function_as_ct_only_for_runtime_arg_checks() {
    let src = r#"
effect ComptimeReadFiles { fn read(path: String) -> String }
fn load(path: String) -> String with ComptimeReadFiles {
  path
}
fn main() -> Int {
  let path = @runtime { "config.toml" };
  let _value = load(path);
  0
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);

    assert!(
        residual
            .diagnostics()
            .entries()
            .iter()
            .any(|diag| diag.code == "BTA_CT_ONLY_RUNTIME_ARG")
    );
}

#[test]
fn non_thunkable_effects_mark_call_result_runtime() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn ping() -> Int with Console {
  do Console.print("x");
  7
}
fn main() -> Int {
  let y = ping();
  y
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);

    assert!(residual.bta().stage_of_var.values().any(|stage| matches!(
        stage,
        Stage::Rt(cielo::pipeline::phases::Reason::EffectNotDischarged(_))
    )));
}

#[test]
fn target_word_size_changes_ct_integer_wrapping() {
    let src = r#"
fn main() -> Int {
  let x = 2147483647 + 1;
  x
}
"#;
    let mut interner_64 = Interner::new();
    let compiler_64 = Compiler::new(CompilerConfig::default());
    let residual_64 = compiler_64.compile_source_v0(src, SourceId::from_u32(0), &mut interner_64);

    let mut config_32 = CompilerConfig::default();
    config_32.target.word_size_bits = 32;
    config_32.target.pointer_alignment = 4;
    let compiler_32 = Compiler::new(config_32);
    let mut interner_32 = Interner::new();
    let residual_32 = compiler_32.compile_source_v0(src, SourceId::from_u32(0), &mut interner_32);

    let ints_64 = residual_64
        .ct()
        .ct_cache
        .values()
        .filter_map(|lit| match lit {
            Literal::Int(value) => Some(*value),
            _ => None,
        })
        .collect::<Vec<_>>();
    let ints_32 = residual_32
        .ct()
        .ct_cache
        .values()
        .filter_map(|lit| match lit {
            Literal::Int(value) => Some(*value),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert!(
        ints_64.contains(&2_147_483_648),
        "64-bit target should keep wide arithmetic result: {ints_64:?}"
    );
    assert!(
        ints_32.contains(&-2_147_483_648),
        "32-bit target should wrap arithmetic result: {ints_32:?}"
    );
}

#[test]
fn target_word_size_normalizes_ct_integer_literals() {
    let src = r#"
fn main() -> Int {
  let x = 2147483648;
  x
}
"#;
    let mut interner_64 = Interner::new();
    let residual_64 = Compiler::new(CompilerConfig::default()).compile_source_v0(
        src,
        SourceId::from_u32(0),
        &mut interner_64,
    );

    let mut cfg_32 = CompilerConfig::default();
    cfg_32.target.word_size_bits = 32;
    let mut interner_32 = Interner::new();
    let residual_32 =
        Compiler::new(cfg_32).compile_source_v0(src, SourceId::from_u32(1), &mut interner_32);

    let ints_64 = residual_64
        .ct()
        .ct_cache
        .values()
        .filter_map(|lit| match lit {
            Literal::Int(value) => Some(*value),
            _ => None,
        })
        .collect::<Vec<_>>();
    let ints_32 = residual_32
        .ct()
        .ct_cache
        .values()
        .filter_map(|lit| match lit {
            Literal::Int(value) => Some(*value),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert!(
        ints_64.contains(&2_147_483_648),
        "64-bit targets should keep wide integer literals: {ints_64:?}"
    );
    assert!(
        ints_32.contains(&-2_147_483_648),
        "32-bit targets should normalize integer literals to target width: {ints_32:?}"
    );
}

#[test]
fn ct_cache_key_and_eval_stats_reflect_target_endianness_and_alignment() {
    let src = r#"
fn main() -> Int {
  let x = 1 + 2;
  let y = x * 3;
  y
}
"#;
    let mut little_cfg = CompilerConfig::default();
    little_cfg.target.word_size_bits = 64;
    little_cfg.target.endianness = Endianness::Little;
    little_cfg.target.pointer_alignment = 8;
    let mut big_cfg = CompilerConfig::default();
    big_cfg.target.word_size_bits = 64;
    big_cfg.target.endianness = Endianness::Big;
    big_cfg.target.pointer_alignment = 16;

    let mut little_interner = Interner::new();
    let little_residual = Compiler::new(little_cfg).compile_source_v0(
        src,
        SourceId::from_u32(0),
        &mut little_interner,
    );
    let mut big_interner = Interner::new();
    let big_residual =
        Compiler::new(big_cfg).compile_source_v0(src, SourceId::from_u32(1), &mut big_interner);

    assert_eq!(little_residual.ct().cache_key.target_endianness, "little");
    assert_eq!(big_residual.ct().cache_key.target_endianness, "big");
    assert_eq!(little_residual.ct().cache_key.target_pointer_alignment, 8);
    assert_eq!(big_residual.ct().cache_key.target_pointer_alignment, 16);
    assert_ne!(
        little_residual.ct().cache_key,
        big_residual.ct().cache_key,
        "cache key must vary across endianness/alignment changes"
    );

    let mut ints_little = little_residual
        .ct()
        .ct_cache
        .values()
        .filter_map(|lit| match lit {
            Literal::Int(value) => Some(*value),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut ints_big = big_residual
        .ct()
        .ct_cache
        .values()
        .filter_map(|lit| match lit {
            Literal::Int(value) => Some(*value),
            _ => None,
        })
        .collect::<Vec<_>>();
    ints_little.sort_unstable();
    ints_big.sort_unstable();
    assert_eq!(
        ints_little, ints_big,
        "for pure integer arithmetic, endianness/alignment should not change folded values"
    );
    assert_eq!(
        little_residual.ct().eval_stats,
        big_residual.ct().eval_stats,
        "evaluator instrumentation should be stable when semantics are unchanged"
    );
}

#[test]
fn comptime_read_files_tracks_dependency_hash_changes() {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("cielo_ct_dep_{stamp}.txt"));
    fs::write(&path, "alpha").expect("write dep file");
    let path_text = path.to_string_lossy().replace('\\', "\\\\");

    let src = format!(
        r#"
effect ComptimeReadFiles {{ fn read(path: String) -> String }}
fn main() -> Int {{
  do ComptimeReadFiles.read("{path_text}");
  0
}}
"#
    );

    let compiler = Compiler::new(CompilerConfig::default());
    let mut interner_1 = Interner::new();
    let residual_1 =
        compiler.compile_source_v0(src.as_str(), SourceId::from_u32(0), &mut interner_1);
    let dep_1 = residual_1.ct().file_deps.first().expect("first dep");

    fs::write(&path, "beta").expect("rewrite dep file");
    let mut interner_2 = Interner::new();
    let residual_2 =
        compiler.compile_source_v0(src.as_str(), SourceId::from_u32(1), &mut interner_2);
    let dep_2 = residual_2
        .ct()
        .file_deps
        .first()
        .expect("first dep after edit");

    assert_eq!(
        dep_1.path, dep_2.path,
        "dependency identity path should stay stable"
    );
    assert_ne!(
        dep_1.content_hash, dep_2.content_hash,
        "dependency hash should change when file contents change"
    );
}

#[test]
fn bta_knownness_marks_cached_values_as_persistable_when_possible() {
    let src = r#"
fn main() -> Int {
  let x = 1 + 2;
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);

    assert!(
        !residual.ct().ct_cache.is_empty(),
        "ct cache must contain evaluated expressions"
    );
    for expr_id in residual.ct().ct_cache.keys() {
        assert!(
            matches!(
                residual.bta().knownness_of_expr.get(&expr_id),
                Some(Knownness::KnownPersistable)
            ),
            "cached expression e{} should be marked as persistable-known",
            expr_id.as_u32()
        );
    }
}
