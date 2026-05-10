use std::sync::Arc;

use cielo_base::{Interner, SourceId};
use cielo_frontend::parser::parse_source;
use cielo_ir::builtins::BuiltinSymbols;
use cielo_ir::target::TargetSpec;
use cielo_lowering::{LowerConfig, LowerOutput, TargetBuiltinSymbols, lower_program};
use cielo_sema::check_core;

use crate::{CoreFile, Db, ParsedFile, SourceFile, TargetProfile, TypedFile};

#[salsa::tracked(no_eq, returns(clone))]
pub fn parsed_file(db: &dyn Db, source: SourceFile) -> Arc<ParsedFile> {
    let source_id = SourceId::from_u32(source.source_id(db));
    let mut interner = Interner::new();
    let parsed = parse_source(source.text(db), source_id, &mut interner);
    Arc::new(ParsedFile {
        source: source_id,
        path: source.path(db),
        ast: parsed.program,
        diagnostics: parsed.diagnostics,
        interner: Arc::new(interner),
    })
}

#[salsa::tracked(no_eq, returns(clone))]
pub fn core_file(db: &dyn Db, source: SourceFile, target: TargetProfile) -> Arc<CoreFile> {
    let parsed = parsed_file(db, source);
    let mut interner = parsed.interner.as_ref().clone();
    let main = interner.intern("main");
    let builtins = TargetBuiltinSymbols::intern(&mut interner);
    let runtime_builtins = BuiltinSymbols::intern(&mut interner);
    let interner = Arc::new(interner);
    let lowered = lower_program(
        &parsed.ast,
        LowerConfig::with_entrypoint(main)
            .with_target_builtins(TargetSpec::from(target), builtins)
            .with_builtins(runtime_builtins)
            .with_names(interner.clone()),
    );
    let mut diagnostics = parsed.diagnostics.clone();
    diagnostics.extend(lowered.diagnostics);
    Arc::new(CoreFile {
        source: parsed.source,
        core: LowerOutput::new(lowered.program, diagnostics),
        interner,
    })
}

#[salsa::tracked(no_eq, returns(clone))]
pub fn typed_file(db: &dyn Db, source: SourceFile, target: TargetProfile) -> Arc<TypedFile> {
    let core = core_file(db, source, target);
    let (program, diagnostics) = core.core.clone().into_parts();
    Arc::new(TypedFile {
        source: core.source,
        typed: check_core(program, diagnostics),
        interner: core.interner.clone(),
    })
}
