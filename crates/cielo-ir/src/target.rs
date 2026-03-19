//! Target facts shared by lowering and code generation.

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Endianness {
    Little,
    Big,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TargetSpec {
    pub word_size_bits: u8,
    pub endianness: Endianness,
    pub pointer_alignment: u8,
}

impl Default for TargetSpec {
    fn default() -> Self {
        Self {
            word_size_bits: 64,
            endianness: if cfg!(target_endian = "little") {
                Endianness::Little
            } else {
                Endianness::Big
            },
            pointer_alignment: 8,
        }
    }
}
