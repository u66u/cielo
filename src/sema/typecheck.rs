// Pass 2/8: typecheck_core (v0 type + stmt-effect table population)
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
// - Type inference: fixed-point over Expr/Stmt constraints
// - Effect inference: memoized DFS over stmt graph (linear in stmt count)

use std::collections::{HashMap, HashSet};

use crate::common::diagnostics::DiagnosticBag;
use crate::common::ids::{ExprId, FuncId, StmtId, SymbolId, TypeId, VarId};
use crate::ir::core::{BinaryOp, CoreProgram, ExprKind, Literal, UnaryOp};
use crate::pipeline::phases::SemanticTables;
use crate::sema::effect::SortedEffectRow;
use crate::sema::ty::{EnumVariant, PrimitiveType, StructField, TypeKind, TypeStore};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PrimitiveTypeIds {
    pub unit: TypeId,
    pub bool_: TypeId,
    pub int: TypeId,
    pub float: TypeId,
    pub char_: TypeId,
    pub string: TypeId,
}

pub fn typecheck_core(program: &CoreProgram, diagnostics: &mut DiagnosticBag) -> SemanticTables {
    let mut store = TypeStore::new();
    let primitives = intern_primitives(&mut store);
    let adt_types = intern_program_adts(program, &mut store);

    let mut sema = SemanticTables::with_counts(program.exprs().len(), program.stmts().len());
    sema.effects_of_expr = vec![SortedEffectRow::empty(); program.exprs().len()];

    let mut var_types: Vec<Option<TypeId>> = Vec::new();
    let mut func_returns: Vec<Option<TypeId>> = program
        .functions()
        .iter()
        .map(|f| f.return_type.is_valid().then_some(f.return_type))
        .collect();

    for function in program.functions() {
        for (idx, param_var) in function.params.iter().copied().enumerate() {
            if let Some(param_ty) = function.param_types.get(idx).copied()
                && param_ty.is_valid()
            {
                let _ = set_var_type(param_var, param_ty, &mut var_types);
            }
        }
    }

    let stmt_returns = precompute_stmt_returns(program);

    let mut changed = true;
    while changed {
        changed = false;

        for (idx, _) in program.exprs().iter().enumerate() {
            let expr_id = ExprId::new(idx);
            changed |= infer_expr_type(
                program,
                expr_id,
                &adt_types,
                primitives,
                &mut sema.type_of_expr,
                &mut var_types,
                &mut func_returns,
            );
            sema.effects_of_expr[idx] = SortedEffectRow::empty();
        }

        for (idx, stmt) in program.stmts().iter().enumerate() {
            let stmt_id = StmtId::new(idx);
            changed |= constrain_stmt(
                program,
                stmt_id,
                stmt,
                &stmt_returns,
                primitives,
                &mut sema.type_of_expr,
                &mut var_types,
                &mut func_returns,
            );
        }

        for (func_idx, function) in program.functions().iter().enumerate() {
            let func_id = FuncId::new(func_idx);
            for return_expr in &stmt_returns[function.body.index()] {
                changed |= unify_expr_with_func_return(
                    program,
                    *return_expr,
                    func_id,
                    &mut sema.type_of_expr,
                    &mut var_types,
                    &mut func_returns,
                );
            }
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
        .map(|(idx, _)| store.persistability(TypeId::new(idx)))
        .collect();

    infer_stmt_effects(program, &mut sema.effects_of_stmt);
    sema
}

fn infer_expr_type(
    program: &CoreProgram,
    expr_id: ExprId,
    adt_types: &HashMap<SymbolId, TypeId>,
    prim: PrimitiveTypeIds,
    expr_types: &mut [Option<TypeId>],
    var_types: &mut Vec<Option<TypeId>>,
    func_returns: &mut [Option<TypeId>],
) -> bool {
    let Some(expr) = program.expr(expr_id) else {
        return false;
    };

    match &expr.kind {
        ExprKind::Literal(lit) => set_expr_type(
            program,
            expr_id,
            type_for_literal(lit, prim),
            expr_types,
            var_types,
        ),
        ExprKind::Var(var) => unify_expr_with_var(program, expr_id, *var, expr_types, var_types),
        ExprKind::Unary { op, expr } => {
            infer_unary(program, expr_id, *op, *expr, prim, expr_types, var_types)
        }
        ExprKind::Binary { op, lhs, rhs } => infer_binary(
            program, expr_id, *op, *lhs, *rhs, prim, expr_types, var_types,
        ),
        ExprKind::PureCall { callee, args } => constrain_call_signature(
            program,
            *callee,
            args,
            Some(expr_id),
            None,
            expr_types,
            var_types,
            func_returns,
        ),
        ExprKind::MakeStruct { ty, fields } => {
            let mut changed = false;
            if let Some(adt_ty) = adt_types.get(ty).copied() {
                changed |= set_expr_type(program, expr_id, adt_ty, expr_types, var_types);
            }
            for field in fields {
                changed |= infer_expr_type(
                    program,
                    *field,
                    adt_types,
                    prim,
                    expr_types,
                    var_types,
                    func_returns,
                );
            }
            changed
        }
        ExprKind::MakeEnum { ty, fields, .. } => {
            let mut changed = false;
            if let Some(adt_ty) = adt_types.get(ty).copied() {
                changed |= set_expr_type(program, expr_id, adt_ty, expr_types, var_types);
            }
            for field in fields {
                changed |= infer_expr_type(
                    program,
                    *field,
                    adt_types,
                    prim,
                    expr_types,
                    var_types,
                    func_returns,
                );
            }
            changed
        }
        ExprKind::Error(_) => false,
    }
}

fn constrain_stmt(
    program: &CoreProgram,
    _stmt_id: StmtId,
    stmt: &crate::ir::core::StmtNode,
    stmt_returns: &[Vec<ExprId>],
    prim: PrimitiveTypeIds,
    expr_types: &mut [Option<TypeId>],
    var_types: &mut Vec<Option<TypeId>>,
    func_returns: &mut [Option<TypeId>],
) -> bool {
    match &stmt.kind {
        crate::ir::core::StmtKind::Return(_) => false,
        crate::ir::core::StmtKind::Let { binding, value, .. } => {
            unify_expr_with_var(program, *value, *binding, expr_types, var_types)
        }
        crate::ir::core::StmtKind::Val { binding, value, .. } => {
            let mut changed = false;
            for ret_expr in &stmt_returns[value.index()] {
                changed |= unify_expr_with_var(program, *ret_expr, *binding, expr_types, var_types);
            }
            changed
        }
        crate::ir::core::StmtKind::Call {
            result,
            callee,
            args,
            ..
        } => constrain_call_signature(
            program,
            *callee,
            args,
            None,
            Some(*result),
            expr_types,
            var_types,
            func_returns,
        ),
        crate::ir::core::StmtKind::If { cond, .. } => {
            set_expr_type(program, *cond, prim.bool_, expr_types, var_types)
        }
        crate::ir::core::StmtKind::Match { .. } => false,
        crate::ir::core::StmtKind::Perform { result, .. } => {
            if let Some(var) = result {
                set_var_type(*var, prim.unit, var_types)
            } else {
                false
            }
        }
        crate::ir::core::StmtKind::Handle { handler, body, .. } => {
            let mut changed = false;
            if let Some(handler_def) = program.handlers().get(handler.index()) {
                for body_ret in &stmt_returns[body.index()] {
                    changed |= unify_expr_with_var(
                        program,
                        *body_ret,
                        handler_def.return_param,
                        expr_types,
                        var_types,
                    );
                }
                for clause in &handler_def.clauses {
                    for clause_ret in &stmt_returns[clause.body.index()] {
                        changed |= unify_expr_with_var(
                            program,
                            *clause_ret,
                            handler_def.return_param,
                            expr_types,
                            var_types,
                        );
                    }
                }
                for handler_ret in &stmt_returns[handler_def.return_body.index()] {
                    changed |= unify_expr_with_var(
                        program,
                        *handler_ret,
                        handler_def.return_param,
                        expr_types,
                        var_types,
                    );
                }
            }
            changed
        }
        crate::ir::core::StmtKind::Stage { .. }
        | crate::ir::core::StmtKind::Hole { .. }
        | crate::ir::core::StmtKind::Error(_) => false,
    }
}

fn constrain_call_signature(
    program: &CoreProgram,
    callee: FuncId,
    args: &[ExprId],
    result_expr: Option<ExprId>,
    result_var: Option<VarId>,
    expr_types: &mut [Option<TypeId>],
    var_types: &mut Vec<Option<TypeId>>,
    func_returns: &mut [Option<TypeId>],
) -> bool {
    let Some(function) = program.function(callee) else {
        return false;
    };

    let mut changed = false;
    for (arg_expr, param_var) in args.iter().copied().zip(function.params.iter().copied()) {
        changed |= unify_expr_with_var(program, arg_expr, param_var, expr_types, var_types);
    }

    if let Some(expr_id) = result_expr {
        changed |= unify_expr_with_func_return(
            program,
            expr_id,
            callee,
            expr_types,
            var_types,
            func_returns,
        );
    }
    if let Some(var_id) = result_var {
        changed |= unify_var_with_func_return(var_id, callee, var_types, func_returns);
    }
    changed
}

fn infer_unary(
    program: &CoreProgram,
    expr_id: ExprId,
    op: UnaryOp,
    inner: ExprId,
    prim: PrimitiveTypeIds,
    expr_types: &mut [Option<TypeId>],
    var_types: &mut Vec<Option<TypeId>>,
) -> bool {
    let mut changed = false;
    let inner_ty = expr_types[inner.index()];
    let this_ty = expr_types[expr_id.index()];

    match op {
        UnaryOp::Neg => {
            let inferred = match (inner_ty, this_ty) {
                (Some(ty), _) if ty == prim.int || ty == prim.float => Some(ty),
                (_, Some(ty)) if ty == prim.int || ty == prim.float => Some(ty),
                _ => None,
            };
            if let Some(ty) = inferred {
                changed |= set_expr_type(program, inner, ty, expr_types, var_types);
                changed |= set_expr_type(program, expr_id, ty, expr_types, var_types);
            }
        }
        UnaryOp::Not => {
            changed |= set_expr_type(program, inner, prim.bool_, expr_types, var_types);
            changed |= set_expr_type(program, expr_id, prim.bool_, expr_types, var_types);
        }
    }

    changed
}

fn infer_binary(
    program: &CoreProgram,
    expr_id: ExprId,
    op: BinaryOp,
    lhs: ExprId,
    rhs: ExprId,
    prim: PrimitiveTypeIds,
    expr_types: &mut [Option<TypeId>],
    var_types: &mut Vec<Option<TypeId>>,
) -> bool {
    let mut changed = false;
    let left = expr_types[lhs.index()];
    let right = expr_types[rhs.index()];

    match op {
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
            let inferred = match (left, right) {
                (Some(ty), _) if ty == prim.int || ty == prim.float => Some(ty),
                (_, Some(ty)) if ty == prim.int || ty == prim.float => Some(ty),
                _ => None,
            };
            if let Some(ty) = inferred {
                changed |= set_expr_type(program, lhs, ty, expr_types, var_types);
                changed |= set_expr_type(program, rhs, ty, expr_types, var_types);
            }
            if let (Some(left), Some(right)) = (expr_types[lhs.index()], expr_types[rhs.index()]) {
                if left == right && (left == prim.int || left == prim.float) {
                    changed |= set_expr_type(program, expr_id, left, expr_types, var_types);
                }
            }
        }
        BinaryOp::Eq | BinaryOp::Ne => {
            match (left, right) {
                (Some(ty), None) => {
                    changed |= set_expr_type(program, rhs, ty, expr_types, var_types)
                }
                (None, Some(ty)) => {
                    changed |= set_expr_type(program, lhs, ty, expr_types, var_types)
                }
                _ => {}
            }
            if let (Some(left), Some(right)) = (expr_types[lhs.index()], expr_types[rhs.index()]) {
                if left == right {
                    changed |= set_expr_type(program, expr_id, prim.bool_, expr_types, var_types);
                }
            }
        }
        BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
            let inferred = match (left, right) {
                (Some(ty), _) if ty == prim.int || ty == prim.float => Some(ty),
                (_, Some(ty)) if ty == prim.int || ty == prim.float => Some(ty),
                _ => None,
            };
            if let Some(ty) = inferred {
                changed |= set_expr_type(program, lhs, ty, expr_types, var_types);
                changed |= set_expr_type(program, rhs, ty, expr_types, var_types);
            }
            if let (Some(left), Some(right)) = (expr_types[lhs.index()], expr_types[rhs.index()]) {
                if left == right && (left == prim.int || left == prim.float) {
                    changed |= set_expr_type(program, expr_id, prim.bool_, expr_types, var_types);
                }
            }
        }
        BinaryOp::And | BinaryOp::Or => {
            changed |= set_expr_type(program, lhs, prim.bool_, expr_types, var_types);
            changed |= set_expr_type(program, rhs, prim.bool_, expr_types, var_types);
            if expr_types[lhs.index()] == Some(prim.bool_)
                && expr_types[rhs.index()] == Some(prim.bool_)
            {
                changed |= set_expr_type(program, expr_id, prim.bool_, expr_types, var_types);
            }
        }
    }

    changed
}

