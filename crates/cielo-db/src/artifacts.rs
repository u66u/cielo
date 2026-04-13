use std::sync::Arc;

use cielo_base::{DiagnosticBag, Interner, SourceId};
use cielo_frontend::ast::Program;
use cielo_ir::linear::LinearProgram;
use cielo_ir::runtime::RuntimeProgram;
use cielo_lowering::LowerOutput;
use cielo_memory::MemoryProgram;
use cielo_sema::TypedCore;
use cielo_staging::pipeline::phases::{BtaClassified, Monomorphized, Residualized};

macro_rules! file_artifact {
    ($(#[$meta:meta])* $name:ident { $($field:ident: $ty:ty),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Debug)]
        pub struct $name {
            pub source: SourceId,
            $(pub $field: $ty,)+
            pub interner: Interner,
        }
    };
}

file_artifact!(
    /// Parsed syntax and diagnostics for one source file.
    ParsedFile {
        path: String,
        ast: Program,
        diagnostics: DiagnosticBag,
    }
);
file_artifact!(
    /// Core lowering output for one source file.
    CoreFile { core: LowerOutput }
);
file_artifact!(
    /// Typechecked Core and facts for one source file.
    TypedFile { typed: TypedCore }
);
file_artifact!(
    /// Core after concrete function instances have been created.
    MonomorphizedFile { mono: Monomorphized }
);
file_artifact!(
    /// Monomorphized Core with comptime/runtime classifications.
    ClassifiedFile { classified: BtaClassified }
);
file_artifact!(
    /// Runtime-only Core after residualization and specialization.
    StagedFile { residual: Residualized }
);
file_artifact!(
    /// Normalized runtime Core and its Linear IR.
    LinearFile {
        residual: Residualized,
        linear: LinearProgram,
    }
);
file_artifact!(
    /// Self-contained input to memory lowering and backend emission.
    RuntimeFile { runtime: RuntimeProgram }
);

#[derive(Clone, Debug)]
pub struct MemoryFile {
    pub runtime: Arc<RuntimeFile>,
    pub memory: MemoryProgram,
}

#[derive(Clone, Debug)]
pub struct EmittedFile {
    pub memory: Arc<MemoryFile>,
    pub c_source: String,
}
