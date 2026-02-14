use std::collections::{BTreeMap, HashSet};

use crate::common::ids::{ExprId, LinearExprId, SymbolId};
use crate::ir::core::{CoreProgram, ExprKind, Literal};
use crate::ir::linear::{LinearExpr, LinearProgram};
use crate::pipeline::phases::{
    ConstantEmbedStrategy, ConstantEntry, ConstantKey, ConstantTable, CtorFieldKey, CtorLiteralKey,
    ScalarLiteralKey,
};

pub const MAX_CONST_ENTRY_BYTES: usize = 1024;
pub const MAX_CONST_POOL_BYTES: usize = 16 * 1024;
pub const LARGE_SERIALIZABLE_POOL_BYTES: usize = 512;
const ESTIMATED_VALUE_BYTES: usize = 16;
const ESTIMATED_CTOR_BYTES: usize = 32;

pub fn build_for_core(program: &CoreProgram) -> ConstantTable {
    let mut counts = ConstantCounts::default();
    let mut seen_strings = HashSet::new();

    for expr in program.exprs() {
        match &expr.kind {
            ExprKind::Literal(literal) => {
                if let Some(key) = ScalarLiteralKey::from_literal(literal) {
                    *counts.scalars.entry(key).or_default() += 1;
                }
                if let Literal::String(value) = literal
                    && seen_strings.insert(value.clone())
                {
                    counts.strings.push(value.clone());
                }
            }
            ExprKind::MakeStruct { ty, fields } => {
                if let Some(key) = ctor_key_from_core(program, *ty, SymbolId::INVALID, fields) {
                    *counts.ctors.entry(key).or_default() += 1;
                }
            }
            ExprKind::MakeEnum {
                ty,
                variant,
                fields,
            } => {
                if let Some(key) = ctor_key_from_core(program, *ty, *variant, fields) {
                    *counts.ctors.entry(key).or_default() += 1;
                }
            }
            ExprKind::Var(_)
            | ExprKind::Unary { .. }
            | ExprKind::Binary { .. }
            | ExprKind::PureCall { .. }
            | ExprKind::Error(_) => {}
        }
    }

    build_constant_table(counts)
}

pub fn build_for_linear(program: &LinearProgram) -> ConstantTable {
    let mut counts = ConstantCounts::default();
    let mut seen_strings = HashSet::new();

    for expr in program.exprs() {
        match &expr.kind {
            LinearExpr::Literal(literal) => {
                if let Some(key) = ScalarLiteralKey::from_literal(literal) {
                    *counts.scalars.entry(key).or_default() += 1;
                }
                if let Literal::String(value) = literal
                    && seen_strings.insert(value.clone())
                {
                    counts.strings.push(value.clone());
                }
            }
            LinearExpr::MakeStruct { ty, fields } => {
                if let Some(key) =
                    ctor_key_from_linear_expr(program, *ty, SymbolId::INVALID, fields)
                {
                    *counts.ctors.entry(key).or_default() += 1;
                }
            }
            LinearExpr::MakeEnum {
                ty,
                variant,
                fields,
            } => {
                if let Some(key) = ctor_key_from_linear_expr(program, *ty, *variant, fields) {
                    *counts.ctors.entry(key).or_default() += 1;
                }
            }
            LinearExpr::Var(_)
            | LinearExpr::Unary { .. }
            | LinearExpr::Binary { .. }
            | LinearExpr::PureCall { .. }
            | LinearExpr::Error => {}
        }
    }

    build_constant_table(counts)
}

#[derive(Clone, Debug, Default)]
struct ConstantCounts {
    scalars: BTreeMap<ScalarLiteralKey, usize>,
    strings: Vec<String>,
    ctors: BTreeMap<CtorLiteralKey, usize>,
}

