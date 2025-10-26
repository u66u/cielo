// Pass: typecheck_core (v0 type + stmt-effect table population)
//
// Inputs:
// - CoreProgram produced by lowering
//
// Outputs:
// - SemanticTables: expr types, expr effects, stmt effects, persistability
// - Diagnostics for incomplete type inference in v0
//
// Invariants:
// - Expr effects are always empty (Expr/Stmt split)
// - Stmt effects conservatively approximate dynamic effect flow
// - Handle nodes discharge their handled effect label from body summaries
//
// Diagnostics:
// - `TYPE_INFER_INCOMPLETE` warnings for unresolved expr types
//
// Complexity:
// - Type inference: fixpoint over expr graph
// - Effect inference: memoized DFS over stmt graph (linear in stmt count)

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

    let mut sema = SemanticTables::with_counts(program.exprs().len(), program.stmts().len());
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

    infer_stmt_effects(program, &mut sema.effects_of_stmt);
    sema
}

fn infer_stmt_effects(program: &CoreProgram, out: &mut [SortedEffectRow]) {
    let mut memo: Vec<Option<SortedEffectRow>> = vec![None; out.len()];
    for idx in 0..program.stmts().len() {
        let stmt_id = crate::common::ids::StmtId::new(idx);
        let row = infer_stmt_effect(program, stmt_id, &mut memo);
        out[idx] = row;
    }
}

fn infer_stmt_effect(
    program: &CoreProgram,
    stmt_id: crate::common::ids::StmtId,
    memo: &mut [Option<SortedEffectRow>],
) -> SortedEffectRow {
    if let Some(row) = memo.get(stmt_id.index()).and_then(Clone::clone) {
        return row;
    }

    let row = match program.stmt(stmt_id).map(|node| &node.kind) {
        Some(crate::ir::core::StmtKind::Return(_)) => SortedEffectRow::empty(),
        Some(crate::ir::core::StmtKind::Let { next, .. }) => {
            infer_stmt_effect(program, *next, memo)
        }
        Some(crate::ir::core::StmtKind::Val { value, next, .. }) => {
            infer_stmt_effect(program, *value, memo).union(&infer_stmt_effect(program, *next, memo))
        }
        Some(crate::ir::core::StmtKind::Call { effects, next, .. }) => {
            effects.union(&infer_stmt_effect(program, *next, memo))
        }
        Some(crate::ir::core::StmtKind::Perform { effect, next, .. }) => {
            SortedEffectRow::singleton(*effect).union(&infer_stmt_effect(program, *next, memo))
        }
        Some(crate::ir::core::StmtKind::If {
            then_branch,
            else_branch,
            ..
        }) => infer_stmt_effect(program, *then_branch, memo).union(&infer_stmt_effect(
            program,
            *else_branch,
            memo,
        )),
        Some(crate::ir::core::StmtKind::Match { arms, default, .. }) => {
            let mut row = SortedEffectRow::empty();
            for arm in arms {
                row = row.union(&infer_stmt_effect(program, arm.body, memo));
            }
            if let Some(default_stmt) = default {
                row = row.union(&infer_stmt_effect(program, *default_stmt, memo));
            }
            row
        }
        Some(crate::ir::core::StmtKind::Handle {
            handler,
            body,
            next,
        }) => {
            let mut row = infer_stmt_effect(program, *body, memo);
            let handled_effect = program.handlers().get(handler.index()).map(|h| h.effect);
            if let Some(effect) = handled_effect {
                row = row.subtract(&SortedEffectRow::singleton(effect));
            }
            if let Some(next_stmt) = next {
                row = row.union(&infer_stmt_effect(program, *next_stmt, memo));
            }
            row
        }
        Some(crate::ir::core::StmtKind::Hole { .. })
        | Some(crate::ir::core::StmtKind::Error(_)) => SortedEffectRow::empty(),
        None => SortedEffectRow::empty(),
    };

    memo[stmt_id.index()] = Some(row.clone());
    row
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
        ExprKind::MakeEnum { ty, .. } => Some(*ty),
        ExprKind::Error(_) => None,
    }
}

fn infer_binary_type(
    op: BinaryOp,
    left: Option<crate::common::ids::TypeId>,
    right: Option<crate::common::ids::TypeId>,
    prim: PrimitiveTypeIds,
) -> Option<crate::common::ids::TypeId> {
    let (left, right) = (left?, right?);
    match op {
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
            if left == right && (left == prim.int || left == prim.float) {
                Some(left)
            } else {
                None
            }
        }
        BinaryOp::Eq | BinaryOp::Ne => {
            if left == right {
                Some(prim.bool_)
            } else {
                None
            }
        }
        BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
            if left == right && (left == prim.int || left == prim.float) {
                Some(prim.bool_)
            } else {
                None
            }
        }
        BinaryOp::And | BinaryOp::Or => {
            if left == prim.bool_ && right == prim.bool_ {
                Some(prim.bool_)
            } else {
                None
            }
        }
    }
}

fn type_for_literal(lit: &Literal, prim: PrimitiveTypeIds) -> crate::common::ids::TypeId {
    match lit {
        Literal::Unit => prim.unit,
        Literal::Bool(_) => prim.bool_,
        Literal::Int(_) => prim.int,
        Literal::Float(_) => prim.float,
        Literal::Char(_) => prim.char_,
        Literal::String(_) => prim.string,
    }
}

fn intern_primitives(store: &mut TypeStore) -> PrimitiveTypeIds {
    PrimitiveTypeIds {
        unit: store.intern(TypeKind::Primitive(PrimitiveType::Unit)),
        bool_: store.intern(TypeKind::Primitive(PrimitiveType::Bool)),
        int: store.intern(TypeKind::Primitive(PrimitiveType::Int)),
        float: store.intern(TypeKind::Primitive(PrimitiveType::Float)),
        char_: store.intern(TypeKind::Primitive(PrimitiveType::Char)),
        string: store.intern(TypeKind::Primitive(PrimitiveType::String)),
    }
}