fn unify_expr_with_var(
    program: &CoreProgram,
    expr_id: ExprId,
    var_id: VarId,
    expr_types: &mut [Option<TypeId>],
    var_types: &mut Vec<Option<TypeId>>,
) -> bool {
    let mut changed = false;
    if let Some(var_ty) = get_var_type(var_id, var_types) {
        changed |= set_expr_type(program, expr_id, var_ty, expr_types, var_types);
    }
    if let Some(expr_ty) = expr_types[expr_id.index()] {
        changed |= set_var_type(var_id, expr_ty, var_types);
    }
    changed
}

fn unify_expr_with_func_return(
    program: &CoreProgram,
    expr_id: ExprId,
    func_id: FuncId,
    expr_types: &mut [Option<TypeId>],
    var_types: &mut Vec<Option<TypeId>>,
    func_returns: &mut [Option<TypeId>],
) -> bool {
    let mut changed = false;
    if let Some(ret_ty) = func_returns[func_id.index()] {
        changed |= set_expr_type(program, expr_id, ret_ty, expr_types, var_types);
    }
    if let Some(expr_ty) = expr_types[expr_id.index()] {
        changed |= set_func_return_type(func_id, expr_ty, func_returns);
    }
    changed
}

fn unify_var_with_func_return(
    var_id: VarId,
    func_id: FuncId,
    var_types: &mut Vec<Option<TypeId>>,
    func_returns: &mut [Option<TypeId>],
) -> bool {
    let mut changed = false;
    if let Some(ret_ty) = func_returns[func_id.index()] {
        changed |= set_var_type(var_id, ret_ty, var_types);
    }
    if let Some(var_ty) = get_var_type(var_id, var_types) {
        changed |= set_func_return_type(func_id, var_ty, func_returns);
    }
    changed
}

