use std::collections::HashMap;

use crate::common::ids::SymbolId;

#[derive(Clone, Debug, Default)]
pub struct Interner {
    symbols: Vec<String>,
    lookup: HashMap<String, SymbolId>,
}

impl Interner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.symbols.len()
    }

    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }

    pub fn intern(&mut self, text: &str) -> SymbolId {
        if let Some(id) = self.lookup.get(text) {
            return *id;
        }
        let id = SymbolId::new(self.symbols.len());
        let owned = text.to_owned();
        self.symbols.push(owned.clone());
