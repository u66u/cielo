use cielo::common::ids::{EffectLabelId, LinearFuncId, SourceId, VarId};
use cielo::common::symbols::Interner;
use cielo::ir::core::Literal;
use cielo::ir::linear::{
    CallConvention, LinearExpr, LinearFunction, LinearMatchArm, LinearProgram, LinearStmt,
};
use cielo::passes::{c_emit, c_emit::emit_c_program, handler_specialize, linearize};
use cielo::pipeline::phases::{ConstantEmbedStrategy, ConstantKey, CtorFieldKey, ScalarLiteralKey};
use cielo::{Compiler, CompilerConfig};
use std::collections::HashSet;
use std::fmt::Write as _;

#[test]
fn emits_c_for_basic_arithmetic_program() {
    let src = r#"
fn main() -> Int {
  let x = 1 + 2;
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    assert_eq!(compiled.linear.functions.len(), 1);
    assert!(compiled.c_source.contains("cv_add("));
    assert!(compiled.c_source.contains("int main(void)"));
    assert!(compiled.residual.diagnostics().entries().is_empty());
}

#[test]
fn emits_runtime_stub_calls_for_effect_operations() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  do Console.print("hello");
  0
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    let perform_line = find_perform_call_line(&compiled.c_source, "print")
        .expect("expected runtime perform stub for Console.print");
    let print_symbol = parse_perform_symbol_id(perform_line, 0, "print", 1)
        .expect("perform stub should carry an op symbol id");
    let expected_print_symbol = interner.intern("print").as_u32();
    assert_eq!(
        print_symbol, expected_print_symbol,
        "perform stubs should thread symbol-prelude ids for op dispatch"
    );
    assert!(
        compiled
            .c_source
            .contains(format!("cielo_perform(0, {expected_print_symbol}, \"print\", 1").as_str()),
        "perform stubs should pass effect id + op symbol id + op name + arity"
    );
    assert!(compiled.c_source.contains("print"));
}

#[test]
fn source_if_with_ct_condition_is_pruned_before_linear_ir() {
    let src = r#"
fn main() -> Int {
  let x = if true { 1 } else { 2 };
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);
    let main = compiled
        .linear
        .functions
        .iter()
        .find(|function| interner.resolve(function.name) == Some("main"))
        .expect("main function");

    assert!(
        !linear_stmt_graph_contains_if(&compiled.linear, main.body),
        "ct-known source if should be pruned before linearization"
    );
}

#[test]
fn source_if_with_alias_ct_condition_is_pruned_before_linear_ir() {
    let src = r#"
fn main() -> Int {
  let c0 = true;
  let c1 = c0;
  let x = if c1 { 1 } else { 2 };
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);
    let main = compiled
        .linear
        .functions
        .iter()
        .find(|function| interner.resolve(function.name) == Some("main"))
        .expect("main function");

    assert!(
        !linear_stmt_graph_contains_if(&compiled.linear, main.body),
        "alias-to-literal ct condition should be pruned before linearization"
    );
    assert!(
        linear_stmt_graph_contains_literal_int(&compiled.linear, main.body, 1),
        "selected branch payload should remain"
    );
}

#[test]
fn source_match_with_known_variant_is_pruned_before_linear_ir() {
    let src = r#"
enum Flag { On, Off }
fn main() -> Int {
  let x = match On() {
    | On => 1
    | _ => 0
  };
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);
    let main = compiled
        .linear
        .functions
        .iter()
        .find(|function| interner.resolve(function.name) == Some("main"))
        .expect("main function");

    assert!(
        !linear_stmt_graph_contains_match(&compiled.linear, main.body),
        "known-variant source match should be pruned before linearization"
    );
}

#[test]
fn source_match_with_binder_is_pruned_and_binder_flow_is_preserved() {
    let src = r#"
enum OptionI { Some(Int), None }
fn main() -> Int {
  let x = match Some(41) {
    | Some(v) => v + 1
    | _ => 0
  };
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);
    let main = compiled
        .linear
        .functions
        .iter()
        .find(|function| interner.resolve(function.name) == Some("main"))
        .expect("main function");

    assert!(
        !linear_stmt_graph_contains_match(&compiled.linear, main.body),
        "known-variant source match with binders should be pruned before linearization"
    );
    assert!(
        linear_stmt_graph_contains_literal_int(&compiled.linear, main.body, 41),
        "selected match arm should preserve payload literal flow"
    );
    assert!(
        linear_stmt_graph_contains_add_rhs_int(&compiled.linear, main.body, 1),
        "selected match arm body should remain after pruning"
    );
}

#[test]
fn source_match_with_alias_scrutinee_and_binder_is_pruned() {
    let src = r#"
enum OptionI { Some(Int), None }
fn main() -> Int {
  let s0 = Some(41);
  let s1 = s0;
  let x = match s1 {
    | Some(v) => v + 1
    | _ => 0
  };
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);
    let main = compiled
        .linear
        .functions
        .iter()
        .find(|function| interner.resolve(function.name) == Some("main"))
        .expect("main function");

    assert!(
        !linear_stmt_graph_contains_match(&compiled.linear, main.body),
        "alias-to-constructor scrutinee should still prune match before linearization"
    );
    assert!(
        linear_stmt_graph_contains_literal_int(&compiled.linear, main.body, 41),
        "selected arm should preserve payload flow"
    );
    assert!(
        linear_stmt_graph_contains_add_rhs_int(&compiled.linear, main.body, 1),
        "selected arm computation should remain after pruning"
    );
}

#[test]
fn c_emitter_dedups_string_literals_in_const_pool() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  do Console.print("same");
  do Console.print("same");
  0
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    assert_eq!(
        compiled
            .c_source
            .matches("static const char* cielo_const_s_")
            .count(),
        1,
        "duplicated string literals should be pooled exactly once"
    );
    assert!(
        compiled.c_source.contains("cv_string(cielo_const_s_0)"),
        "pooled string should be referenced through const symbol"
    );
}

#[test]
fn c_emitter_respects_const_pool_entry_size_cap() {
    let long = "a".repeat(1100);
    let src = format!(
        r#"
effect Console {{ fn print(s: String) -> () }}
fn main() -> Int {{
  do Console.print("{long}");
  0
}}
"#
    );
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled =
        compiler.compile_source_v0_to_c(src.as_str(), SourceId::from_u32(0), &mut interner);

    assert!(
        !compiled
            .c_source
            .contains("static const char* cielo_const_s_"),
        "oversized literals should not be added to const pool"
    );
    assert!(
        compiled.c_source.contains("cv_string(\""),
        "oversized literals should still be emitted inline"
    );
}

