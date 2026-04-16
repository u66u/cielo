use std::sync::Arc;

use cielo_ir::target::TargetSpec;
use cielo_staging::{
    passes::{comptime, monomorphize},
    pipeline::phases::StagedCore,
};

use crate::{
    ClassifiedFile, Db, MonomorphizedFile, SourceFile, StagedFile, TargetProfile, typed_file,
};

#[salsa::tracked(no_eq, returns(clone))]
pub fn monomorphized_file(
    db: &dyn Db,
    source: SourceFile,
    target: TargetProfile,
) -> Arc<MonomorphizedFile> {
    let typed = typed_file(db, source, target);
    let mono = monomorphize::run(typed.typed.clone());
    Arc::new(MonomorphizedFile {
        source: typed.source,
        mono,
        interner: typed.interner.clone(),
    })
}

#[salsa::tracked(no_eq, returns(clone))]
pub fn classified_file(
    db: &dyn Db,
    source: SourceFile,
    target: TargetProfile,
) -> Arc<ClassifiedFile> {
    let mono = monomorphized_file(db, source, target);
    let classified = comptime::evaluate_classify(mono.mono.clone(), TargetSpec::from(target));
    Arc::new(ClassifiedFile {
        source: mono.source,
        classified,
        interner: mono.interner.clone(),
    })
}

#[salsa::tracked(no_eq, returns(clone))]
pub fn staged_file(db: &dyn Db, source: SourceFile, target: TargetProfile) -> Arc<StagedFile> {
    let classified = classified_file(db, source, target);
    let staged: StagedCore = comptime::residualize_specialize(classified.classified.clone());
    Arc::new(StagedFile {
        source: classified.source,
        staged,
        interner: classified.interner.clone(),
    })
}
