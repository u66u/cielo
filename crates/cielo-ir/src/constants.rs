use crate::core::Literal;
use cielo_base::ids::SymbolId;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum ScalarLiteralKey {
    Bool(bool),
    Int(i64),
    Char(char),
    Float(u64),
}

impl ScalarLiteralKey {
    pub fn from_literal(literal: &Literal) -> Option<Self> {
        match literal {
            Literal::Bool(value) => Some(Self::Bool(*value)),
            Literal::Int(value) => Some(Self::Int(*value)),
            Literal::Char(value) => Some(Self::Char(*value)),
            Literal::Float(value) if value.is_finite() => Some(Self::Float(value.to_bits())),
            Literal::Unit | Literal::Float(_) | Literal::String(_) => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum CtorFieldKey {
    Unit,
    Bool(bool),
    Int(i64),
    Char(char),
    Float(u64),
    String(String),
    Ctor(Box<CtorLiteralKey>),
}

impl CtorFieldKey {
    pub fn from_literal(literal: &Literal) -> Option<Self> {
        match literal {
            Literal::Unit => Some(Self::Unit),
            Literal::Bool(value) => Some(Self::Bool(*value)),
            Literal::Int(value) => Some(Self::Int(*value)),
            Literal::Char(value) => Some(Self::Char(*value)),
            Literal::Float(value) if value.is_finite() => Some(Self::Float(value.to_bits())),
            Literal::String(value) => Some(Self::String(value.clone())),
            Literal::Float(_) => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CtorLiteralKey {
    pub ty: SymbolId,
    pub variant: SymbolId,
    pub fields: Vec<CtorFieldKey>,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum ConstantKey {
    Scalar(ScalarLiteralKey),
    String(String),
    Ctor(CtorLiteralKey),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConstantEmbedStrategy {
    StaticConst,
    Pooled,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ConstantEntry {
    pub key: ConstantKey,
    pub strategy: ConstantEmbedStrategy,
    pub estimated_size_bytes: usize,
}

#[derive(Clone, Debug, Default)]
pub struct ConstantTable {
    pub entries: Vec<ConstantEntry>,
    pub entry_cap_bytes: usize,
    pub unit_cap_bytes: usize,
    pub total_size_bytes: usize,
}
