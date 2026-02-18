use cielo::common::ids::SourceId;
use cielo::common::symbols::Interner;
use cielo::ir::core::Literal;
use cielo::{Compiler, CompilerConfig};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs};

fn fresh_path(prefix: &str, ext: &str) -> std::path::PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}_{stamp}.{ext}"))
}

#[test]
fn ct_query_cache_reuses_results_with_stable_key_and_deps() {
    let cache_path = fresh_path("cielo_ct_query_cache", "bin");
    let mut config = CompilerConfig::default();
    config.ct_query_cache_path = Some(cache_path.clone());
    let compiler = Compiler::new(config);

    let src = r#"
fn main() -> Int {
  let a = 1 + 2;
  let b = a * 3;
  b
}
"#;

    let mut interner_1 = Interner::new();
    let run_1 = compiler.compile_source(src, SourceId::from_u32(0), &mut interner_1);
    assert!(
        run_1.ct().eval_stats.eval_attempts > 0,
        "initial run should evaluate CT expressions"
    );
    assert!(
        run_1
            .ct()
            .ct_cache
            .values()
            .any(|lit| matches!(lit, Literal::Int(3))),
        "baseline cache should include folded addition result"
    );

    let mut interner_2 = Interner::new();
    let run_2 = compiler.compile_source(src, SourceId::from_u32(1), &mut interner_2);
    assert_eq!(
        run_2.ct().eval_stats.eval_attempts,
        0,
        "matching query key + deps should restore ct cache from persistent store"
    );
    assert_eq!(
        run_2.ct().ct_cache.len(),
        run_1.ct().ct_cache.len(),
        "restored cache should preserve folded entry count"
    );
    assert!(
        run_2
            .ct()
            .ct_cache
            .values()
            .any(|lit| matches!(lit, Literal::Int(3))),
        "restored cache should keep folded values"
    );

    let _ = fs::remove_file(cache_path);
}

#[test]
fn ct_query_cache_invalidates_when_comptime_file_dep_changes() {
    let cache_path = fresh_path("cielo_ct_query_cache_dep", "bin");
    let dep_path = fresh_path("cielo_ct_dep_input", "txt");
    fs::write(dep_path.as_path(), "alpha").expect("write dep");
    let dep_text = dep_path.to_string_lossy().replace('\\', "\\\\");

    let mut config = CompilerConfig::default();
    config.ct_query_cache_path = Some(cache_path.clone());
    let compiler = Compiler::new(config);
    let src = format!(
        r#"
effect ComptimeReadFiles {{ fn read(path: String) -> String }}
fn main() -> Int {{
  do ComptimeReadFiles.read("{dep_text}");
  let x = 1 + 2;
  x
}}
"#
    );

    let mut interner_1 = Interner::new();
    let run_1 = compiler.compile_source(src.as_str(), SourceId::from_u32(0), &mut interner_1);
    assert!(
        run_1.ct().eval_stats.eval_attempts > 0,
        "first run should evaluate ct expressions"
    );
    assert!(
        !run_1.ct().file_deps.is_empty(),
        "comptime file reads should produce tracked dependencies"
    );

    let mut interner_2 = Interner::new();
    let run_2 = compiler.compile_source(src.as_str(), SourceId::from_u32(1), &mut interner_2);
    assert_eq!(
        run_2.ct().eval_stats.eval_attempts,
        0,
        "unchanged dependency content should hit persistent ct query cache"
    );

    fs::write(dep_path.as_path(), "beta").expect("rewrite dep");
    let mut interner_3 = Interner::new();
    let run_3 = compiler.compile_source(src.as_str(), SourceId::from_u32(2), &mut interner_3);
    assert!(
        run_3.ct().eval_stats.eval_attempts > 0,
        "dependency hash changes must invalidate persistent ct query cache"
    );

    let _ = fs::remove_file(cache_path);
    let _ = fs::remove_file(dep_path);
}

