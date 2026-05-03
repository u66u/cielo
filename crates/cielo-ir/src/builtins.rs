//! Operations the runtime implements directly.
//!
//! A builtin is not declared in source and is not an effect: a call reaches
//! the runtime without passing through the handler stack, so output no longer
//! depends on an effect escaping every handler.

use cielo_base::ids::SymbolId;
use cielo_base::symbols::Interner;
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Builtin {
    Print,
}

impl Builtin {
    pub const ALL: [Self; 1] = [Self::Print];

    pub fn name(self) -> &'static str {
        match self {
            Self::Print => "print",
        }
    }

    /// The C runtime function implementing this builtin. Codegen calls it
    /// directly; the symbol-keyed table in the header is only for operations
    /// that arrive through `perform`.
    pub fn c_symbol(self) -> &'static str {
        match self {
            Self::Print => "cielo_builtin_print",
        }
    }

    pub fn arity(self) -> usize {
        match self {
            Self::Print => 1,
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
