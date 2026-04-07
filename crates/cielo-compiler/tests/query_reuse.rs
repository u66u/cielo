use std::sync::Arc;

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
