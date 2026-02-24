#![allow(dead_code)]

use cielo::common::diagnostics::DiagnosticBag;
use cielo::common::ids::SourceId;
use cielo::common::symbols::Interner;
use cielo::ir::core::CoreProgram;
use cielo::pipeline::phases::{Residualized, SemanticTables};
use cielo::sema::typecheck::typecheck_core;
use cielo::{Compiler, CompilerConfig};

pub fn lower_to_core(source: &str) -> CoreProgram {
    let compiler = Compiler::new(CompilerConfig::default());
    let mut interner = Interner::new();
    let core = compiler.parse_and_lower_to_core(source, SourceId::from_u32(0), &mut interner);
    core.program().clone()
}

pub fn lower_and_typecheck(source: &str) -> (CoreProgram, SemanticTables) {
    let compiler = Compiler::new(CompilerConfig::default());
    let mut interner = Interner::new();
    let core = compiler.parse_and_lower_to_core(source, SourceId::from_u32(0), &mut interner);
    let program = core.program().clone();
    let mut diagnostics = DiagnosticBag::default();
    let sema = typecheck_core(&program, &mut diagnostics);
    assert!(
        !diagnostics.has_errors(),
        "fixture must typecheck cleanly, got diagnostics: {:?}",
        diagnostics.entries()
    );
    (program, sema)
}

pub fn compile_source_v1(source: &str) -> Residualized {
    let compiler = Compiler::new(CompilerConfig::default());
    let mut interner = Interner::new();
    compiler.compile_source(source, SourceId::from_u32(0), &mut interner)
}
