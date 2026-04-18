use std::sync::Arc;

use cielo_base::SourceId;
use cielo_db::{
    CieloDatabase, CompileProfile, ComptimeInputs, CoreFile, EmittedFile, MemoryFile, ParsedFile,
    RuntimeFile, SourceFile, StagedFile, TargetProfile, TypedFile,
};
use cielo_memory::MemoryProfile;

pub use cielo_ir::target::{Endianness, TargetSpec};

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct CompilerConfig {
    pub target: TargetSpec,
    pub memory: MemoryProfile,
}

#[derive(Default)]
pub struct Compiler {
    config: CompilerConfig,
    db: CieloDatabase,
}

impl std::fmt::Debug for Compiler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Compiler")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Compiler {
    pub fn new(config: CompilerConfig) -> Self {
        Self {
            config,
            db: CieloDatabase::default(),
        }
    }

    pub fn config(&self) -> CompilerConfig {
        self.config.clone()
    }

    pub fn database(&self) -> &CieloDatabase {
        &self.db
    }

    pub fn source(&self, text: &str, source_id: SourceId) -> SourceFile {
        self.source_at("<memory>", text, source_id)
    }

    pub fn source_at(&self, path: &str, text: &str, source_id: SourceId) -> SourceFile {
        SourceFile::new(
            &self.db,
            source_id.as_u32(),
            path.to_owned(),
            text.to_owned(),
        )
    }

    pub fn parsed(&self, source: SourceFile) -> Arc<ParsedFile> {
        cielo_db::parsed_file(&self.db, source)
    }

    pub fn core(&self, source: SourceFile) -> Arc<CoreFile> {
        cielo_db::core_file(&self.db, source, self.target_profile())
    }

    pub fn typed(&self, source: SourceFile) -> Arc<TypedFile> {
        cielo_db::typed_file(&self.db, source, self.target_profile())
    }

    pub fn monomorphized(&self, source: SourceFile) -> Arc<cielo_db::MonomorphizedFile> {
        cielo_db::monomorphized_file(&self.db, source, self.target_profile())
    }

    pub fn classified(&self, source: SourceFile) -> Arc<cielo_db::ClassifiedFile> {
        cielo_db::classified_file(
            &self.db,
            source,
            self.target_profile(),
            self.comptime_inputs(source),
        )
    }

    pub fn staged(&self, source: SourceFile) -> Arc<StagedFile> {
        cielo_db::staged_file(
            &self.db,
            source,
            self.target_profile(),
            self.comptime_inputs(source),
        )
    }

    pub fn linear(&self, source: SourceFile) -> Arc<cielo_db::LinearFile> {
        cielo_db::linear_file(
            &self.db,
            source,
            self.target_profile(),
            self.comptime_inputs(source),
        )
    }

    pub fn runtime(&self, source: SourceFile) -> Arc<RuntimeFile> {
        cielo_db::runtime_file(
            &self.db,
            source,
            self.target_profile(),
            self.comptime_inputs(source),
        )
    }

    pub fn memory(&self, source: SourceFile) -> Arc<MemoryFile> {
        self.memory_with_profile(source, self.config.memory)
    }

    pub fn memory_with_profile(
        &self,
        source: SourceFile,
        memory: MemoryProfile,
    ) -> Arc<MemoryFile> {
        cielo_db::compile_memory(
            &self.db,
            source,
            self.compile_profile(memory),
            self.comptime_inputs(source),
        )
    }

    pub fn emit(&self, source: SourceFile) -> Arc<EmittedFile> {
        self.emit_with_profile(source, self.config.memory)
    }

    pub fn emit_with_profile(&self, source: SourceFile, memory: MemoryProfile) -> Arc<EmittedFile> {
        cielo_db::compile(
            &self.db,
            source,
            self.compile_profile(memory),
            self.comptime_inputs(source),
        )
    }

    pub fn compile(&self, text: &str, source_id: SourceId) -> Arc<EmittedFile> {
        self.emit(self.source(text, source_id))
    }

    pub fn compile_with_profile(
        &self,
        text: &str,
        source_id: SourceId,
        memory: MemoryProfile,
    ) -> Arc<EmittedFile> {
        self.emit_with_profile(self.source(text, source_id), memory)
    }

    fn target_profile(&self) -> TargetProfile {
        TargetProfile::from(self.config.target)
    }

    fn comptime_inputs(&self, source: SourceFile) -> ComptimeInputs {
        let paths = cielo_db::comptime_paths(&self.db, source, self.target_profile());
        self.db.load_comptime_inputs(paths.as_slice())
    }

    fn compile_profile(&self, memory: MemoryProfile) -> CompileProfile {
        CompileProfile {
            target: self.target_profile(),
            memory,
        }
    }
}