fn set_expr_type(
    program: &CoreProgram,
    expr_id: ExprId,
    ty: TypeId,
    expr_types: &mut [Option<TypeId>],
    var_types: &mut Vec<Option<TypeId>>,
) -> bool {
    let mut changed = set_slot(expr_types.get_mut(expr_id.index()), ty);
    if let Some(crate::ir::core::ExprKind::Var(var)) = program.expr(expr_id).map(|e| &e.kind) {
        changed |= set_var_type(*var, ty, var_types);
    }
    changed
}

fn set_var_type(var_id: VarId, ty: TypeId, var_types: &mut Vec<Option<TypeId>>) -> bool {
    let idx = var_id.index();
    if idx >= var_types.len() {
        var_types.resize(idx + 1, None);
    }
    set_slot(var_types.get_mut(idx), ty)
}

fn set_func_return_type(func_id: FuncId, ty: TypeId, func_returns: &mut [Option<TypeId>]) -> bool {
    set_slot(func_returns.get_mut(func_id.index()), ty)
}

fn get_var_type(var_id: VarId, var_types: &[Option<TypeId>]) -> Option<TypeId> {
    var_types.get(var_id.index()).copied().flatten()
}

fn set_slot(slot: Option<&mut Option<TypeId>>, ty: TypeId) -> bool {
    let Some(slot) = slot else {
        return false;
    };
    match slot {
        Some(existing) if *existing == ty => false,
        Some(_) => false,
        None => {
            *slot = Some(ty);
            true
        }
    }
}

