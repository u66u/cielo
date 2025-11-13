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
use crate::common::ids::{
    EffectLabelId, ExprId, FuncId, HandlerId, StmtId, SymbolId, TypeId, VarId,
};
use crate::ir::core::{
    BinaryOp, CoreProgram, CoreTypeRef, ExprKind, Literal, PrimitiveTypeRef, UnaryOp,
};
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

struct TypeCheckerContext<'a> {
    program: &'a CoreProgram,
    adt_types: &'a HashMap<SymbolId, TypeId>,
    effect_signatures: &'a EffectSignatureTable,
    prim: PrimitiveTypeIds,
    stmt_returns: &'a [Vec<ExprId>],
    expr_types: &'a mut [Option<TypeId>],
    var_types: &'a mut Vec<Option<TypeId>>,
    func_returns: &'a mut [Option<TypeId>],
}

#[derive(Clone, Debug)]
struct EffectSignature {
    param_types: Vec<Option<TypeId>>,
    return_type: Option<TypeId>,
}

type EffectSignatureTable = HashMap<(EffectLabelId, SymbolId), EffectSignature>;

impl<'a> TypeCheckerContext<'a> {
    fn infer_expr_type(&mut self, expr_id: ExprId) -> bool {
        infer_expr_type(
            self.program,
            expr_id,
            self.adt_types,
            self.prim,
            self.expr_types,
            self.var_types,
            self.func_returns,
        )
    }

    fn constrain_stmt(&mut self, stmt_id: StmtId, stmt: &crate::ir::core::StmtNode) -> bool {
        constrain_stmt(
            self.program,
            stmt_id,
            stmt,
            self.effect_signatures,
            self.stmt_returns,
            self.prim,
            self.expr_types,
            self.var_types,
            self.func_returns,
        )
    }

    fn unify_expr_with_func_return(&mut self, expr_id: ExprId, func_id: FuncId) -> bool {
        unify_expr_with_func_return(
            self.program,
            expr_id,
            func_id,
            self.expr_types,
            self.var_types,
            self.func_returns,
        )
    }
}

