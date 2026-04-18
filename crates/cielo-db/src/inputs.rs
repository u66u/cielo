use cielo_ir::target::{Endianness, TargetSpec};
use cielo_memory::MemoryProfile;
use cielo_staging::pipeline::phases::CtFileDep;

#[salsa::input]
#[derive(Debug)]
pub struct SourceFile {
    #[returns(copy)]
    pub source_id: u32,
    #[returns(clone)]
    pub path: String,
    #[returns(deref)]
    pub text: String,
}

#[salsa::input]
#[derive(Debug)]
pub struct ComptimeInputs {
    #[returns(clone)]
    pub files: Vec<CtFileDep>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TargetProfile {
    pub word_size_bits: u8,
    pub endianness: Endianness,
    pub pointer_alignment: u8,
}

impl Default for TargetProfile {
    fn default() -> Self {
        Self::from(TargetSpec::default())
    }
}

impl From<TargetSpec> for TargetProfile {
    fn from(target: TargetSpec) -> Self {
        Self {
            word_size_bits: target.word_size_bits,
            endianness: target.endianness,
            pointer_alignment: target.pointer_alignment,
        }
    }
}

impl From<TargetProfile> for TargetSpec {
    fn from(target: TargetProfile) -> Self {
        Self {
            word_size_bits: target.word_size_bits,
            endianness: target.endianness,
            pointer_alignment: target.pointer_alignment,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CompileProfile {
    pub target: TargetProfile,
    pub memory: MemoryProfile,
}

impl Default for CompileProfile {
    fn default() -> Self {
        Self {
            target: TargetProfile::default(),
            memory: MemoryProfile::default(),
        }
    }
}