fn precompute_stmt_returns(program: &CoreProgram) -> Vec<Vec<ExprId>> {
    let mut memo: Vec<Option<Vec<ExprId>>> = vec![None; program.stmts().len()];
    for idx in 0..program.stmts().len() {
        let stmt_id = StmtId::new(idx);
        let mut visiting = HashSet::new();
        let returns = collect_stmt_returns(program, stmt_id, &mut memo, &mut visiting);
        memo[idx] = Some(returns);
    }
    memo.into_iter()
        .map(|entry| entry.unwrap_or_default())
        .collect()
}

fn collect_stmt_returns(
    program: &CoreProgram,
    stmt_id: StmtId,
    memo: &mut [Option<Vec<ExprId>>],
    visiting: &mut HashSet<StmtId>,
) -> Vec<ExprId> {
    if let Some(cached) = memo.get(stmt_id.index()).and_then(Clone::clone) {
        return cached;
    }
    if !visiting.insert(stmt_id) {
        return Vec::new();
    }

    let mut out = Vec::new();
    match program.stmt(stmt_id).map(|node| &node.kind) {
        Some(crate::ir::core::StmtKind::Return(expr)) => out.push(*expr),
        Some(crate::ir::core::StmtKind::Let { next, .. })
        | Some(crate::ir::core::StmtKind::Val { next, .. })
        | Some(crate::ir::core::StmtKind::Call { next, .. })
        | Some(crate::ir::core::StmtKind::Perform { next, .. }) => {
            out.extend(collect_stmt_returns(program, *next, memo, visiting));
        }
        Some(crate::ir::core::StmtKind::If {
            then_branch,
            else_branch,
            ..
        }) => {
            out.extend(collect_stmt_returns(program, *then_branch, memo, visiting));
            out.extend(collect_stmt_returns(program, *else_branch, memo, visiting));
        }
        Some(crate::ir::core::StmtKind::Match { arms, default, .. }) => {
            for arm in arms {
                out.extend(collect_stmt_returns(program, arm.body, memo, visiting));
            }
            if let Some(default_stmt) = default {
                out.extend(collect_stmt_returns(program, *default_stmt, memo, visiting));
            }
        }
        Some(crate::ir::core::StmtKind::Handle { body, next, .. })
        | Some(crate::ir::core::StmtKind::Stage { body, next, .. }) => {
            if let Some(next_stmt) = next {
                out.extend(collect_stmt_returns(program, *next_stmt, memo, visiting));
            } else {
                out.extend(collect_stmt_returns(program, *body, memo, visiting));
            }
        }
        Some(crate::ir::core::StmtKind::Hole { .. })
        | Some(crate::ir::core::StmtKind::Error(_))
        | None => {}
    }

    visiting.remove(&stmt_id);

    let mut dedup = HashSet::new();
    out.retain(|expr| dedup.insert(*expr));
    memo[stmt_id.index()] = Some(out.clone());
    out
}