pub fn typecheck_core(program: &CoreProgram, diagnostics: &mut DiagnosticBag) -> SemanticTables {
    let mut store = TypeStore::new();
    let primitives = intern_primitives(&mut store);
    let adt_types = intern_program_adts(program, &mut store);
    let effect_signatures = build_effect_signatures(program, &adt_types, primitives);

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
    {
        let mut checker = TypeCheckerContext {
            program,
            adt_types: &adt_types,
            effect_signatures: &effect_signatures,
            prim: primitives,
            stmt_returns: &stmt_returns,
            expr_types: &mut sema.type_of_expr,
            var_types: &mut var_types,
            func_returns: &mut func_returns,
        };

        let mut changed = true;
        while changed {
            changed = false;

            for (idx, _) in checker.program.exprs().iter().enumerate() {
                let expr_id = ExprId::new(idx);
                changed |= checker.infer_expr_type(expr_id);
            }

            for (idx, stmt) in checker.program.stmts().iter().enumerate() {
                let stmt_id = StmtId::new(idx);
                changed |= checker.constrain_stmt(stmt_id, stmt);
            }

            for (func_idx, function) in checker.program.functions().iter().enumerate() {
                let func_id = FuncId::new(func_idx);
                for return_expr in &checker.stmt_returns[function.body.index()] {
                    changed |= checker.unify_expr_with_func_return(*return_expr, func_id);
                }
            }
        }
    }

    validate_effect_signatures(
        program,
        &effect_signatures,
        &sema.type_of_expr,
        &var_types,
        diagnostics,
    );

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
    effect_signatures: &EffectSignatureTable,
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
        crate::ir::core::StmtKind::Perform {
            result,
            effect,
            operation,
            args,
            ..
        } => {
            let mut changed = false;
            if let Some(signature) = effect_signatures.get(&(*effect, *operation)) {
                for (arg, expected_ty) in args.iter().zip(signature.param_types.iter()) {
                    if let Some(expected_ty) = expected_ty {
                        changed |=
                            set_expr_type(program, *arg, *expected_ty, expr_types, var_types);
                    }
                }
                if let Some(var) = result {
                    if let Some(return_ty) = signature.return_type {
                        changed |= set_var_type(*var, return_ty, var_types);
                    } else {
                        changed |= set_var_type(*var, prim.unit, var_types);
                    }
                }
            } else if let Some(var) = result {
                changed |= set_var_type(*var, prim.unit, var_types);
            }
            changed
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
                    if let Some(signature) =
                        effect_signatures.get(&(handler_def.effect, clause.operation))
                    {
                        for (param, expected_ty) in clause.params.iter().zip(&signature.param_types)
                        {
                            if let Some(expected_ty) = expected_ty {
                                changed |= set_var_type(*param, *expected_ty, var_types);
                            }
                        }
                    }
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

fn build_effect_signatures(
    program: &CoreProgram,
    adt_types: &HashMap<SymbolId, TypeId>,
    prim: PrimitiveTypeIds,
) -> EffectSignatureTable {
    let mut table = EffectSignatureTable::new();
    for effect in program.effects() {
        for operation in &effect.operations {
            table.insert(
                (effect.label, operation.name),
                EffectSignature {
                    param_types: operation
                        .param_types
                        .iter()
                        .copied()
                        .map(|ty| resolve_type_ref(ty, adt_types, prim))
                        .collect(),
                    return_type: resolve_type_ref(operation.return_type, adt_types, prim),
                },
            );
        }
    }
    table
}

fn resolve_type_ref(
    ty: CoreTypeRef,
    adt_types: &HashMap<SymbolId, TypeId>,
    prim: PrimitiveTypeIds,
) -> Option<TypeId> {
    match ty {
        CoreTypeRef::Unit => Some(prim.unit),
        CoreTypeRef::Primitive(primitive) => Some(match primitive {
            PrimitiveTypeRef::Bool => prim.bool_,
            PrimitiveTypeRef::Int => prim.int,
            PrimitiveTypeRef::Float => prim.float,
            PrimitiveTypeRef::Char => prim.char_,
            PrimitiveTypeRef::String => prim.string,
        }),
        CoreTypeRef::Named(name) => adt_types.get(&name).copied(),
        CoreTypeRef::Unknown => None,
    }
}

fn validate_effect_signatures(
    program: &CoreProgram,
    signatures: &EffectSignatureTable,
    expr_types: &[Option<TypeId>],
    var_types: &[Option<TypeId>],
    diagnostics: &mut DiagnosticBag,
) {
    let mut seen_handlers = HashSet::new();

    for stmt in program.stmts() {
        match &stmt.kind {
            crate::ir::core::StmtKind::Perform {
                effect,
                operation,
                args,
                ..
            } => validate_perform_signature(
                signatures,
                *effect,
                *operation,
                args,
                expr_types,
                stmt.span,
                diagnostics,
            ),
            crate::ir::core::StmtKind::Handle { handler, .. } => {
                validate_handler_signature(
                    program,
                    signatures,
                    *handler,
                    var_types,
                    &mut seen_handlers,
                    diagnostics,
                );
            }
            _ => {}
        }
    }
}

fn validate_perform_signature(
    signatures: &EffectSignatureTable,
    effect: EffectLabelId,
    operation: SymbolId,
    args: &[ExprId],
    expr_types: &[Option<TypeId>],
    span: crate::common::span::Span,
    diagnostics: &mut DiagnosticBag,
) {
    let Some(signature) = signatures.get(&(effect, operation)) else {
        diagnostics.error(
            "TYPE_UNKNOWN_EFFECT_OP",
            "Unknown effect operation in Core perform statement",
            span,
        );
        return;
    };

    if signature.param_types.len() != args.len() {
        diagnostics.error(
            "TYPE_BAD_EFFECT_OP_ARITY",
            format!(
                "Effect operation arity mismatch: expected {}, got {}",
                signature.param_types.len(),
                args.len()
            ),
            span,
        );
        return;
    }

    for (idx, (arg, expected_ty)) in args.iter().zip(signature.param_types.iter()).enumerate() {
        let Some(expected_ty) = expected_ty else {
            continue;
        };
        let Some(actual_ty) = expr_types.get(arg.index()).copied().flatten() else {
            continue;
        };
        if actual_ty != *expected_ty {
            diagnostics.error(
                "TYPE_EFFECT_ARG_MISMATCH",
                format!(
                    "Effect argument #{} type mismatch: expected t{}, got t{}",
                    idx + 1,
                    expected_ty.as_u32(),
                    actual_ty.as_u32()
                ),
                span,
            );
        }
    }
}

fn validate_handler_signature(
    program: &CoreProgram,
    signatures: &EffectSignatureTable,
    handler: HandlerId,
    var_types: &[Option<TypeId>],
    seen_handlers: &mut HashSet<HandlerId>,
    diagnostics: &mut DiagnosticBag,
) {
    if !seen_handlers.insert(handler) {
        return;
    }
    let Some(handler_def) = program.handlers().get(handler.index()) else {
        return;
    };

    for clause in &handler_def.clauses {
        let Some(signature) = signatures.get(&(handler_def.effect, clause.operation)) else {
            diagnostics.error(
                "TYPE_UNKNOWN_HANDLER_OP",
                "Unknown effect operation in handler clause",
                clause.span,
            );
            continue;
        };

        if signature.param_types.len() != clause.params.len() {
            diagnostics.error(
                "TYPE_BAD_HANDLER_CLAUSE_ARITY",
                format!(
                    "Handler clause arity mismatch: expected {}, got {}",
                    signature.param_types.len(),
                    clause.params.len()
                ),
                clause.span,
            );
            continue;
        }

        for (idx, (param, expected_ty)) in clause
            .params
            .iter()
            .zip(signature.param_types.iter())
            .enumerate()
        {
            let Some(expected_ty) = expected_ty else {
                continue;
            };
            let Some(actual_ty) = var_types.get(param.index()).copied().flatten() else {
                continue;
            };
            if actual_ty != *expected_ty {
                diagnostics.error(
                    "TYPE_HANDLER_PARAM_MISMATCH",
                    format!(
                        "Handler clause param #{} type mismatch: expected t{}, got t{}",
                        idx + 1,
                        expected_ty.as_u32(),
                        actual_ty.as_u32()
                    ),
                    clause.span,
                );
            }
        }
    }
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