#[test]
fn c_emitter_pools_repeated_runtime_int_literals() {
    let src = r#"
fn main() -> Int {
  @runtime {
    let a = 7;
    let b = 7;
    a + b
  }
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    assert!(
        compiled
            .residual
            .residual()
            .constant_table
            .entries
            .iter()
            .any(|entry| {
                matches!(entry.key, ConstantKey::Scalar(ScalarLiteralKey::Int(7)))
                    && matches!(entry.strategy, ConstantEmbedStrategy::StaticConst)
            }),
        "residualize+specialize should precompute scalar embedding strategy in ConstantTable"
    );
    assert_eq!(
        compiled
            .c_source
            .matches("static const CieloValue cielo_const_v_")
            .count(),
        1,
        "repeated runtime scalar literals should emit one pooled CieloValue constant"
    );
    assert!(
        compiled.c_source.matches("= cielo_const_v_0;").count() >= 2,
        "pooled scalar literal symbol should be reused at runtime callsites"
    );
}

#[test]
fn c_emitter_keeps_single_use_runtime_int_literal_inline() {
    let src = r#"
fn main() -> Int {
  @runtime {
    let a = 7;
    a
  }
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    assert!(
        !compiled
            .c_source
            .contains("static const CieloValue cielo_const_v_"),
        "single-use runtime scalar literals should stay inline to avoid pool bloat"
    );
}

#[test]
fn c_emitter_pools_repeated_runtime_float_literals() {
    let mut interner = Interner::new();
    let main_name = interner.intern("main");

    let mut program = LinearProgram::default();
    let first = program.push_expr(LinearExpr::Literal(Literal::Float(1.5)));
    let second = program.push_expr(LinearExpr::Literal(Literal::Float(1.5)));
    let zero = program.push_expr(LinearExpr::Literal(Literal::Int(0)));

    let ret = program.push_stmt(LinearStmt::Return(zero));
    let second_let = program.push_stmt(LinearStmt::Let {
        binding: VarId::from_u32(1),
        value: second,
        next: ret,
    });
    let body = program.push_stmt(LinearStmt::Let {
        binding: VarId::from_u32(0),
        value: first,
        next: second_let,
    });

    program.functions.push(LinearFunction {
        id: LinearFuncId::new(0),
        name: main_name,
        params: vec![],
        body,
    });
    program.entrypoints = vec![LinearFuncId::new(0)];

    let emitted = emit_c_program(&program, &interner);

    let pooled_symbol = emitted
        .lines()
        .find_map(|line| {
            if !line.starts_with("static const CieloValue cielo_const_v_")
                || !line.contains(".tag = CV_FLOAT")
            {
                return None;
            }
            line.split_whitespace()
                .nth(3)
                .map(|symbol| symbol.trim_end_matches(';').to_owned())
        })
        .expect("expected pooled float constant");

    assert!(
        emitted
            .matches(format!("= {pooled_symbol};").as_str())
            .count()
            >= 2,
        "pooled float literal symbol should be reused at runtime callsites"
    );
}

#[test]
fn c_emitter_pools_repeated_runtime_ctor_literals() {
    let src = r#"
enum Pair { Mk(Int, Int) }
fn main() -> Int {
  @runtime {
    let a = Mk(1, 2);
    let b = Mk(1, 2);
    match b {
      | Mk(x, y) => x + y
      | _ => 0
    }
  }
}
"#;
    let mut interner = Interner::new();
    let compiled =
        compile_source_v0_to_c_without_normalize(src, SourceId::from_u32(0), &mut interner);

    assert_eq!(
        compiled
            .c_source
            .matches("static const CieloValue cielo_const_ctor_v_")
            .count(),
        1,
        "repeated runtime ctor literals should emit one pooled ctor value"
    );
    assert!(
        compiled.c_source.matches("= cielo_const_ctor_v_0;").count() >= 2,
        "pooled ctor symbol should be reused across repeated runtime constructor sites"
    );
    assert!(
        !compiled
            .c_source
            .contains("cielo_make_ctor(\"Pair\", \"Mk\", 2"),
        "pooled repeated ctor literals should avoid repeated heap ctor construction"
    );
}

#[test]
fn c_emitter_pools_repeated_runtime_ctor_literals_with_float_fields() {
    let mut interner = Interner::new();
    let main_name = interner.intern("main");
    let pair_name = interner.intern("PairF");
    let mk_name = interner.intern("Mk");

    let mut program = LinearProgram::default();
    let first = program.push_expr(LinearExpr::Literal(Literal::Float(1.25)));
    let second = program.push_expr(LinearExpr::Literal(Literal::Float(2.5)));
    let ctor_a = program.push_expr(LinearExpr::MakeEnum {
        ty: pair_name,
        variant: mk_name,
        fields: vec![first, second],
    });
    let ctor_b = program.push_expr(LinearExpr::MakeEnum {
        ty: pair_name,
        variant: mk_name,
        fields: vec![first, second],
    });
    let zero = program.push_expr(LinearExpr::Literal(Literal::Int(0)));

    let ret = program.push_stmt(LinearStmt::Return(zero));
    let second_let = program.push_stmt(LinearStmt::Let {
        binding: VarId::from_u32(1),
        value: ctor_b,
        next: ret,
    });
    let body = program.push_stmt(LinearStmt::Let {
        binding: VarId::from_u32(0),
        value: ctor_a,
        next: second_let,
    });

    program.functions.push(LinearFunction {
        id: LinearFuncId::new(0),
        name: main_name,
        params: vec![],
        body,
    });
    program.entrypoints = vec![LinearFuncId::new(0)];

    let emitted = emit_c_program(&program, &interner);

    assert_eq!(
        emitted
            .matches("static const CieloValue cielo_const_ctor_v_")
            .count(),
        1,
        "repeated runtime float ctor literals should emit one pooled ctor value"
    );
    assert!(
        emitted.contains(".tag = CV_FLOAT"),
        "pooled ctor field table should preserve float field payloads"
    );
    assert!(
        !emitted.contains("cielo_make_ctor(\"PairF\", \"Mk\", 2"),
        "pooled repeated float ctor literals should avoid repeated inline ctor construction"
    );
}

#[test]
fn c_emitter_pools_repeated_runtime_nested_ctor_literals() {
    let src = r#"
enum Pair { Mk(Int, Int) }
enum Boxed { Wrap(Pair, Pair) }
fn main() -> Int {
  @runtime {
    let a = Wrap(Mk(1, 2), Mk(3, 4));
    let b = Wrap(Mk(1, 2), Mk(3, 4));
    match b {
      | Wrap(left, right) => 0
      | _ => 0
    }
  }
}
"#;
    let mut interner = Interner::new();
    let compiled =
        compile_source_v0_to_c_without_normalize(src, SourceId::from_u32(0), &mut interner);

    assert!(
        compiled
            .residual
            .residual()
            .constant_table
            .entries
            .iter()
            .any(|entry| match (&entry.key, entry.strategy) {
                (ConstantKey::Ctor(key), ConstantEmbedStrategy::Pooled) => {
                    interner.resolve(key.ty) == Some("Boxed")
                        && interner.resolve(key.variant) == Some("Wrap")
                        && key
                            .fields
                            .iter()
                            .all(|field| matches!(field, CtorFieldKey::Ctor(_)))
                }
                _ => false,
            }),
        "constant table should pool the outer nested constructor and keep nested ctor field keys"
    );
    assert!(
        compiled
            .c_source
            .matches("static const CieloValue cielo_const_ctor_v_")
            .count()
            >= 1,
        "nested ctor pooling should still emit pooled ctor values"
    );
    assert!(
        compiled
            .c_source
            .contains("static CieloCtor cielo_const_ctor_nested_"),
        "nested ctor fields should materialize nested static ctor descriptors"
    );
    assert!(
        !compiled
            .c_source
            .contains("cielo_make_ctor(\"Boxed\", \"Wrap\", 2"),
        "pooled nested ctor literals should avoid inline outer ctor construction"
    );
}

#[test]
fn c_emitter_keeps_single_use_runtime_ctor_literal_inline() {
    let src = r#"
enum Pair { Mk(Int, Int) }
fn main() -> Int {
  @runtime {
    let a = Mk(1, 2);
    match a {
      | Mk(x, y) => x + y
      | _ => 0
    }
  }
}
"#;
    let mut interner = Interner::new();
    let compiled =
        compile_source_v0_to_c_without_normalize(src, SourceId::from_u32(0), &mut interner);

    assert!(
        !compiled
            .c_source
            .contains("static const CieloValue cielo_const_ctor_v_"),
        "single-use ctor literals should stay inline to avoid pool bloat"
    );
    assert!(
        compiled
            .c_source
            .contains("cielo_make_ctor(\"Pair\", \"Mk\", 2"),
        "single-use ctor literal should still lower via inline ctor helper call"
    );
}

#[test]
fn c_emitter_pools_large_single_use_runtime_ctor_literal() {
    let src = r#"
enum Blob { Mk(
  Int, Int, Int, Int, Int, Int, Int, Int,
  Int, Int, Int, Int, Int, Int, Int, Int,
  Int, Int, Int, Int, Int, Int, Int, Int
) }
fn main() -> Int {
  @runtime {
    let a = Mk(
      1, 2, 3, 4, 5, 6, 7, 8,
      9, 10, 11, 12, 13, 14, 15, 16,
      17, 18, 19, 20, 21, 22, 23, 24
    );
    match a {
      | Mk(
          x0, x1, x2, x3, x4, x5, x6, x7,
          x8, x9, x10, x11, x12, x13, x14, x15,
          x16, x17, x18, x19, x20, x21, x22, x23
        ) => x0
      | _ => 0
    }
  }
}
"#;
    let mut interner = Interner::new();
    let compiled =
        compile_source_v0_to_c_without_normalize(src, SourceId::from_u32(0), &mut interner);

    assert_eq!(
        compiled
            .c_source
            .matches("static const CieloValue cielo_const_ctor_v_")
            .count(),
        1,
        "large single-use ctor literals should be pooled by size policy"
    );
    assert!(
        compiled
            .c_source
            .contains("static CieloCtor cielo_const_ctor_0 ="),
        "pooled large ctor literal should emit a static ctor descriptor"
    );
    assert!(
        !compiled
            .c_source
            .contains("cielo_make_ctor(\"Blob\", \"Mk\", 24"),
        "pooled large single-use ctor literal should not fall back to inline ctor creation"
    );
}

#[test]
fn c_emitter_skips_oversized_ctor_pool_entry_and_falls_back_inline() {
    let long = "x".repeat(1300);
    let src = format!(
        r#"
enum Pair {{ Mk(String, Int) }}
fn main() -> Int {{
  @runtime {{
    let a = Mk("{long}", 1);
    let b = Mk("{long}", 1);
    match b {{
      | Mk(s, x) => x
      | _ => 0
    }}
  }}
}}
"#
    );
    let mut interner = Interner::new();
    let compiled = compile_source_v0_to_c_without_normalize(
        src.as_str(),
        SourceId::from_u32(0),
        &mut interner,
    );

    assert!(
        !compiled
            .c_source
            .contains("static const CieloValue cielo_const_ctor_v_"),
        "oversized ctor constants should not be pooled"
    );
    assert!(
        compiled
            .c_source
            .contains("cielo_make_ctor(\"Pair\", \"Mk\", 2"),
        "oversized pooled candidates should fall back to inline constructor creation"
    );
}

#[test]
fn c_emitter_limits_ctor_pool_by_compilation_unit_budget() {
    let ctor_arity = 32usize;
    let repeated_unique_ctors = 40usize;
    let mut body = String::new();
    for idx in 0..repeated_unique_ctors {
        let args = (0..ctor_arity)
            .map(|offset| (idx + offset).to_string())
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(&mut body, "    let a{idx} = Mk({args});").expect("append ctor a");
        writeln!(&mut body, "    let b{idx} = Mk({args});").expect("append ctor b");
        writeln!(&mut body, "    do Sink.use(a{idx});").expect("append sink use a");
        writeln!(&mut body, "    do Sink.use(b{idx});").expect("append sink use b");
    }
    body.push_str("    0\n");

    let fields = (0..ctor_arity)
        .map(|_| "Int")
        .collect::<Vec<_>>()
        .join(", ");
    let src = format!(
        "enum Blob {{ Mk({fields}) }}\neffect Sink {{ fn use(v: Blob) -> () }}\nfn main() -> Int {{\n  @runtime {{\n{body}  }}\n}}\n"
    );
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled =
        compiler.compile_source_v0_to_c(src.as_str(), SourceId::from_u32(0), &mut interner);

    let pooled_values = compiled
        .c_source
        .matches("static const CieloValue cielo_const_ctor_v_")
        .count();
    let pooled_ctors = compiled
        .c_source
        .matches("static CieloCtor cielo_const_ctor_")
        .count();

    assert!(
        pooled_values > 0,
        "budgeted ctor pool should still keep hot literals"
    );
    assert!(
        pooled_values < repeated_unique_ctors,
        "pool byte cap should stop before pooling every repeated ctor literal"
    );
    assert_eq!(
        pooled_values, pooled_ctors,
        "each pooled ctor value should have exactly one pooled ctor descriptor"
    );
    assert!(
        compiled
            .c_source
            .contains("cielo_make_ctor(\"Blob\", \"Mk\", 32"),
        "when pool budget is saturated, remaining ctor literals should stay inline"
    );
}

#[test]
fn lowers_handled_perform_into_clause_without_runtime_dispatch() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  let x = handle { do Console.print("hello"); 1 } with Console {
    | print(s) => 0
  };
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    assert!(
        !compiled.c_source.contains("= cielo_handler_push(0);"),
        "handled callsites should not emit runtime handler push/pop"
    );
    assert!(
        find_perform_call_line(&compiled.c_source, "print").is_none(),
        "handled operations should lower into clause bodies instead of runtime perform stubs"
    );
    assert!(
        compiled.c_source.contains("cv_int(0)"),
        "handler clause return should be reflected in emitted C body"
    );
}

#[test]
fn c_emitter_binds_distinct_capabilities_per_handler_installation() {
    let mut interner = Interner::new();
    let main_name = interner.intern("main");

    let mut program = LinearProgram::default();
    let zero = program.push_expr(LinearExpr::Literal(Literal::Int(0)));
    let ret = program.push_stmt(LinearStmt::Return(zero));
    let inner_handle = program.push_stmt(LinearStmt::Handle {
        effect: EffectLabelId::from_u32(0),
        body: ret,
        next: None,
    });
    let outer_handle = program.push_stmt(LinearStmt::Handle {
        effect: EffectLabelId::from_u32(0),
        body: inner_handle,
        next: None,
    });

    program.functions.push(LinearFunction {
        id: LinearFuncId::new(0),
        name: main_name,
        params: vec![],
        body: outer_handle,
    });
    program.entrypoints = vec![LinearFuncId::new(0)];

    let emitted = emit_c_program(&program, &interner);
    let capabilities = handler_push_capability_temps_for_effect(&emitted, 0);

    assert_eq!(
        capabilities.len(),
        2,
        "each handler installation should bind a capability id from push"
    );
    assert_ne!(
        capabilities[0], capabilities[1],
        "nested handler installations should use distinct capability bindings"
    );
    for capability in capabilities {
        assert!(
            emitted.contains(format!("cielo_handler_pop({capability});").as_str()),
            "handler pops should use the bound capability id, not raw effect id"
        );
    }
    assert!(
        !emitted.contains("cielo_handler_pop(0);"),
        "handler pops should not target effect labels directly"
    );
}

#[test]
fn c_emitter_materializes_handler_evidence_records() {
    let mut interner = Interner::new();
    let main_name = interner.intern("main");

    let mut program = LinearProgram::default();
    let zero = program.push_expr(LinearExpr::Literal(Literal::Int(0)));
    let ret = program.push_stmt(LinearStmt::Return(zero));
    let handle = program.push_stmt(LinearStmt::Handle {
        effect: EffectLabelId::from_u32(0),
        body: ret,
        next: None,
    });
    program.functions.push(LinearFunction {
        id: LinearFuncId::new(0),
        name: main_name,
        params: vec![],
        body: handle,
    });
    program.entrypoints = vec![LinearFuncId::new(0)];

    let emitted = emit_c_program(&program, &interner);
    assert!(
        emitted.contains("CieloEvidence __cielo_evidence_"),
        "handler lowering should materialize a concrete evidence struct per installation"
    );
    assert!(
        emitted.contains("cielo_handler_push_with_evidence(0, &__cielo_evidence_"),
        "handler installation should bind capability and evidence together"
    );
}

#[test]
fn c_emitter_threads_capability_into_scoped_perform_calls() {
    let mut interner = Interner::new();
    let main_name = interner.intern("main");
    let tick_op = interner.intern("tick");

    let mut program = LinearProgram::default();
    let arg = program.push_expr(LinearExpr::Literal(Literal::Int(1)));
    let ret_value = program.push_expr(LinearExpr::Literal(Literal::Int(0)));
    let ret = program.push_stmt(LinearStmt::Return(ret_value));
    let perform = program.push_stmt(LinearStmt::Perform {
        result: None,
        effect: EffectLabelId::from_u32(0),
        operation: tick_op,
        args: vec![arg],
        next: ret,
    });
    let handle = program.push_stmt(LinearStmt::Handle {
        effect: EffectLabelId::from_u32(0),
        body: perform,
        next: None,
    });

    program.functions.push(LinearFunction {
        id: LinearFuncId::new(0),
        name: main_name,
        params: vec![],
        body: handle,
    });
    program.entrypoints = vec![LinearFuncId::new(0)];

    let emitted = emit_c_program(&program, &interner);
    let capabilities = handler_push_capability_temps_for_effect(&emitted, 0);
    assert_eq!(
        capabilities.len(),
        1,
        "single handler installation should produce one capability binding"
    );

    let scoped_call = format!(
        "(void)cielo_perform_scoped(0, {}, {}, \"tick\", 1",
        capabilities[0],
        tick_op.as_u32()
    );
    assert!(
        emitted.contains(scoped_call.as_str()),
        "perform in matching handler scope should pass lexical capability id"
    );
}

#[test]
fn c_emitter_uses_nearest_capability_for_nested_same_effect_handlers() {
    let mut interner = Interner::new();
    let main_name = interner.intern("main");
    let tick_op = interner.intern("tick");

    let mut program = LinearProgram::default();
    let one = program.push_expr(LinearExpr::Literal(Literal::Int(1)));
    let two = program.push_expr(LinearExpr::Literal(Literal::Int(2)));
    let zero = program.push_expr(LinearExpr::Literal(Literal::Int(0)));

    let final_ret = program.push_stmt(LinearStmt::Return(zero));
    let outer_perform_after_inner = program.push_stmt(LinearStmt::Perform {
        result: None,
        effect: EffectLabelId::from_u32(0),
        operation: tick_op,
        args: vec![two],
        next: final_ret,
    });
    let inner_ret = program.push_stmt(LinearStmt::Return(zero));
    let inner_perform = program.push_stmt(LinearStmt::Perform {
        result: None,
        effect: EffectLabelId::from_u32(0),
        operation: tick_op,
        args: vec![one],
        next: inner_ret,
    });
    let inner_handle = program.push_stmt(LinearStmt::Handle {
        effect: EffectLabelId::from_u32(0),
        body: inner_perform,
        next: Some(outer_perform_after_inner),
    });
    let outer_handle = program.push_stmt(LinearStmt::Handle {
        effect: EffectLabelId::from_u32(0),
        body: inner_handle,
        next: None,
    });
    program.functions.push(LinearFunction {
        id: LinearFuncId::new(0),
        name: main_name,
        params: vec![],
        body: outer_handle,
    });
    program.entrypoints = vec![LinearFuncId::new(0)];

    let emitted = emit_c_program(&program, &interner);
    let capabilities = handler_push_capability_temps_for_effect(&emitted, 0);
    assert_eq!(
        capabilities.len(),
        2,
        "nested same-effect handlers should create distinct capability bindings"
    );
    let perform_caps = scoped_perform_capability_temps_for_effect(&emitted, 0);
    assert_eq!(
        perform_caps.len(),
        2,
        "expected scoped perform calls in both inner and outer handler regions"
    );
    assert_eq!(
        perform_caps[0], capabilities[1],
        "perform inside inner handler must target inner capability"
    );
    assert_eq!(
        perform_caps[1], capabilities[0],
        "perform after inner pop must target outer capability"
    );
}

#[test]
fn lowers_resumptive_clause_into_continuation_flow() {
    let src = r#"
effect LocalState { fn tick() -> Int }
fn main() -> Int {
  let x = handle { do LocalState.tick(); 9 } with LocalState {
    | tick(resume) => resume(41)
  };
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);
    let main = compiled
        .linear
        .functions
        .iter()
        .find(|function| interner.resolve(function.name) == Some("main"))
        .expect("main function");

    assert!(
        find_perform_call_line(&compiled.c_source, "tick").is_none(),
        "handled resumptive operations should not call runtime perform stubs"
    );
    assert!(
        compiled.c_source.contains("cv_int(9)"),
        "resumptive clause should continue into the operation continuation"
    );
    assert!(
        linear_stmt_graph_tail_resume_wrapper_count(&compiled.linear, main.body) == 1,
        "tail resumptions should leave exactly one identity wrapper shape after TR optimization"
    );
    assert!(
        compiled
            .residual
            .diagnostics()
            .entries()
            .iter()
            .any(|diag| {
                diag.code == "LINEARIZE_RESUME_LOWERING_GATE"
                    && diag.message.contains("Direct path selected")
            }),
        "tail-resumptive clauses should report a direct-path lowering gate decision"
    );
}

#[test]
fn lowers_non_tail_resume_with_control_flow_after_resumption() {
    let src = r#"
effect LocalState { fn tick() -> Int }
fn main() -> Int {
  let x = handle { do LocalState.tick(); 9 } with LocalState {
    | tick(resume) => {
      let y = resume(41);
      y + 1
    }
  };
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);
    let main = compiled
        .linear
        .functions
        .iter()
        .find(|function| interner.resolve(function.name) == Some("main"))
        .expect("main function");

    assert!(
        !linear_stmt_graph_contains_perform_effect(&compiled.linear, main.body, 0),
        "handled resumptions should not leave residual perform dispatch for handled effect"
    );
    assert!(
        linear_stmt_graph_contains_literal_int(&compiled.linear, main.body, 9),
        "continuation value should still flow from resumed branch"
    );
    assert!(
        linear_stmt_graph_contains_add_rhs_int(&compiled.linear, main.body, 1),
        "non-tail code after resume should be preserved"
    );
    assert!(
        compiled
            .residual
            .diagnostics()
            .entries()
            .iter()
            .any(|diag| {
                diag.code == "LINEARIZE_RESUME_LOWERING_GATE"
                    && diag.message.contains("Control path selected")
            }),
        "non-tail resumptive clauses should report a control-path lowering gate decision"
    );
}

#[test]
fn keeps_resume_after_intermediate_effect_on_control_path() {
    let src = r#"
effect LocalState { fn tick() -> Int }
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  let x = handle { do LocalState.tick(); 9 } with LocalState {
    | tick(resume) => {
      do Console.print("edge");
      resume(41)
    }
  };
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);
    let main = compiled
        .linear
        .functions
        .iter()
        .find(|function| interner.resolve(function.name) == Some("main"))
        .expect("main function");

    assert!(
        !compiled
            .residual
            .diagnostics()
            .entries()
            .iter()
            .any(|diag| diag.code == "LINEARIZE_DIRECT_RESUME_NON_TAIL"),
        "intermediate-effect resume path should classify as control, not direct"
    );
    assert!(
        linear_stmt_graph_contains_perform_effect(&compiled.linear, main.body, 1),
        "non-handled intermediate effect in clause should be preserved on linear path"
    );
    assert!(
        linear_stmt_graph_tail_resume_wrapper_count(&compiled.linear, main.body) >= 2,
        "control-path resumptions should not collapse to the direct-tail wrapper shape"
    );
}

