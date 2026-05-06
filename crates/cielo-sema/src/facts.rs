//! Semantic facts that are valid for one Core snapshot.

use std::collections::HashMap;

use cielo_base::densemap::DenseMap;
use cielo_base::{EffectLabelId, ExprId, StmtId, SymbolId, TypeId, VarId};
use cielo_ir::core::CoreTypeRef;
use cielo_ir::effect::{EffectProperties, SortedEffectRow};
use cielo_ir::ownership::OwnershipClass;

use crate::ty::Persistability;

/// Core splits calls across both arenas: pure calls are expressions, effectful
/// calls are statements.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum CallSite {
    Expr(ExprId),
    Stmt(StmtId),
}

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
    /// Field projections resolved to a positional index. Core carries the field
    /// name because lowering has no types to resolve it against.
    pub field_index_of_expr: HashMap<ExprId, u32>,
    /// Type arguments inferred for each call to a generic function, keyed by the
    /// callee's type-parameter name so monomorphization does not have to agree
    /// with inference on a positional ordering. Absent for non-generic callees.
    pub type_args_of_call: HashMap<CallSite, Vec<(SymbolId, CoreTypeRef)>>,
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
            field_index_of_expr: HashMap::new(),
            effect_properties: HashMap::new(),
            type_args_of_call: HashMap::new(),
        }
    }
}
