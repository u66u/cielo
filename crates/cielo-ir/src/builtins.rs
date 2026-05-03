//! Operations the runtime implements directly.
//!
//! A builtin is not declared in source and is not an effect: a call reaches
//! the runtime without passing through the handler stack, so output no longer
//! depends on an effect escaping every handler.

use crate::core::{CoreTypeRef, PrimitiveTypeRef};
use cielo_base::ids::SymbolId;
use cielo_base::symbols::Interner;
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Builtin {
    Print,
    StrLen,
    StrConcat,
}

impl Builtin {
    pub const ALL: [Self; 3] = [Self::Print, Self::StrLen, Self::StrConcat];

    pub fn name(self) -> &'static str {
        match self {
            Self::Print => "print",
            Self::StrLen => "str_len",
            Self::StrConcat => "str_concat",
        }
    }

    /// The C runtime function implementing this builtin. Codegen calls it
    /// directly; the symbol-keyed table in the header is only for operations
    /// that arrive through `perform`.
    pub fn c_symbol(self) -> &'static str {
        match self {
            Self::Print => "cielo_builtin_print",
            Self::StrLen => "cielo_builtin_str_len",
            Self::StrConcat => "cielo_builtin_str_concat",
        }
    }

    pub fn arity(self) -> usize {
        match self {
            Self::Print | Self::StrLen => 1,
            Self::StrConcat => 2,
        }
    }

    /// Whether evaluating the builtin produces output. Only these must survive
    /// dead-code elimination when their result is unused.
    pub fn is_observable(self) -> bool {
        matches!(self, Self::Print)
    }

    /// `None` for a parameter the builtin accepts at any type, which today is
    /// only `print`'s: it formats every tag.
    pub fn param_types(self) -> &'static [Option<PrimitiveTypeRef>] {
        match self {
            Self::Print => &[None],
            Self::StrLen => &[Some(PrimitiveTypeRef::String)],
            Self::StrConcat => &[
                Some(PrimitiveTypeRef::String),
                Some(PrimitiveTypeRef::String),
            ],
        }
    }

    pub fn return_type(self) -> CoreTypeRef {
        match self {
            Self::Print => CoreTypeRef::Unit,
            Self::StrLen => CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
            Self::StrConcat => CoreTypeRef::Primitive(PrimitiveTypeRef::String),
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|builtin| builtin.name() == name)
    }
}

/// Builtin names resolved to symbols. Lowering has no interner of its own, so
/// the mapping is interned up front and threaded through `LowerConfig`.
#[derive(Clone, Debug, Default)]
pub struct BuiltinSymbols {
    by_symbol: HashMap<SymbolId, Builtin>,
}

impl BuiltinSymbols {
    pub fn intern(interner: &mut Interner) -> Self {
        Self {
            by_symbol: Builtin::ALL
                .into_iter()
                .map(|builtin| (interner.intern(builtin.name()), builtin))
                .collect(),
        }
    }

    pub fn lookup(&self, symbol: SymbolId) -> Option<Builtin> {
        self.by_symbol.get(&symbol).copied()
    }
}