#[test]
fn reports_multi_shot_resume_in_handler_clause() {
    let src = r#"
effect LocalState { fn tick() -> Int }
fn main() -> Int {
  let x = handle { do LocalState.tick(); 9 } with LocalState {
    | tick(resume) => {
      let a = resume(41);
      resume(a)
    }
  };
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    assert!(
        compiled
            .residual
            .diagnostics()
            .entries()
            .iter()
            .any(|diag| diag.code == "LINEARIZE_MULTI_SHOT_RESUME"),
        "multi-shot resume usage should produce a dedicated linearization diagnostic"
    );
    assert!(
        compiled
            .residual
            .diagnostics()
            .entries()
            .iter()
            .any(|diag| {
                diag.code == "LINEARIZE_RESUME_QUALIFIER" && diag.message.contains("Multi")
            }),
        "multi-shot clauses should report the resume qualifier"
    );
}

#[test]
fn allows_branch_exclusive_single_shot_resume_paths() {
    let src = r#"
effect LocalState { fn tick(flag: Bool) -> Int }
fn main() -> Int {
  let x = handle { do LocalState.tick(true); 9 } with LocalState {
    | tick(flag, resume) => {
      if flag {
        resume(41)
      } else {
        resume(42)
      }
    }
  };
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);
    let main = compiled
        .linear
        .functions
        .iter()
        .find(|function| interner.resolve(function.name) == Some("main"))
        .expect("main function");

    assert!(
        !compiled
            .residual
            .diagnostics()
            .entries()
            .iter()
            .any(|diag| diag.code == "LINEARIZE_MULTI_SHOT_RESUME"),
        "distinct branches that each resume once should be accepted as single-shot"
    );
    assert!(
        !compiled
            .residual
            .diagnostics()
            .entries()
            .iter()
            .any(|diag| diag.code == "LINEARIZE_DIRECT_RESUME_NON_TAIL"),
        "branch-exclusive tail resumptions should stay on direct tail paths"
    );
    assert!(
        !linear_stmt_graph_contains_perform_effect(&compiled.linear, main.body, 0),
        "handled branch-exclusive resumptions should not leave residual perform dispatch"
    );
    assert!(
        linear_stmt_graph_tail_resume_wrapper_count(&compiled.linear, main.body) >= 1,
        "single-shot branch resumptions should still lower through tail-resumption wrappers"
    );
}

#[test]
fn reports_multi_shot_when_one_branch_resumes_twice_on_the_same_path() {
    let src = r#"
effect LocalState { fn tick(flag: Bool) -> Int }
fn main() -> Int {
  let x = handle { do LocalState.tick(false); 9 } with LocalState {
    | tick(flag, resume) => {
      if flag {
        resume(41)
      } else {
        let y = resume(42);
        resume(y)
      }
    }
  };
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    assert!(
        compiled
            .residual
            .diagnostics()
            .entries()
            .iter()
            .any(|diag| diag.code == "LINEARIZE_MULTI_SHOT_RESUME"),
        "a control-flow path with two resumes must still be rejected as multi-shot"
    );
}

#[test]
fn classifies_affine_resume_qualifier_for_optional_resume_paths() {
    let src = r#"
effect LocalState { fn tick(flag: Bool) -> Int }
fn main() -> Int {
  let x = handle { do LocalState.tick(true); 9 } with LocalState {
    | tick(flag, resume) => {
      if flag {
        resume(41)
      } else {
        0
      }
    }
  };
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    assert!(
        compiled
            .residual
            .diagnostics()
            .entries()
            .iter()
            .any(|diag| {
                diag.code == "LINEARIZE_RESUME_QUALIFIER" && diag.message.contains("Affine")
            }),
        "branches with optional resume should classify as Affine"
    );
}

#[test]
fn classifies_linear_resume_qualifier_when_all_paths_resume_once() {
    let src = r#"
effect LocalState { fn tick(flag: Bool) -> Int }
fn main() -> Int {
  let x = handle { do LocalState.tick(true); 9 } with LocalState {
    | tick(flag, resume) => {
      if flag {
        resume(41)
      } else {
        resume(42)
      }
    }
  };
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    assert!(
        compiled
            .residual
            .diagnostics()
            .entries()
            .iter()
            .any(|diag| {
                diag.code == "LINEARIZE_RESUME_QUALIFIER" && diag.message.contains("Linear")
            }),
        "exactly-once resume paths should classify as Linear"
    );
}

#[test]
fn classifies_abortive_resume_qualifier_when_resume_binder_is_unused() {
    let src = r#"
effect LocalState { fn tick() -> Int }
fn main() -> Int {
  let x = handle { do LocalState.tick(); 9 } with LocalState {
    | tick(resume) => 41
  };
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    assert!(
        compiled
            .residual
            .diagnostics()
            .entries()
            .iter()
            .any(|diag| {
                diag.code == "LINEARIZE_RESUME_QUALIFIER" && diag.message.contains("Abortive")
            }),
        "unused resume binders should classify as Abortive"
    );
}

#[test]
fn lowers_mixed_direct_and_control_resume_clauses_in_one_handler() {
    let src = r#"
effect LocalState {
  fn tick() -> Int
  fn bump(flag: Bool) -> Int
}
effect Console { fn print(s: String) -> () }

fn runtime_flag() -> Bool with Console {
  do Console.print("flag");
  true
}

fn main() -> Int {
  let x = handle {
    do LocalState.tick();
    do LocalState.bump(runtime_flag());
    5
  } with LocalState {
    | tick(resume) => resume(40)
    | bump(flag, resume) => {
      if flag {
        let y = resume(1);
        y + 1
      } else {
        resume(2)
      }
    }
  };
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);
    let main = compiled
        .linear
        .functions
        .iter()
        .find(|function| interner.resolve(function.name) == Some("main"))
        .expect("main function");

    let diagnostics = compiled.residual.diagnostics().entries();
    assert!(
        diagnostics.iter().any(|diag| {
            diag.code == "LINEARIZE_RESUME_LOWERING_GATE"
                && diag.message.contains("Direct path selected")
        }),
        "direct resumptive clauses should report a direct lowering gate in mixed handlers"
    );
    assert!(
        diagnostics.iter().any(|diag| {
            diag.code == "LINEARIZE_RESUME_LOWERING_GATE"
                && diag.message.contains("Control path selected")
        }),
        "non-tail resumptive clauses should report a control lowering gate in mixed handlers"
    );
    assert!(
        !diagnostics
            .iter()
            .any(|diag| diag.code == "LINEARIZE_HANDLED_EFFECT_LEAK"),
        "mixed clause lowering should not leak handled performs across the linear boundary"
    );
    assert!(
        !linear_stmt_graph_contains_perform_effect(&compiled.linear, main.body, 0),
        "handled effects should not survive as residual runtime perform calls"
    );
}

#[test]
fn eliminates_dead_handler_with_empty_handled_effect_intersection() {
    let src = r#"
effect LocalState { fn tick() -> Int }
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  let x = handle {
    do Console.print("hello");
    41
  } with LocalState {
    | tick(resume) => resume(0)
  };
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);
    let main = compiled
        .linear
        .functions
        .iter()
        .find(|function| interner.resolve(function.name) == Some("main"))
        .expect("main function");

    let diagnostics = compiled.residual.diagnostics().entries();
    assert!(
        diagnostics
            .iter()
            .any(|diag| diag.code == "LINEARIZE_DEAD_HANDLER_ELIMINATED"),
        "handlers with no intersection against body effects should be eliminated"
    );
    assert!(
        !linear_stmt_graph_contains_perform_effect(&compiled.linear, main.body, 0),
        "eliminated handlers should not leave handled effect performs"
    );
    assert!(
        linear_stmt_graph_contains_perform_effect(&compiled.linear, main.body, 1),
        "dead-handler elimination should preserve unrelated body effects"
    );
}

#[test]
fn emits_match_branches_with_ctor_runtime_helpers() {
    let mut interner = Interner::new();
    let main_name = interner.intern("main");
    let option_name = interner.intern("Option");
    let some_name = interner.intern("Some");

    let mut program = LinearProgram::default();
    let seven = program.push_expr(LinearExpr::Literal(Literal::Int(7)));
    let scrutinee = program.push_expr(LinearExpr::MakeEnum {
        ty: option_name,
        variant: some_name,
        fields: vec![seven],
    });

    let binder = VarId::from_u32(0);
    let binder_expr = program.push_expr(LinearExpr::Var(binder));
    let arm_body = program.push_stmt(LinearStmt::Return(binder_expr));
    let default_expr = program.push_expr(LinearExpr::Literal(Literal::Int(0)));
    let default_body = program.push_stmt(LinearStmt::Return(default_expr));
    let body = program.push_stmt(LinearStmt::Match {
        scrutinee,
        arms: vec![LinearMatchArm {
            tag: some_name,
            binders: vec![binder],
            body: arm_body,
        }],
        default: Some(default_body),
    });

    program.functions.push(LinearFunction {
        id: LinearFuncId::new(0),
        name: main_name,
        params: vec![],
        body,
    });
    program.entrypoints = vec![LinearFuncId::new(0)];

    let emitted = emit_c_program(&program, &interner);
    assert!(emitted.contains("cielo_ctor_is_variant("));
    assert!(emitted.contains("cielo_ctor_field("));
    assert!(emitted.contains("cielo_make_ctor("));
}

#[test]
fn linearize_classifies_effectful_calls_by_convention() {
    let src = r#"
effect LocalState { fn tick() -> Int }
effect Console { fn print(s: String) -> () }

fn local() -> Int with LocalState {
  do LocalState.tick();
  1
}

fn io() -> Int with Console {
  do Console.print("x");
  2
}

fn main() -> Int {
  let a = local();
  let b = io();
  b
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    let main = compiled
        .linear
        .functions
        .iter()
        .find(|function| interner.resolve(function.name) == Some("main"))
        .expect("main function");

    let mut seen = HashSet::new();
    let mut conventions = Vec::new();
    collect_call_conventions(&compiled.linear, main.body, &mut seen, &mut conventions);

    assert!(conventions.contains(&CallConvention::Direct));
    assert!(conventions.contains(&CallConvention::Control));
}

#[test]
fn c_emitter_threads_call_convention_wrappers() {
    let src = r#"
effect LocalState { fn tick() -> Int }
effect Console { fn print(s: String) -> () }

fn pure(x: Int) -> Int {
  x + 1
}

fn local() -> Int with LocalState {
  do LocalState.tick();
  1
}

fn io() -> Int with Console {
  do Console.print("x");
  2
}

fn main() -> Int {
  let p = pure(1);
  let a = local();
  let b = io();
  b
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let compiled = compiler.compile_source_v0_to_c(src, SourceId::from_u32(0), &mut interner);

    assert!(compiled.c_source.contains("CIELO_CALL_PURE("));
    assert!(compiled.c_source.contains("CIELO_CALL_DIRECT("));
    assert!(compiled.c_source.contains("CIELO_CALL_CONTROL("));
}

#[test]
fn linearize_prunes_unreachable_recursive_function_cycles() {
    let src = r#"
fn live(x: Int) -> Int {
  x + 1
}

fn dead_a() -> Int {
  dead_b()
}

fn dead_b() -> Int {
  dead_a()
}

fn main() -> Int {
  let seed = @runtime { 7 };
  live(seed)
}
"#;
    let mut interner = Interner::new();
    let compiled =
        compile_source_v0_to_c_without_normalize(src, SourceId::from_u32(0), &mut interner);

    let mut names = linear_function_names(&compiled.linear, &interner);
    names.sort_unstable();
    assert_eq!(names, vec!["live".to_owned(), "main".to_owned()]);
    assert!(
        !compiled.c_source.contains("cielo_fn_dead_a_"),
        "unreachable dead_a must not be emitted"
    );
    assert!(
        !compiled.c_source.contains("cielo_fn_dead_b_"),
        "unreachable dead_b must not be emitted"
    );
}

#[test]
fn linearize_keeps_pure_call_dependencies_reachable() {
    let src = r#"
fn helper(x: Int) -> Int {
  x + 1
}

fn wrapper(seed: Int) -> Int {
  helper(seed)
}

fn dead() -> Int {
  0
}

fn main() -> Int {
  let seed = @runtime { 41 };
  wrapper(seed)
}
"#;
    let mut interner = Interner::new();
    let compiled =
        compile_source_v0_to_c_without_normalize(src, SourceId::from_u32(0), &mut interner);

    let mut names = linear_function_names(&compiled.linear, &interner);
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["helper".to_owned(), "main".to_owned(), "wrapper".to_owned()]
    );
    assert!(
        !compiled.c_source.contains("cielo_fn_dead_"),
        "unreachable pure function must not be emitted"
    );
}

#[test]
fn linearize_drops_unspecialized_copy_after_handler_specialization() {
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

    let loop_count = compiled
        .linear
        .functions
        .iter()
        .filter(|function| interner.resolve(function.name) == Some("loop"))
        .count();
    assert_eq!(
        compiled.linear.functions.len(),
        2,
        "only main + specialized loop should remain reachable"
    );
    assert_eq!(
        loop_count, 1,
        "unspecialized loop copy should be pruned after callsite retargeting"
    );
}

fn compile_source_v0_to_c_without_normalize(
    src: &str,
    source_id: SourceId,
    interner: &mut Interner,
) -> cielo::CompiledC {
    let compiler = Compiler::new(CompilerConfig::default());
    let core = compiler.parse_and_lower_to_core(src, source_id, interner);
    let residual = compiler.run_v0_core_pipeline(core);
    let specialized = handler_specialize::run(residual);
    let linearized = linearize::run(specialized);
    let emitted = c_emit::run(linearized, interner);
    cielo::CompiledC {
        residual: emitted.linearized.residual,
        linear: emitted.linearized.linear,
        c_source: emitted.c_source,
    }
}

fn collect_call_conventions(
    program: &LinearProgram,
    stmt_id: cielo::common::ids::LinearStmtId,
    seen: &mut HashSet<cielo::common::ids::LinearStmtId>,
    out: &mut Vec<CallConvention>,
) {
    if !seen.insert(stmt_id) {
        return;
    }
    let Some(stmt) = program.stmt(stmt_id) else {
        return;
    };
    match &stmt.kind {
        LinearStmt::PureCall { next, .. } => {
            out.push(CallConvention::Pure);
            collect_call_conventions(program, *next, seen, out);
        }
        LinearStmt::DirectCall { next, .. } => {
            out.push(CallConvention::Direct);
            collect_call_conventions(program, *next, seen, out);
        }
        LinearStmt::ControlCall { next, .. } => {
            out.push(CallConvention::Control);
            collect_call_conventions(program, *next, seen, out);
        }
        LinearStmt::Let { next, .. } | LinearStmt::Perform { next, .. } => {
            collect_call_conventions(program, *next, seen, out);
        }
        LinearStmt::Val { value, next, .. } => {
            collect_call_conventions(program, *value, seen, out);
            collect_call_conventions(program, *next, seen, out);
        }
        LinearStmt::If {
            then_branch,
            else_branch,
            ..
        } => {
            collect_call_conventions(program, *then_branch, seen, out);
            collect_call_conventions(program, *else_branch, seen, out);
        }
        LinearStmt::Match { arms, default, .. } => {
            for arm in arms {
                collect_call_conventions(program, arm.body, seen, out);
            }
            if let Some(default_stmt) = default {
                collect_call_conventions(program, *default_stmt, seen, out);
            }
        }
        LinearStmt::Handle { body, next, .. } | LinearStmt::Stage { body, next, .. } => {
            collect_call_conventions(program, *body, seen, out);
            if let Some(next_stmt) = next {
                collect_call_conventions(program, *next_stmt, seen, out);
            }
        }
        LinearStmt::Return(_) | LinearStmt::Hole | LinearStmt::Error => {}
    }
}

fn linear_stmt_graph_contains_perform_effect(
    program: &LinearProgram,
    root: cielo::common::ids::LinearStmtId,
    effect: u32,
) -> bool {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if matches!(stmt.kind, LinearStmt::Perform { effect: eff, .. } if eff.as_u32() == effect) {
            return true;
        }
        for child in stmt.child_stmts() {
            stack.push(child);
        }
    }
    false
}

fn linear_stmt_graph_contains_if(
    program: &LinearProgram,
    root: cielo::common::ids::LinearStmtId,
) -> bool {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if matches!(stmt.kind, LinearStmt::If { .. }) {
            return true;
        }
        for child in stmt.child_stmts() {
            stack.push(child);
        }
    }
    false
}

fn linear_stmt_graph_contains_match(
    program: &LinearProgram,
    root: cielo::common::ids::LinearStmtId,
) -> bool {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if matches!(stmt.kind, LinearStmt::Match { .. }) {
            return true;
        }
        for child in stmt.child_stmts() {
            stack.push(child);
        }
    }
    false
}

fn linear_stmt_graph_contains_literal_int(
    program: &LinearProgram,
    root: cielo::common::ids::LinearStmtId,
    value: i64,
) -> bool {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        for expr_id in stmt.child_exprs() {
            if linear_expr_is_int_literal(program, expr_id, value) {
                return true;
            }
        }
        for child in stmt.child_stmts() {
            stack.push(child);
        }
    }
    false
}

fn linear_stmt_graph_contains_add_rhs_int(
    program: &LinearProgram,
    root: cielo::common::ids::LinearStmtId,
    rhs: i64,
) -> bool {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };

        let exprs = match &stmt.kind {
            LinearStmt::Return(expr) => vec![*expr],
            LinearStmt::Let { value, .. } => vec![*value],
            _ => Vec::new(),
        };
        for expr_id in exprs {
            if let Some(expr) = program.expr(expr_id)
                && let LinearExpr::Binary {
                    op, rhs: rhs_expr, ..
                } = expr.kind
                && op == cielo::ir::core::BinaryOp::Add
                && linear_expr_is_int_literal(program, rhs_expr, rhs)
            {
                return true;
            }
        }

        for child in stmt.child_stmts() {
            stack.push(child);
        }
    }
    false
}

