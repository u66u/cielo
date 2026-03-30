use cielo::common::ids::SourceId;
use cielo::common::symbols::Interner;
use cielo::{Compiler, CompilerConfig};
use cielo_memory::{MemoryModel, MemoryModule};

#[test]
fn compiler_parse_uses_the_database_boundary() {
    let compiler = Compiler::new(CompilerConfig::default());
    let source = compiler.database_source("fn main() {}", SourceId::from_u32(0));
    let mut interner = Interner::new();
    let parsed = compiler.database_parse_file(source, &mut interner);
    assert_eq!(parsed.ast().items.len(), 1);
    assert!(!parsed.diagnostics().has_errors());
}

#[test]
fn compiler_typechecks_through_the_database_boundary() {
    let compiler = Compiler::new(CompilerConfig::default());
    let source = compiler.database_source("fn main() {}", SourceId::from_u32(0));
    let mut interner = Interner::new();
    let typed = compiler.database_type_file(source, &mut interner);
    assert_eq!(typed.program().functions().len(), 1);
    assert!(!typed.diagnostics().has_errors());
    assert_eq!(
        typed.sema().type_of_expr.len(),
        typed.program().exprs().len()
    );
}

#[test]
fn compiler_profile_selects_a_strategy_specific_product() {
    let compiler = Compiler::new(CompilerConfig::default().with_memory_model(MemoryModel::Tracing));
    let source = compiler.database_source("fn main() {}", SourceId::from_u32(0));
    let memory = compiler.database_memory_file(source);
    assert!(matches!(memory.as_ref(), MemoryModule::Tracing(_)));
}