fn intern_program_adts(program: &CoreProgram, store: &mut TypeStore) -> HashMap<SymbolId, TypeId> {
    let mut adt_types = HashMap::new();

    for decl in program.structs() {
        if adt_types.contains_key(&decl.name) {
            continue;
        }
        let fields = (0..decl.field_count)
            .map(|_| StructField {
                name: SymbolId::INVALID,
                ty: TypeId::INVALID,
            })
            .collect();
        let ty = store.intern(TypeKind::Struct {
            name: decl.name,
            fields,
        });
        adt_types.insert(decl.name, ty);
    }

    for decl in program.enums() {
        if adt_types.contains_key(&decl.name) {
            continue;
        }
        let variants = decl
            .variants
            .iter()
            .map(|variant| EnumVariant {
                name: variant.name,
                fields: (0..variant.field_count).map(|_| TypeId::INVALID).collect(),
            })
            .collect();
        let ty = store.intern(TypeKind::Enum {
            name: decl.name,
            variants,
        });
        adt_types.insert(decl.name, ty);
    }

    adt_types
}

fn infer_stmt_effects(program: &CoreProgram, out: &mut [SortedEffectRow]) {
    let mut memo: Vec<Option<SortedEffectRow>> = vec![None; out.len()];
    for idx in 0..program.stmts().len() {
        let stmt_id = StmtId::new(idx);
        let row = infer_stmt_effect(program, stmt_id, &mut memo);
        out[idx] = row;
    }
}

fn infer_stmt_effect(
    program: &CoreProgram,
    stmt_id: StmtId,
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
        Some(crate::ir::core::StmtKind::Stage { body, next, .. }) => {
            let mut row = infer_stmt_effect(program, *body, memo);
            if let Some(next_stmt) = next {
                row = row.union(&infer_stmt_effect(program, *next_stmt, memo));
            }
            row
        }
        Some(crate::ir::core::StmtKind::Hole { .. })
        | Some(crate::ir::core::StmtKind::Error(_))
        | None => SortedEffectRow::empty(),
    };

    memo[stmt_id.index()] = Some(row.clone());
    row
}

fn type_for_literal(lit: &Literal, prim: PrimitiveTypeIds) -> TypeId {
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
