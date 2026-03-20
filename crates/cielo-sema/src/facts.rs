//! Semantic facts that are valid for one Core snapshot.

use std::collections::HashMap;

use cielo_base::densemap::DenseMap;
use cielo_base::{EffectLabelId, TypeId, VarId};
use cielo_ir::effect::{EffectProperties, SortedEffectRow};

use crate::ownership::OwnershipClass;
use crate::ty::Persistability;

#[derive(Clone, Debug, Default)]
pub struct SemanticTables {
    pub type_of_expr: Vec<Option<TypeId>>,
    pub effects_of_expr: Vec<SortedEffectRow>,
    pub effects_of_stmt: Vec<SortedEffectRow>,
    pub ownership_of_expr: Vec<OwnershipClass>,
    pub ownership_of_var: DenseMap<VarId, OwnershipClass>,
    pub ownership_of_type: Vec<OwnershipClass>,
    pub persistability_of_type: Vec<Persistability>,
    pub effect_properties: HashMap<EffectLabelId, EffectProperties>,
}

impl SemanticTables {
    pub fn with_counts(expr_count: usize, stmt_count: usize) -> Self {
        Self {
            type_of_expr: vec![None; expr_count],
            effects_of_expr: vec![SortedEffectRow::empty(); expr_count],
            effects_of_stmt: vec![SortedEffectRow::empty(); stmt_count],
            ownership_of_expr: vec![OwnershipClass::BorrowedView; expr_count],
            ownership_of_var: DenseMap::default(),
            ownership_of_type: Vec::new(),
            persistability_of_type: Vec::new(),
            effect_properties: HashMap::new(),
        }
    }
}
