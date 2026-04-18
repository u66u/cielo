use std::fs;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use cielo::{Compiler, CompilerConfig};
use cielo_base::SourceId;

#[test]
fn compiler_reuses_a_staged_query_result() {
    let compiler = Compiler::new(CompilerConfig::default());
    let source = compiler.source(
        "fn main() -> Int { let x = 1 + 2; x }",
        SourceId::from_u32(0),
    );
    let _ = compiler.database().take_query_events();

    let first = compiler.staged(source);
    let first_events = compiler.database().take_query_events();
    let second = compiler.staged(source);
    let second_events = compiler.database().take_query_events();

    assert!(Arc::ptr_eq(&first, &second));
    assert!(!first_events.is_empty());
    assert!(second_events.is_empty());
}

#[test]
fn changed_comptime_file_only_reruns_staging() {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("cielo_db_ct_dep_{stamp}.txt"));
    fs::write(&path, "alpha").expect("write comptime input");
    let path_text = path.to_string_lossy().replace('\\', "\\\\");
    let text = format!(
        r#"
effect ComptimeReadFiles {{ fn read(path: String) -> String }}
fn main() -> Int {{
  do ComptimeReadFiles.read("{path_text}");
  0
}}
"#
    );

    let compiler = Compiler::new(CompilerConfig::default());
    let source = compiler.source(&text, SourceId::from_u32(0));
    let first = compiler.staged(source);
    let first_hash = first.staged.ct().file_deps[0].content_hash.clone();
    let _ = compiler.database().take_query_events();

    fs::write(&path, "beta").expect("rewrite comptime input");
    let second = compiler.staged(source);
    let second_hash = second.staged.ct().file_deps[0].content_hash.clone();
    let events = compiler.database().take_query_events();
    fs::remove_file(path).expect("remove comptime input");

    assert_ne!(first_hash, second_hash);
    assert!(!Arc::ptr_eq(&first, &second));
    assert!(
        events
            .iter()
            .any(|event| event.description.contains("classified_file"))
    );
    assert!(
        events
            .iter()
            .any(|event| event.description.contains("staged_file"))
    );
    assert!(
        !events
            .iter()
            .any(|event| event.description.contains("parsed_file"))
    );
    assert!(
        !events
            .iter()
            .any(|event| event.description.contains("monomorphized_file"))
    );
}