#[test]
fn ct_query_cache_parity_matrix_hit_miss_and_invalidation() {
    let cache_path = fresh_path("cielo_ct_query_cache_matrix", "bin");
    let src_a = r#"
fn main() -> Int {
  let x = 2147483648;
  x
}
"#;
    let src_b = r#"
fn main() -> Int {
  let x = 2147483649;
  x
}
"#;

    let mut cfg_64 = CompilerConfig::default();
    cfg_64.ct_query_cache_path = Some(cache_path.clone());
    let compiler_64 = Compiler::new(cfg_64);

    let mut interner_1 = Interner::new();
    let cold_64 = compiler_64.compile_source(src_a, SourceId::from_u32(0), &mut interner_1);
    assert!(
        cold_64.ct().eval_stats.eval_attempts > 0,
        "matrix[miss-cold] should compute ct cache on first run"
    );
    assert!(
        cold_64
            .ct()
            .ct_cache
            .values()
            .any(|lit| matches!(lit, Literal::Int(2_147_483_648))),
        "64-bit target should preserve wide folded literal value"
    );

    let mut interner_2 = Interner::new();
    let hit_64 = compiler_64.compile_source(src_a, SourceId::from_u32(1), &mut interner_2);
    assert_eq!(
        hit_64.ct().eval_stats.eval_attempts,
        0,
        "matrix[hit] should reuse persistent cache when key+deps+fingerprint match"
    );

    let mut cfg_32 = CompilerConfig::default();
    cfg_32.target.word_size_bits = 32;
    cfg_32.ct_query_cache_path = Some(cache_path.clone());
    let compiler_32 = Compiler::new(cfg_32);

    let mut interner_3 = Interner::new();
    let miss_target = compiler_32.compile_source(src_a, SourceId::from_u32(2), &mut interner_3);
    assert!(
        miss_target.ct().eval_stats.eval_attempts > 0,
        "matrix[invalidate-target] should invalidate cache when target key changes"
    );
    assert!(
        miss_target
            .ct()
            .ct_cache
            .values()
            .any(|lit| matches!(lit, Literal::Int(-2_147_483_648))),
        "32-bit key invalidation should recompute and normalize folded literal"
    );

    let mut interner_4 = Interner::new();
    let hit_32 = compiler_32.compile_source(src_a, SourceId::from_u32(3), &mut interner_4);
    assert_eq!(
        hit_32.ct().eval_stats.eval_attempts,
        0,
        "matrix[hit-after-target] should hit cache after recomputing under new key"
    );

    let mut interner_5 = Interner::new();
    let miss_fingerprint =
        compiler_32.compile_source(src_b, SourceId::from_u32(4), &mut interner_5);
    assert!(
        miss_fingerprint.ct().eval_stats.eval_attempts > 0,
        "matrix[invalidate-fingerprint] should invalidate cache when program fingerprint changes"
    );
    assert!(
        miss_fingerprint
            .ct()
            .ct_cache
            .values()
            .any(|lit| matches!(lit, Literal::Int(-2_147_483_647))),
        "program fingerprint invalidation should recompute folded values for new source"
    );

    let mut interner_6 = Interner::new();
    let hit_fingerprint = compiler_32.compile_source(src_b, SourceId::from_u32(5), &mut interner_6);
    assert_eq!(
        hit_fingerprint.ct().eval_stats.eval_attempts,
        0,
        "matrix[hit-after-fingerprint] should hit cache after source-specific recompute"
    );

    let _ = fs::remove_file(cache_path);
}

#[test]
fn fused_stage_a_query_cache_parity_matrix_hit_miss_and_invalidation() {
    let cache_path = fresh_path("cielo_ct_query_cache_stage_a_matrix", "bin");
    let src_a = r#"
fn main() -> Int {
  let x = 2147483648;
  x
}
"#;
    let src_b = r#"
fn main() -> Int {
  let x = 2147483649;
  x
}
"#;

    let mut cfg_64 = CompilerConfig::default();
    cfg_64.ct_query_cache_path = Some(cache_path.clone());
    let compiler_64 = Compiler::new(cfg_64);

    let mut interner_1 = Interner::new();
    let cold_core =
        compiler_64.parse_and_lower_to_core(src_a, SourceId::from_u32(10), &mut interner_1);
    let cold_64 = compiler_64.run_v1_evaluate_classify(cold_core);
    assert!(
        cold_64.ct().eval_stats.eval_attempts > 0,
        "fused matrix[miss-cold] should compute ct cache on first Stage-A run"
    );
    assert!(
        cold_64
            .ct()
            .ct_cache
            .values()
            .any(|lit| matches!(lit, Literal::Int(2_147_483_648))),
        "fused Stage-A 64-bit target should preserve wide folded literal value"
    );

    let mut interner_2 = Interner::new();
    let hit_core =
        compiler_64.parse_and_lower_to_core(src_a, SourceId::from_u32(11), &mut interner_2);
    let hit_64 = compiler_64.run_v1_evaluate_classify(hit_core);
    assert_eq!(
        hit_64.ct().eval_stats.eval_attempts,
        0,
        "fused matrix[hit] should reuse persistent cache when key+deps+fingerprint match"
    );
    assert_eq!(
        cold_64.bta().stage_of_expr.len(),
        hit_64.bta().stage_of_expr.len(),
        "cache hits must preserve fused Stage-A table coverage"
    );
    for (expr_id, cold_stage) in cold_64.bta().stage_of_expr.iter() {
        assert_eq!(
            hit_64.bta().stage_of_expr.get(&expr_id),
            Some(cold_stage),
            "cache hits must preserve fused Stage-A classifications at e{}",
            expr_id.as_u32()
        );
    }

    let mut cfg_32 = CompilerConfig::default();
    cfg_32.target.word_size_bits = 32;
    cfg_32.ct_query_cache_path = Some(cache_path.clone());
    let compiler_32 = Compiler::new(cfg_32);

    let mut interner_3 = Interner::new();
    let miss_target_core =
        compiler_32.parse_and_lower_to_core(src_a, SourceId::from_u32(12), &mut interner_3);
    let miss_target = compiler_32.run_v1_evaluate_classify(miss_target_core);
    assert!(
        miss_target.ct().eval_stats.eval_attempts > 0,
        "fused matrix[invalidate-target] should invalidate cache when target key changes"
    );
    assert!(
        miss_target
            .ct()
            .ct_cache
            .values()
            .any(|lit| matches!(lit, Literal::Int(-2_147_483_648))),
        "fused Stage-A target invalidation should recompute and normalize folded literal"
    );

    let mut interner_4 = Interner::new();
    let miss_fp_core =
        compiler_32.parse_and_lower_to_core(src_b, SourceId::from_u32(13), &mut interner_4);
    let miss_fingerprint = compiler_32.run_v1_evaluate_classify(miss_fp_core);
    assert!(
        miss_fingerprint.ct().eval_stats.eval_attempts > 0,
        "fused matrix[invalidate-fingerprint] should invalidate cache when source changes"
    );
    assert!(
        miss_fingerprint
            .ct()
            .ct_cache
            .values()
            .any(|lit| matches!(lit, Literal::Int(-2_147_483_647))),
        "fused Stage-A fingerprint invalidation should recompute folded values for new source"
    );

    let mut interner_5 = Interner::new();
    let hit_fp_core =
        compiler_32.parse_and_lower_to_core(src_b, SourceId::from_u32(14), &mut interner_5);
    let hit_fingerprint = compiler_32.run_v1_evaluate_classify(hit_fp_core);
    assert_eq!(
        hit_fingerprint.ct().eval_stats.eval_attempts,
        0,
        "fused matrix[hit-after-fingerprint] should hit cache after source-specific recompute"
    );

    let _ = fs::remove_file(cache_path.clone());
    let _ = fs::remove_file(cache_path.with_extension("ctdeps.bin"));
}

