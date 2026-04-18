use cielo::{Compiler, CompilerConfig, MemoryPreset, MemoryProfile, MemoryReport, MemoryStrategy};
use cielo_base::SourceId;

#[test]
fn one_compiler_reuses_runtime_for_multiple_memory_profiles() {
    let compiler = Compiler::new(CompilerConfig::default());
    let source = compiler.source(
        "enum Boxed { Wrap(Int) } fn main() -> Int { let x = Wrap(1); 0 }",
        SourceId::from_u32(0),
    );

    let arc = compiler.memory(source);
    assert!(matches!(
        compiler.config().memory.strategy,
        MemoryStrategy::ReferenceCounting(_)
    ));
    assert!(!arc.runtime.runtime.cfg.blocks().is_empty());
    let _ = compiler.database().take_query_events();

    let unmanaged =
        compiler.memory_with_profile(source, MemoryProfile::from_preset(MemoryPreset::Unmanaged));
    let events = compiler.database().take_query_events();

    assert!(matches!(unmanaged.memory.report(), MemoryReport::Unmanaged));
    assert!(
        events
            .iter()
            .any(|event| event.description.contains("unmanaged_memory_file"))
    );
    assert!(
        !events
            .iter()
            .any(|event| event.description.contains("runtime_file"))
    );

    let emitted =
        compiler.emit_with_profile(source, MemoryProfile::from_preset(MemoryPreset::Unmanaged));
    assert!(emitted.c_source.contains("int main(void)"));
}