fn linear_stmt_graph_tail_resume_wrapper_count(
    program: &LinearProgram,
    root: cielo::common::ids::LinearStmtId,
) -> usize {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    let mut count = 0usize;
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if let LinearStmt::Val { binding, next, .. } = stmt.kind
            && let Some(next_stmt) = program.stmt(next)
            && let LinearStmt::Let {
                binding: return_param,
                value,
                next: return_next,
            } = next_stmt.kind
            && linear_expr_is_var(program, value, binding)
            && let Some(return_stmt) = program.stmt(return_next)
            && let LinearStmt::Return(return_expr) = return_stmt.kind
            && linear_expr_is_var(program, return_expr, return_param)
        {
            count += 1;
        }
        for child in stmt.child_stmts() {
            stack.push(child);
        }
    }
    count
}

fn linear_expr_is_var(
    program: &LinearProgram,
    expr_id: cielo::common::ids::LinearExprId,
    var: VarId,
) -> bool {
    program
        .expr(expr_id)
        .is_some_and(|expr| matches!(expr.kind, LinearExpr::Var(bound) if bound == var))
}

fn linear_expr_is_int_literal(
    program: &LinearProgram,
    expr_id: cielo::common::ids::LinearExprId,
    value: i64,
) -> bool {
    program.expr(expr_id).is_some_and(
        |expr| matches!(&expr.kind, LinearExpr::Literal(Literal::Int(lit)) if *lit == value),
    )
}