#[test]
fn test_query_cache_hit_and_invalidation_matrix() {
    let temp_dir = env::temp_dir();
    let cache_path = temp_dir.join("cielo_test_cache.bin");
    let _ = fs::remove_file(&cache_path); // Ensure clean state
    let _ = fs::remove_file(cache_path.with_extension("ctdeps.bin"));

    let source_v1 = r#"
    fn main() -> Int {
        let x = 10 + 20;
        let y = x * 2;
        y
    }
    "#;

    let source_v2 = r#"
    fn main() -> Int {
        let x = 10 + 21; // Changed literal -> different fingerprint
        let y = x * 2;
        y
    }
    "#;

    // COLD RUN
    let mut config = CompilerConfig::default();
    config.ct_query_cache_path = Some(cache_path.clone());
    let compiler_cold = Compiler::new(config.clone());

    let mut interner1 = Interner::new();
    let res_cold = compiler_cold.compile_source(source_v1, SourceId::new(1), &mut interner1);

    let cold_attempts = res_cold.ct().eval_stats.eval_attempts;
    assert!(
        cold_attempts > 0,
        "Cold run should evaluate expressions instead of restoring a warm query cache snapshot"
    );

    // WARM RUN (PERFECT HIT)
    let compiler_warm = Compiler::new(config.clone());
    let mut interner2 = Interner::new();
    let res_warm = compiler_warm.compile_source(source_v1, SourceId::new(2), &mut interner2);

    let warm_attempts = res_warm.ct().eval_stats.eval_attempts;
    assert_eq!(
        warm_attempts, 0,
        "Warm run should restore ct results directly from persistent query cache"
    );

    // both should have evaluated `10 + 20 * 2` down to the exact same Known literal count
    assert_eq!(
        res_cold.residual().residualize_stats.embedded_literals,
        res_warm.residual().residualize_stats.embedded_literals,
        "Semantic drift: Warm run embedded a different number of literals than Cold run"
    );

    // INVALIDATION: AST FINGERPRINT CHANGED
    let compiler_miss = Compiler::new(config.clone());
    let mut interner3 = Interner::new();
    let res_miss = compiler_miss.compile_source(source_v2, SourceId::new(3), &mut interner3);

    assert!(
        res_miss.ct().eval_stats.eval_attempts > 0,
        "Cache must invalidate if AST fingerprint changes"
    );

    // re-warm the cache with source_v1
    let _ = compiler_warm.compile_source(source_v1, SourceId::new(4), &mut interner2);

    // change target word size from default (64) to 32
    let mut config_target_miss = config.clone();
    config_target_miss.target.word_size_bits = 32;
    let compiler_target_miss = Compiler::new(config_target_miss);

    let mut interner4 = Interner::new();
    let res_target_miss =
        compiler_target_miss.compile_source(source_v1, SourceId::new(5), &mut interner4);

    assert!(
        res_target_miss.ct().eval_stats.eval_attempts > 0,
        "Cache must invalidate if TargetSpec word_size_bits changes"
    );
}
