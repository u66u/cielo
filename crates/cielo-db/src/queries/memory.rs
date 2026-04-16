use std::sync::Arc;

use cielo_backend_c as backend_c;
use cielo_memory::{ArcConfig, MemoryInput, MemoryProfile, MemoryStrategy};

use crate::{CompileProfile, Db, EmittedFile, MemoryFile, SourceFile, TargetProfile, runtime_file};

#[salsa::tracked(no_eq, returns(clone))]
pub fn memory_file(
    db: &dyn Db,
    source: SourceFile,
    target: TargetProfile,
    memory: MemoryProfile,
) -> Arc<MemoryFile> {
    match memory.strategy {
        MemoryStrategy::Unmanaged => unmanaged_memory_file(db, source, target),
        MemoryStrategy::ReferenceCounting(config) => {
            refcount_memory_file(db, source, target, config)
        }
    }
}

#[salsa::tracked(no_eq, returns(clone))]
pub fn unmanaged_memory_file(
    db: &dyn Db,
    source: SourceFile,
    target: TargetProfile,
) -> Arc<MemoryFile> {
    let runtime = runtime_file(db, source, target);
    let memory = cielo_memory::unmanaged::lower(MemoryInput {
        runtime: &runtime.runtime,
    });
    Arc::new(MemoryFile { runtime, memory })
}

#[salsa::tracked(no_eq, returns(clone))]
pub fn refcount_memory_file(
    db: &dyn Db,
    source: SourceFile,
    target: TargetProfile,
    config: ArcConfig,
) -> Arc<MemoryFile> {
    let runtime = runtime_file(db, source, target);
    let memory = cielo_memory::refcount::lower(
        MemoryInput {
            runtime: &runtime.runtime,
        },
        config,
    );
    Arc::new(MemoryFile { runtime, memory })
}

#[salsa::tracked(no_eq, returns(clone))]
pub fn emitted_file(
    db: &dyn Db,
    source: SourceFile,
    target: TargetProfile,
    memory: MemoryProfile,
) -> Arc<EmittedFile> {
    let memory = memory_file(db, source, target, memory);
    let c_source = backend_c::emit(
        &memory.memory.cfg,
        &memory.runtime.interner,
        &memory.runtime.runtime.constants,
        memory.memory.emit_arc_trace_comments,
    );
    Arc::new(EmittedFile { memory, c_source })
}

pub fn compile(db: &dyn Db, source: SourceFile, profile: CompileProfile) -> Arc<EmittedFile> {
    emitted_file(db, source, profile.target, profile.memory)
}

pub fn compile_memory(db: &dyn Db, source: SourceFile, profile: CompileProfile) -> Arc<MemoryFile> {
    memory_file(db, source, profile.target, profile.memory)
}
