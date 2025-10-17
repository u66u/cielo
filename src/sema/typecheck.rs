use crate::common::diagnostics::DiagnosticBag;
use crate::ir::core::{BinaryOp, CoreProgram, ExprKind, Literal, UnaryOp};
use crate::pipeline::phases::SemanticTables;
use crate::sema::effect::SortedEffectRow;
use crate::sema::ty::{PrimitiveType, TypeKind, TypeStore};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PrimitiveTypeIds {
    pub unit: crate::common::ids::TypeId,
    pub bool_: crate::common::ids::TypeId,
    pub int: crate::common::ids::TypeId,
    pub float: crate::common::ids::TypeId,
    pub char_: crate::common::ids::TypeId,
    pub string: crate::common::ids::TypeId,
}

pub fn typecheck_core(program: &CoreProgram, diagnostics: &mut DiagnosticBag) -> SemanticTables {
    let mut store = TypeStore::new();
    let primitives = intern_primitives(&mut store);

    let mut sema = SemanticTables::with_expr_count(program.exprs().len());
    sema.effects_of_expr = vec![SortedEffectRow::empty(); program.exprs().len()];

    let mut changed = true;
    while changed {
        changed = false;
        for (idx, expr) in program.exprs().iter().enumerate() {
            let expr_id = crate::common::ids::ExprId::new(idx);
            let prev = sema.type_of_expr[idx];
            let inferred = infer_expr_type(expr, &sema.type_of_expr, primitives);
            if prev != inferred {
                sema.type_of_expr[idx] = inferred;
                changed = true;
            }

            if sema.type_of_expr[idx].is_none() {
                continue;
            }

            // v0 policy: Expr nodes are pure by construction.
            sema.effects_of_expr[expr_id.index()] = SortedEffectRow::empty();
        }
    }

    for (idx, expr) in program.exprs().iter().enumerate() {
        if sema.type_of_expr[idx].is_none() {
            diagnostics.warning(
                "TYPE_INFER_INCOMPLETE",
                "Could not infer a complete type for this expression in v0 checker",
                expr.span,
            );
        }
    }

    sema.persistability_of_type = store
        .kinds()
        .iter()
        .enumerate()
        .map(|(idx, _)| store.persistability(crate::common::ids::TypeId::new(idx)))
        .collect();
    sema
}

fn infer_expr_type(
    expr: &crate::ir::core::ExprNode,
    known: &[Option<crate::common::ids::TypeId>],
    prim: PrimitiveTypeIds,
) -> Option<crate::common::ids::TypeId> {
    match &expr.kind {
        ExprKind::Literal(lit) => Some(type_for_literal(lit, prim)),
        ExprKind::Unary { op, expr } => {
            let inner = known.get(expr.index()).copied().flatten();
            match (op, inner) {
                (UnaryOp::Neg, Some(ty)) if ty == prim.int || ty == prim.float => Some(ty),
                (UnaryOp::Not, Some(ty)) if ty == prim.bool_ => Some(prim.bool_),
                _ => None,
            }
        }
        ExprKind::Binary { op, lhs, rhs } => {
            let left = known.get(lhs.index()).copied().flatten();
            let right = known.get(rhs.index()).copied().flatten();
            infer_binary_type(*op, left, right, prim)
        }
        ExprKind::Var(_) => None,
        ExprKind::PureCall { .. } => None,
        ExprKind::MakeStruct { ty, .. } => Some(*ty),
