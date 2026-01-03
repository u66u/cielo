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
        run_1.ct().ct_cache.values().any(|lit| matches!(lit, Literal::Int(3))),
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
        run_2.ct().ct_cache.values().any(|lit| matches!(lit, Literal::Int(3))),
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