fn build_constant_table(counts: ConstantCounts) -> ConstantTable {
    let mut budget = ConstPoolBudget::new(MAX_CONST_POOL_BYTES);
    let mut entries = Vec::new();

    for (key, count) in counts.scalars {
        if count < 2 {
            continue;
        }
        if !budget.try_reserve(ESTIMATED_VALUE_BYTES) {
            continue;
        }
        entries.push(ConstantEntry {
            key: ConstantKey::Scalar(key),
            strategy: ConstantEmbedStrategy::StaticConst,
            estimated_size_bytes: ESTIMATED_VALUE_BYTES,
        });
    }

    for value in counts.strings {
        let estimated_size_bytes = value.len().saturating_add(1);
        if !budget.try_reserve(estimated_size_bytes) {
            continue;
        }
        entries.push(ConstantEntry {
            key: ConstantKey::String(value),
            strategy: ConstantEmbedStrategy::StaticConst,
            estimated_size_bytes,
        });
    }

    for (key, count) in counts.ctors {
        let estimated_size_bytes = estimate_ctor_pool_bytes(&key);
        if !should_pool_ctor_literal(count, estimated_size_bytes) {
            continue;
        }
        if !budget.try_reserve(estimated_size_bytes) {
            continue;
        }
        entries.push(ConstantEntry {
            key: ConstantKey::Ctor(key),
            strategy: ConstantEmbedStrategy::Pooled,
            estimated_size_bytes,
        });
    }

    ConstantTable {
        entries,
        entry_cap_bytes: MAX_CONST_ENTRY_BYTES,
        unit_cap_bytes: MAX_CONST_POOL_BYTES,
        total_size_bytes: budget.used_bytes,
    }
}

#[derive(Clone, Copy, Debug)]
struct ConstPoolBudget {
    used_bytes: usize,
    max_bytes: usize,
}

impl ConstPoolBudget {
    fn new(max_bytes: usize) -> Self {
        Self {
            used_bytes: 0,
            max_bytes,
        }
    }

    fn try_reserve(&mut self, bytes: usize) -> bool {
        if bytes == 0 {
            return true;
        }
        if bytes > MAX_CONST_ENTRY_BYTES {
            return false;
        }
        if self.used_bytes.saturating_add(bytes) > self.max_bytes {
            return false;
        }
        self.used_bytes = self.used_bytes.saturating_add(bytes);
        true
    }
}

fn ctor_key_from_core(
    program: &CoreProgram,
    ty: SymbolId,
    variant: SymbolId,
    fields: &[ExprId],
) -> Option<CtorLiteralKey> {
    let mut field_keys = Vec::with_capacity(fields.len());
    for field in fields {
        let literal = match &program.expr(*field)?.kind {
            ExprKind::Literal(literal) => literal,
            _ => return None,
        };
        field_keys.push(CtorFieldKey::from_literal(literal)?);
    }
    Some(CtorLiteralKey {
        ty,
        variant,
        fields: field_keys,
    })
}

pub fn ctor_key_from_linear_expr(
    program: &LinearProgram,
    ty: SymbolId,
    variant: SymbolId,
    fields: &[LinearExprId],
) -> Option<CtorLiteralKey> {
    let mut field_keys = Vec::with_capacity(fields.len());
    for field in fields {
        let literal = match &program.expr(*field)?.kind {
            LinearExpr::Literal(literal) => literal,
            _ => return None,
        };
        field_keys.push(CtorFieldKey::from_literal(literal)?);
    }
    Some(CtorLiteralKey {
        ty,
        variant,
        fields: field_keys,
    })
}

fn should_pool_ctor_literal(use_count: usize, estimated_bytes: usize) -> bool {
    use_count >= 2 || estimated_bytes >= LARGE_SERIALIZABLE_POOL_BYTES
}

fn estimate_ctor_pool_bytes(key: &CtorLiteralKey) -> usize {
    let fields_bytes = key
        .fields
        .iter()
        .map(estimate_ctor_field_pool_bytes)
        .sum::<usize>();
    ESTIMATED_VALUE_BYTES
        .saturating_add(ESTIMATED_CTOR_BYTES)
        .saturating_add(key.fields.len().saturating_mul(ESTIMATED_VALUE_BYTES))
        .saturating_add(fields_bytes)
}

fn estimate_ctor_field_pool_bytes(field: &CtorFieldKey) -> usize {
    match field {
        CtorFieldKey::Unit => 1,
        CtorFieldKey::Bool(_) => 1,
        CtorFieldKey::Int(_) => 8,
        CtorFieldKey::Char(_) => 4,
        CtorFieldKey::Float(_) => 8,
        CtorFieldKey::String(value) => value.len().saturating_add(1),
    }
}
