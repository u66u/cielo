use cielo::common::ids::SourceId;
use cielo::common::symbols::Interner;
use cielo::ir::core::Literal;
use cielo::{Compiler, CompilerConfig};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

fn fresh_path(prefix: &str, ext: &str) -> std::path::PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}_{stamp}.{ext}"))
}

#[test]
fn ct_query_cache_reuses_results_with_stable_key_and_deps() {
    let cache_path = fresh_path("cielo_ct_query_cache", "tsv");
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
    let run_1 = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner_1);
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
    let run_2 = compiler.compile_source_v0(src, SourceId::from_u32(1), &mut interner_2);
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
    let cache_path = fresh_path("cielo_ct_query_cache_dep", "tsv");
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
    let run_1 = compiler.compile_source_v0(src.as_str(), SourceId::from_u32(0), &mut interner_1);
    assert!(
        run_1.ct().eval_stats.eval_attempts > 0,
        "first run should evaluate ct expressions"
    );
    assert!(
        !run_1.ct().file_deps.is_empty(),
        "comptime file reads should produce tracked dependencies"
    );

    let mut interner_2 = Interner::new();
    let run_2 = compiler.compile_source_v0(src.as_str(), SourceId::from_u32(1), &mut interner_2);
    assert_eq!(
        run_2.ct().eval_stats.eval_attempts,
        0,
        "unchanged dependency content should hit persistent ct query cache"
    );

    fs::write(dep_path.as_path(), "beta").expect("rewrite dep");
    let mut interner_3 = Interner::new();
    let run_3 = compiler.compile_source_v0(src.as_str(), SourceId::from_u32(2), &mut interner_3);
    assert!(
        run_3.ct().eval_stats.eval_attempts > 0,
        "dependency hash changes must invalidate persistent ct query cache"
    );

    let _ = fs::remove_file(cache_path);
    let _ = fs::remove_file(dep_path);
}

#[test]
fn ct_query_cache_parity_matrix_hit_miss_and_invalidation() {
    let cache_path = fresh_path("cielo_ct_query_cache_matrix", "tsv");
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
    let cold_64 = compiler_64.compile_source_v0(src_a, SourceId::from_u32(0), &mut interner_1);
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
    let hit_64 = compiler_64.compile_source_v0(src_a, SourceId::from_u32(1), &mut interner_2);
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
    let miss_target = compiler_32.compile_source_v0(src_a, SourceId::from_u32(2), &mut interner_3);
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
    let hit_32 = compiler_32.compile_source_v0(src_a, SourceId::from_u32(3), &mut interner_4);
    assert_eq!(
        hit_32.ct().eval_stats.eval_attempts,
        0,
        "matrix[hit-after-target] should hit cache after recomputing under new key"
    );

    let mut interner_5 = Interner::new();
    let miss_fingerprint =
        compiler_32.compile_source_v0(src_b, SourceId::from_u32(4), &mut interner_5);
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
    let hit_fingerprint =
        compiler_32.compile_source_v0(src_b, SourceId::from_u32(5), &mut interner_6);
    assert_eq!(
        hit_fingerprint.ct().eval_stats.eval_attempts,
        0,
        "matrix[hit-after-fingerprint] should hit cache after source-specific recompute"
    );

    let _ = fs::remove_file(cache_path);
}