fn find_perform_call_line<'a>(c_source: &'a str, operation: &str) -> Option<&'a str> {
    let quoted_operation = format!("\"{operation}\"");
    c_source.lines().find(|line| {
        line.contains("(void)cielo_perform(") && line.contains(quoted_operation.as_str())
    })
}

fn parse_perform_symbol_id(line: &str, effect: u32, operation: &str, argc: usize) -> Option<u32> {
    let trimmed = line.trim();
    let prefix = format!("(void)cielo_perform({effect}, ");
    let suffix = format!(", \"{operation}\", {argc}, ");
    let payload = trimmed.strip_prefix(prefix.as_str())?;
    let (symbol_id, _) = payload.split_once(suffix.as_str())?;
    symbol_id.trim().parse::<u32>().ok()
}

fn handler_push_capability_temps_for_effect(c_source: &str, effect: u32) -> Vec<String> {
    c_source
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if !trimmed.starts_with("uint32_t ") {
                return None;
            }
            let declaration = trimmed.strip_prefix("uint32_t ")?;
            let (binding, _) = declaration.split_once(" = ")?;
            let push_with_effect = format!("= cielo_handler_push({effect});");
            let push_with_evidence = format!("= cielo_handler_push_with_evidence({effect}, &");
            if !trimmed.ends_with(push_with_effect.as_str())
                && !trimmed.contains(push_with_evidence.as_str())
            {
                return None;
            }
            Some(binding.to_owned())
        })
        .collect()
}

fn scoped_perform_capability_temps_for_effect(c_source: &str, effect: u32) -> Vec<String> {
    let prefix = format!("(void)cielo_perform_scoped({effect}, ");
    c_source
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let payload = trimmed.strip_prefix(prefix.as_str())?;
            let (capability, _) = payload.split_once(", ")?;
            Some(capability.to_owned())
        })
        .collect()
}

fn linear_function_names(program: &LinearProgram, interner: &Interner) -> Vec<String> {
    program
        .functions
        .iter()
        .map(|function| {
            interner
                .resolve(function.name)
                .unwrap_or("<missing>")
                .to_owned()
        })
        .collect()
}
