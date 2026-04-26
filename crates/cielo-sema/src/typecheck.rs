// Pass 2/9: typecheck_core (HM-style type inference + stmt-effect table population)
//
// Inputs:
// - CoreProgram produced by lowering
//
// Outputs:
// - SemanticTables: expr types, expr effects, stmt effects, persistability
// - Diagnostics for type mismatches, arity mismatches, and unknown operations
//
// Invariants:
// - Expr effects are always empty (Expr/Stmt split)
// - Stmt effects conservatively approximate dynamic effect flow
// - Handle nodes discharge their handled effect label from body summaries
//
// Diagnostics:
// - `TYPE_*` errors for mismatches/unknowns
//
// Complexity:
// - Type inference: one recursive walk per function body + unification
// - Effect inference: memoized DFS over stmt graph (linear in stmt count)

use std::collections::{HashMap, HashSet};

use crate::facts::SemanticTables;
use crate::ownership::{classify_core_type_ref, classify_type_kind};
use crate::ty::{EnumVariant, PrimitiveType, StructField, TypeKind, TypeStore};
use cielo_base::Span;
use cielo_base::densemap::DenseMap;
use cielo_base::diagnostics::DiagnosticBag;
use cielo_base::{EffectLabelId, ExprId, FuncId, HandlerId, StmtId, SymbolId, TypeId, VarId};
use cielo_ir::core::{
    CoreProgram, CoreTypeRef, ExprKind, Literal, OpCategory, PrimitiveTypeRef, StmtKind, UnaryOp,
};
use cielo_ir::effect::SortedEffectRow;
use cielo_ir::ownership::OwnershipClass;

macro_rules! define_primitive_type_ids {
    ($($field:ident => $primitive:ident),* $(,)?) => {
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub struct PrimitiveTypeIds {
            $(pub $field: TypeId,)*
        }

        fn intern_primitives(store: &mut TypeStore) -> PrimitiveTypeIds {
            PrimitiveTypeIds {
                $($field: store.intern(TypeKind::Primitive(PrimitiveType::$primitive)),)*
            }
        }
    };
}

define_primitive_type_ids! {
    unit => Unit,
    bool_ => Bool,
    int => Int,
    float => Float,
    char_ => Char,
    string => String,
}

#[derive(Clone, Debug)]
struct EffectSignature {
    param_types: Vec<Option<TypeId>>,
    return_type: Option<TypeId>,
}

type EffectSignatureTable = HashMap<(EffectLabelId, SymbolId), EffectSignature>;

#[derive(Clone, Debug)]
struct StructCtorSig {
    result: TypeId,
    fields: Vec<TypeId>,
}

#[derive(Clone, Debug)]
struct EnumCtorSig {
    result: TypeId,
    fields: Vec<TypeId>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TypeTemplate {
    Concrete(TypeId),
    Generic(u16),
}

#[derive(Clone, Debug)]
struct FunctionTemplate {
    params: Vec<TypeTemplate>,
    ret: Option<TypeTemplate>,
    generic_count: usize,
    inferred_ret_var: Option<InferVarId>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct InferVarId(u32);

impl InferVarId {
    fn index(self) -> usize {
        self.0 as usize
    }

    fn from_index(index: usize) -> Self {
        debug_assert!(index <= u32::MAX as usize);
        Self(index as u32)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum InferTy {
    Concrete(TypeId),
    Var(InferVarId),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Scheme {
    Concrete(TypeId),
    MonoVar(InferVarId),
    Generic,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct TypeMismatch {
    left: TypeId,
    right: TypeId,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ResumeExpectation {
    arg: InferTy,
    result: InferTy,
}

type Env = HashMap<VarId, Scheme>;
type ResumeCtx = HashMap<VarId, ResumeExpectation>;

#[derive(Clone, Debug, Default)]
struct InferState {
    parent: Vec<InferVarId>,
    rank: Vec<u8>,
    binding: Vec<Option<TypeId>>,
}

impl InferState {
    fn fresh_var(&mut self) -> InferVarId {
        let id = InferVarId::from_index(self.parent.len());
        self.parent.push(id);
        self.rank.push(0);
        self.binding.push(None);
        id
    }

    fn fresh_ty(&mut self) -> InferTy {
        InferTy::Var(self.fresh_var())
    }

    fn find(&mut self, var: InferVarId) -> InferVarId {
        let idx = var.index();
        let parent = self.parent[idx];
        if parent == var {
            return var;
        }
        let root = self.find(parent);
        self.parent[idx] = root;
        root
    }

    fn resolve(&mut self, ty: InferTy) -> InferTy {
        match ty {
            InferTy::Concrete(ty) => InferTy::Concrete(ty),
            InferTy::Var(var) => {
                let root = self.find(var);
                if let Some(bound) = self.binding[root.index()] {
                    InferTy::Concrete(bound)
                } else {
                    InferTy::Var(root)
                }
            }
        }
    }

    fn resolve_concrete(&mut self, ty: InferTy) -> Option<TypeId> {
        match self.resolve(ty) {
            InferTy::Concrete(ty) => Some(ty),
            InferTy::Var(_) => None,
        }
    }

    fn unify(&mut self, left: InferTy, right: InferTy) -> Result<InferTy, TypeMismatch> {
        let left = self.resolve(left);
        let right = self.resolve(right);
        match (left, right) {
            (InferTy::Concrete(lhs), InferTy::Concrete(rhs)) => {
                if lhs == rhs {
                    Ok(InferTy::Concrete(lhs))
                } else {
                    Err(TypeMismatch {
                        left: lhs,
                        right: rhs,
                    })
                }
            }
            (InferTy::Var(var), InferTy::Concrete(ty))
            | (InferTy::Concrete(ty), InferTy::Var(var)) => {
                self.bind_var(var, ty)?;
                Ok(InferTy::Concrete(ty))
            }
            (InferTy::Var(lhs), InferTy::Var(rhs)) => self.union_vars(lhs, rhs),
        }
    }

    fn bind_var(&mut self, var: InferVarId, ty: TypeId) -> Result<(), TypeMismatch> {
        let root = self.find(var);
        let idx = root.index();
        match self.binding[idx] {
            Some(existing) if existing == ty => Ok(()),
            Some(existing) => Err(TypeMismatch {
                left: existing,
                right: ty,
            }),
            None => {
                self.binding[idx] = Some(ty);
                Ok(())
            }
        }
    }

    fn union_vars(&mut self, left: InferVarId, right: InferVarId) -> Result<InferTy, TypeMismatch> {
        let left_root = self.find(left);
        let right_root = self.find(right);
        if left_root == right_root {
            return Ok(self.resolve(InferTy::Var(left_root)));
        }

        let left_idx = left_root.index();
        let right_idx = right_root.index();
        let left_binding = self.binding[left_idx];
        let right_binding = self.binding[right_idx];

        if let (Some(lhs), Some(rhs)) = (left_binding, right_binding)
            && lhs != rhs
        {
            return Err(TypeMismatch {
                left: lhs,
                right: rhs,
            });
        }

        let (root, child) = match self.rank[left_idx].cmp(&self.rank[right_idx]) {
            std::cmp::Ordering::Less => (right_root, left_root),
            std::cmp::Ordering::Greater => (left_root, right_root),
            std::cmp::Ordering::Equal => {
                self.rank[left_idx] = self.rank[left_idx].saturating_add(1);
                (left_root, right_root)
            }
        };

        let root_idx = root.index();
        let child_idx = child.index();
        self.parent[child_idx] = root;

        let merged_binding = self.binding[root_idx].or(self.binding[child_idx]);
        self.binding[root_idx] = merged_binding;

        Ok(match merged_binding {
            Some(ty) => InferTy::Concrete(ty),
            None => InferTy::Var(root),
        })
    }
}

struct TypeChecker<'a> {
    program: &'a CoreProgram,
    diagnostics: &'a mut DiagnosticBag,
    store: TypeStore,
    prim: PrimitiveTypeIds,
    error_type: TypeId,
    struct_ctors: HashMap<SymbolId, StructCtorSig>,
    enum_ctors: HashMap<SymbolId, EnumCtorSig>,
    effect_signatures: EffectSignatureTable,
    function_templates: Vec<FunctionTemplate>,
    infer: InferState,
    expr_tys: Vec<Option<InferTy>>,
    unresolved_type_params: HashMap<InferVarId, TypeId>,
}

impl<'a> TypeChecker<'a> {
    fn new(program: &'a CoreProgram, diagnostics: &'a mut DiagnosticBag) -> Self {
        let mut store = TypeStore::new();
        let prim = intern_primitives(&mut store);
        let error_type = store.intern(TypeKind::Error);
        let adt_types = intern_program_adts(program, &mut store, prim, error_type, diagnostics);
        let (struct_ctors, enum_ctors) = build_ctor_signatures(program, &store, &adt_types);
        let effect_signatures = build_effect_signatures(program, &adt_types, prim);
        let function_templates =
            build_function_templates(program, &adt_types, prim, error_type, diagnostics);

        Self {
            program,
            diagnostics,
            store,
            prim,
            error_type,
            struct_ctors,
            enum_ctors,
            effect_signatures,
            function_templates,
            infer: InferState::default(),
            expr_tys: vec![None; program.exprs().len()],
            unresolved_type_params: HashMap::new(),
        }
    }

    fn run(mut self, conformance: EffectConformance) -> SemanticTables {
        for template in &mut self.function_templates {
            if template.ret.is_none() {
                template.inferred_ret_var = Some(self.infer.fresh_var());
            }
        }

        for (idx, _) in self.program.functions().iter().enumerate() {
            self.infer_function(FuncId::new(idx));
        }

        let mut sema =
            SemanticTables::with_counts(self.program.exprs().len(), self.program.stmts().len());
        sema.effects_of_expr = vec![SortedEffectRow::empty(); self.program.exprs().len()];
        sema.effect_properties = self
            .program
            .effects()
            .iter()
            .map(|effect| (effect.label, effect.properties))
            .collect();

        for idx in 0..self.program.exprs().len() {
            let expr_id = ExprId::new(idx);
            let inferred = self
                .expr_tys
                .get(idx)
                .copied()
                .flatten()
                .unwrap_or(InferTy::Concrete(self.error_type));
            let concrete = self.materialize_ty(inferred);
            sema.type_of_expr[expr_id.index()] = Some(concrete);
        }

        sema.persistability_of_type = self
            .store
            .kinds()
            .iter()
            .enumerate()
            .map(|(idx, _)| self.store.persistability(TypeId::new(idx)))
            .collect();

        sema.ownership_of_type = self.store.kinds().iter().map(classify_type_kind).collect();
        sema.ownership_of_expr = sema
            .type_of_expr
            .iter()
            .map(|slot| {
                slot.and_then(|ty| sema.ownership_of_type.get(ty.index()).copied())
                    .unwrap_or(OwnershipClass::BorrowedView)
            })
            .collect();
        sema.ownership_of_var = self.classify_var_ownership(&sema);

        infer_stmt_effects(self.program, &mut sema.effects_of_stmt);
        if conformance == EffectConformance::Check {
            self.enforce_declared_effects(&sema);
        }
        sema
    }

    /// An effect missing from the `with` row leaves the row empty, and
    /// `normalize` treats a perform with an empty row as dead code. Without
    /// this error the perform is silently deleted along with its output.
    fn enforce_declared_effects(&mut self, sema: &SemanticTables) {
        let functions = self.program.functions();
        for (idx, function) in functions.iter().enumerate() {
            let Some(inferred) = sema.effects_of_stmt.get(function.body.index()) else {
                continue;
            };
            let undeclared = inferred.subtract(&function.declared_effects);
            if undeclared.is_empty() {
                continue;
            }
            let labels = undeclared
                .iter()
                .map(|effect| format!("e{}", effect.as_u32()))
                .collect::<Vec<_>>()
                .join(", ");
            self.diagnostics.error(
                "SEMA_UNDECLARED_EFFECT",
                format!(
                    "function f{idx} performs undeclared effect(s) {labels}; add them to its `with` row"
                ),
                function.span,
            );
        }
    }

    fn classify_var_ownership(&self, sema: &SemanticTables) -> DenseMap<VarId, OwnershipClass> {
        let mut out = DenseMap::default();

        for function in self.program.functions() {
            for (idx, param) in function.params.iter().copied().enumerate() {
                let ownership = function
                    .param_types
                    .get(idx)
                    .map(classify_core_type_ref)
                    .unwrap_or(OwnershipClass::BorrowedView);
                assign_var_ownership(&mut out, param, ownership);
            }
        }

        for handler in self.program.handlers() {
            for clause in &handler.clauses {
                for param in clause.params.iter().copied() {
                    assign_var_ownership(&mut out, param, OwnershipClass::BorrowedView);
                }
                if let Some(resume) = clause.resume_param {
                    assign_var_ownership(&mut out, resume, OwnershipClass::BorrowedView);
                }
            }
        }

        for stmt in self.program.stmts() {
            match &stmt.kind {
                StmtKind::Let { binding, value, .. } => {
                    let ownership = sema
                        .ownership_of_expr
                        .get(value.index())
                        .copied()
                        .unwrap_or(OwnershipClass::BorrowedView);
                    assign_var_ownership(&mut out, *binding, ownership);
                }
                StmtKind::Val { binding, .. } => {
                    assign_var_ownership(&mut out, *binding, OwnershipClass::BorrowedView);
                }
                StmtKind::Call { result, callee, .. } => {
                    let ownership = self
                        .program
                        .function(*callee)
                        .map(|function| classify_core_type_ref(&function.return_type))
                        .unwrap_or(OwnershipClass::BorrowedView);
                    assign_var_ownership(&mut out, *result, ownership);
                }
                StmtKind::Perform {
                    result: Some(result),
                    effect,
                    operation,
                    ..
                } => {
                    let ownership = self
                        .program
                        .effect(*effect)
                        .and_then(|decl| decl.operations.iter().find(|op| op.name == *operation))
                        .map(|op| classify_core_type_ref(&op.return_type))
                        .unwrap_or(OwnershipClass::BorrowedView);
                    assign_var_ownership(&mut out, *result, ownership);
                }
                StmtKind::Perform { result: None, .. } => {}
                StmtKind::Resume { result, .. } => {
                    assign_var_ownership(&mut out, *result, OwnershipClass::BorrowedView);
                }
                StmtKind::Match { arms, .. } => {
                    for arm in arms {
                        for binder in arm.binders.iter().copied() {
                            assign_var_ownership(&mut out, binder, OwnershipClass::BorrowedView);
                        }
                    }
                }
                StmtKind::Return(_)
                | StmtKind::If { .. }
                | StmtKind::Handle { .. }
                | StmtKind::Stage { .. }
                | StmtKind::Hole { .. }
                | StmtKind::Error(_) => {}
            }
        }

        out
    }

    fn infer_function(&mut self, func_id: FuncId) {
        let Some(function) = self.program.function(func_id) else {
            return;
        };
        let Some(template) = self.function_templates.get(func_id.index()).cloned() else {
            return;
        };

        let mut generic_inst = Vec::with_capacity(template.generic_count);
        for _ in 0..template.generic_count {
            generic_inst.push(self.infer.fresh_ty());
        }

        let mut env = Env::new();
        for (idx, param_var) in function.params.iter().copied().enumerate() {
            let param_ty = template
                .params
                .get(idx)
                .map(|tpl| self.instantiate_template(*tpl, &generic_inst))
                .unwrap_or(InferTy::Concrete(self.error_type));
            env.insert(param_var, self.mono_scheme(param_ty));
        }

        let expected_return = template
            .ret
            .map(|tpl| self.instantiate_template(tpl, &generic_inst))
            .or_else(|| template.inferred_ret_var.map(InferTy::Var))
            .unwrap_or(InferTy::Concrete(self.error_type));

        let mut resume_ctx = ResumeCtx::new();
        let body_ty = self.infer_stmt(function.body, &mut env, &mut resume_ctx);
        let _ = self.unify_with(
            body_ty,
            expected_return,
            function.span,
            "TYPE_RETURN_MISMATCH",
            "Function body type does not match return type",
        );
    }

    fn infer_stmt(
        &mut self,
        stmt_id: StmtId,
        env: &mut Env,
        resume_ctx: &mut ResumeCtx,
    ) -> InferTy {
        let Some(stmt) = self.program.stmt(stmt_id) else {
            return InferTy::Concrete(self.error_type);
        };

        match &stmt.kind {
            StmtKind::Return(expr) => self.infer_expr(*expr, env),
            StmtKind::Let {
                binding,
                value,
                next,
            } => {
                let value_ty = self.infer_expr(*value, env);
                let scheme = self.generalize_let(value_ty, env);
                env.insert(*binding, scheme);
                self.infer_stmt(*next, env, resume_ctx)
            }
            StmtKind::Val {
                binding,
                value,
                next,
            } => {
                let mut value_env = env.clone();
                let mut value_resume = resume_ctx.clone();
                let value_ty = self.infer_stmt(*value, &mut value_env, &mut value_resume);
                env.insert(*binding, self.mono_scheme(value_ty));
                self.infer_stmt(*next, env, resume_ctx)
            }
            StmtKind::Call {
                result,
                callee,
                args,
                next,
                ..
            } => {
                let ret_ty = self.infer_call(*callee, args, env, stmt.span);
                env.insert(*result, self.mono_scheme(ret_ty));
                self.infer_stmt(*next, env, resume_ctx)
            }
            StmtKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
                let cond_ty = self.infer_expr(*cond, env);
                let _ = self.unify_with(
                    cond_ty,
                    InferTy::Concrete(self.prim.bool_),
                    stmt.span,
                    "TYPE_IF_COND",
                    "`if` condition must have type Bool",
                );

                let mut then_env = env.clone();
                let mut then_resume = resume_ctx.clone();
                let then_ty = self.infer_stmt(*then_branch, &mut then_env, &mut then_resume);

                let mut else_env = env.clone();
                let mut else_resume = resume_ctx.clone();
                let else_ty = self.infer_stmt(*else_branch, &mut else_env, &mut else_resume);

                self.unify_with(
                    then_ty,
                    else_ty,
                    stmt.span,
                    "TYPE_IF_BRANCH_MISMATCH",
                    "`if` branches must produce the same type",
                )
            }
            StmtKind::Match {
                scrutinee,
                arms,
                default,
            } => self.infer_match_stmt(*scrutinee, arms, *default, env, resume_ctx, stmt.span),
            StmtKind::Perform {
                result,
                effect,
                operation,
                args,
                next,
            } => {
                let sig = self.effect_signatures.get(&(*effect, *operation)).cloned();
                if let Some(sig) = sig {
                    if sig.param_types.len() != args.len() {
                        self.diagnostics.error(
                            "TYPE_BAD_EFFECT_OP_ARITY",
                            format!(
                                "Effect operation arity mismatch: expected {}, got {}",
                                sig.param_types.len(),
                                args.len()
                            ),
                            stmt.span,
                        );
                    }
                    for (idx, (arg_id, expected)) in
                        args.iter().zip(sig.param_types.iter()).enumerate()
                    {
                        if let Some(expected_ty) = expected {
                            let arg_ty = self.infer_expr(*arg_id, env);
                            if let Some(actual) = self.infer.resolve_concrete(arg_ty)
                                && actual != *expected_ty
                            {
                                self.diagnostics.error(
                                    "TYPE_EFFECT_ARG_MISMATCH",
                                    format!(
                                        "Effect argument #{} type mismatch: expected {}, got {}",
                                        idx + 1,
                                        self.type_name(*expected_ty),
                                        self.type_name(actual)
                                    ),
                                    stmt.span,
                                );
                            }
                            let _ = self.unify_with(
                                arg_ty,
                                InferTy::Concrete(*expected_ty),
                                stmt.span,
                                "TYPE_EFFECT_ARG_MISMATCH",
                                "Effect argument type mismatch",
                            );
                        }
                    }

                    if let Some(result_var) = result {
                        let ret_ty = sig.return_type.unwrap_or(self.prim.unit);
                        env.insert(*result_var, Scheme::Concrete(ret_ty));
                    }
                } else {
                    self.diagnostics.error(
                        "TYPE_UNKNOWN_EFFECT_OP",
                        "Unknown effect operation in Core perform statement",
                        stmt.span,
                    );
                    if let Some(result_var) = result {
                        env.insert(*result_var, Scheme::Concrete(self.prim.unit));
                    }
                    for arg in args {
                        let _ = self.infer_expr(*arg, env);
                    }
                }

                self.infer_stmt(*next, env, resume_ctx)
            }
            StmtKind::Resume {
                result,
                resume,
                arg,
                next,
            } => {
                if let Some(expect) = resume_ctx.get(resume).copied() {
                    let arg_ty = self.infer_expr(*arg, env);
                    let _ = self.unify_with(
                        arg_ty,
                        expect.arg,
                        stmt.span,
                        "TYPE_RESUME_ARG_MISMATCH",
                        "`resume` argument type mismatch",
                    );
                    env.insert(*result, self.mono_scheme(expect.result));
                } else {
                    self.diagnostics.error(
                        "TYPE_RESUME_OUTSIDE_HANDLER",
                        "`resume` used outside active handler clause context",
                        stmt.span,
                    );
                    env.insert(*result, Scheme::Concrete(self.error_type));
                }
                self.infer_stmt(*next, env, resume_ctx)
            }
            StmtKind::Handle {
                handler,
                body,
                next,
            } => self.infer_handle_stmt(*handler, *body, *next, env, resume_ctx, stmt.span),
            StmtKind::Stage { body, next, .. } => {
                let mut body_env = env.clone();
                let mut body_resume = resume_ctx.clone();
                let body_ty = self.infer_stmt(*body, &mut body_env, &mut body_resume);
                if let Some(next_stmt) = next {
                    self.infer_stmt(*next_stmt, env, resume_ctx)
                } else {
                    body_ty
                }
            }
            StmtKind::Hole { ty } => InferTy::Concrete(*ty),
            StmtKind::Error(_) => InferTy::Concrete(self.error_type),
        }
    }

    fn infer_match_stmt(
        &mut self,
        scrutinee: ExprId,
        arms: &[cielo_ir::core::MatchArm],
        default: Option<StmtId>,
        env: &mut Env,
        resume_ctx: &mut ResumeCtx,
        span: Span,
    ) -> InferTy {
        let scrutinee_ty = self.infer_expr(scrutinee, env);
        let mut result_ty: Option<InferTy> = None;

        for arm in arms {
            let mut arm_env = env.clone();
            let mut arm_resume = resume_ctx.clone();

            if let Some(variant_sig) = self.enum_ctors.get(&arm.tag).cloned() {
                let _ = self.unify_with(
                    scrutinee_ty,
                    InferTy::Concrete(variant_sig.result),
                    arm.span,
                    "TYPE_MATCH_SCRUTINEE_MISMATCH",
                    "Match arm variant does not match scrutinee type",
                );

                if arm.binders.len() != variant_sig.fields.len() {
                    self.diagnostics.error(
                        "TYPE_MATCH_ARM_ARITY",
                        format!(
                            "Match arm binder count mismatch: expected {}, got {}",
                            variant_sig.fields.len(),
                            arm.binders.len()
                        ),
                        arm.span,
                    );
                }

                for (binder, field_ty) in arm.binders.iter().zip(variant_sig.fields.iter()) {
                    arm_env.insert(*binder, Scheme::Concrete(*field_ty));
                }
            } else {
                self.diagnostics.error(
                    "TYPE_UNKNOWN_MATCH_VARIANT",
                    "Unknown enum variant in match arm",
                    arm.span,
                );
            }

            let arm_ty = self.infer_stmt(arm.body, &mut arm_env, &mut arm_resume);
            result_ty = Some(match result_ty {
                Some(existing) => self.unify_with(
                    existing,
                    arm_ty,
                    arm.span,
                    "TYPE_MATCH_BRANCH_MISMATCH",
                    "All match arms must produce the same type",
                ),
                None => arm_ty,
            });
        }

        if let Some(default_stmt) = default {
            let mut default_env = env.clone();
            let mut default_resume = resume_ctx.clone();
            let default_ty = self.infer_stmt(default_stmt, &mut default_env, &mut default_resume);
            result_ty = Some(match result_ty {
                Some(existing) => self.unify_with(
                    existing,
                    default_ty,
                    span,
                    "TYPE_MATCH_BRANCH_MISMATCH",
                    "Default match branch type mismatch",
                ),
                None => default_ty,
            });
        } else {
            self.check_match_exhaustive(scrutinee_ty, arms, span);
        }

        result_ty.unwrap_or(InferTy::Concrete(self.prim.unit))
    }

    /// Without a default arm, an uncovered variant falls through to a
    /// synthesised unit block at runtime, so a missing case is a wrong answer
    /// rather than a crash.
    fn check_match_exhaustive(
        &mut self,
        scrutinee_ty: InferTy,
        arms: &[cielo_ir::core::MatchArm],
        span: Span,
    ) {
        let scrutinee = self.materialize_ty(scrutinee_ty);
        let Some(TypeKind::Enum { variants, .. }) =
            self.store.kinds().get(scrutinee.index()).cloned()
        else {
            return;
        };

        let mut covered = HashSet::new();
        for arm in arms {
            if !covered.insert(arm.tag) {
                self.diagnostics.error(
                    "TYPE_MATCH_DUPLICATE_ARM",
                    "Match arm repeats a variant already covered",
                    arm.span,
                );
            }
        }

        let missing = variants
            .iter()
            .filter(|variant| !covered.contains(&variant.name))
            .count();
        if missing > 0 {
            self.diagnostics.error(
                "TYPE_MATCH_NOT_EXHAUSTIVE",
                format!(
                    "Match does not cover {missing} of {} variants and has no default arm",
                    variants.len()
                ),
                span,
            );
        }
    }

    fn infer_handle_stmt(
        &mut self,
        handler: HandlerId,
        body: StmtId,
        next: Option<StmtId>,
        env: &mut Env,
        resume_ctx: &mut ResumeCtx,
        span: Span,
    ) -> InferTy {
        let Some(handler_def) = self.program.handlers().get(handler.index()) else {
            self.diagnostics.error(
                "TYPE_UNKNOWN_HANDLER",
                "Unknown handler id in Handle node",
                span,
            );
            if let Some(next_stmt) = next {
                return self.infer_stmt(next_stmt, env, resume_ctx);
            }
            return InferTy::Concrete(self.error_type);
        };

        let handler_result_ty = self.infer.fresh_ty();

        let mut body_env = env.clone();
        let mut body_resume = resume_ctx.clone();
        let body_ty = self.infer_stmt(body, &mut body_env, &mut body_resume);
        let _ = self.unify_with(
            body_ty,
            handler_result_ty,
            handler_def.span,
            "TYPE_HANDLER_BODY_MISMATCH",
            "Handled body type must match handler return type",
        );

        let mut return_env = env.clone();
        return_env.insert(
            handler_def.return_param,
            self.mono_scheme(handler_result_ty),
        );
        let mut return_resume = resume_ctx.clone();
        let return_ty =
            self.infer_stmt(handler_def.return_body, &mut return_env, &mut return_resume);
        let _ = self.unify_with(
            return_ty,
            handler_result_ty,
            handler_def.span,
            "TYPE_HANDLER_RETURN_CLAUSE",
            "Handler return clause type mismatch",
        );

        for clause in &handler_def.clauses {
            let Some(sig) = self
                .effect_signatures
                .get(&(handler_def.effect, clause.operation))
                .cloned()
            else {
                self.diagnostics.error(
                    "TYPE_UNKNOWN_HANDLER_OP",
                    "Unknown effect operation in handler clause",
                    clause.span,
                );
                continue;
            };

            if sig.param_types.len() != clause.params.len() {
                self.diagnostics.error(
                    "TYPE_BAD_HANDLER_CLAUSE_ARITY",
                    format!(
                        "Handler clause arity mismatch: expected {}, got {}",
                        sig.param_types.len(),
                        clause.params.len()
                    ),
                    clause.span,
                );
            }

            let mut clause_env = env.clone();
            let mut clause_resume = resume_ctx.clone();

            for (param, expected_ty) in clause.params.iter().zip(sig.param_types.iter()) {
                let ty = (*expected_ty).unwrap_or(self.error_type);
                clause_env.insert(*param, Scheme::Concrete(ty));
            }

            if let Some(resume_var) = clause.resume_param {
                let arg_ty = InferTy::Concrete(sig.return_type.unwrap_or(self.prim.unit));
                clause_resume.insert(
                    resume_var,
                    ResumeExpectation {
                        arg: arg_ty,
                        result: handler_result_ty,
                    },
                );
            }

            let clause_ty = self.infer_stmt(clause.body, &mut clause_env, &mut clause_resume);
            let _ = self.unify_with(
                clause_ty,
                handler_result_ty,
                clause.span,
                "TYPE_HANDLER_CLAUSE_MISMATCH",
                "Handler clause return type mismatch",
            );
        }

        if let Some(next_stmt) = next {
            self.infer_stmt(next_stmt, env, resume_ctx)
        } else {
            handler_result_ty
        }
    }

    fn infer_call(&mut self, callee: FuncId, args: &[ExprId], env: &Env, span: Span) -> InferTy {
        let Some(template) = self.function_templates.get(callee.index()).cloned() else {
            self.diagnostics.error(
                "TYPE_UNKNOWN_CALLEE",
                "Unknown function id at call site",
                span,
            );
            for arg in args {
                let _ = self.infer_expr(*arg, env);
            }
            return InferTy::Concrete(self.error_type);
        };

        if template.params.len() != args.len() {
            self.diagnostics.error(
                "TYPE_BAD_CALL_ARITY",
                format!(
                    "Call arity mismatch: expected {}, got {}",
                    template.params.len(),
                    args.len()
                ),
                span,
            );
        }

        let mut generic_inst = Vec::with_capacity(template.generic_count);
        for _ in 0..template.generic_count {
            generic_inst.push(self.infer.fresh_ty());
        }

        for (arg_expr, expected_tpl) in args.iter().zip(template.params.iter()) {
            let arg_ty = self.infer_expr(*arg_expr, env);
            let expected_ty = self.instantiate_template(*expected_tpl, &generic_inst);
            let _ = self.unify_with(
                arg_ty,
                expected_ty,
                span,
                "TYPE_CALL_ARG_MISMATCH",
                "Call argument type mismatch",
            );
        }

        template
            .ret
            .map(|ret| self.instantiate_template(ret, &generic_inst))
            .or_else(|| template.inferred_ret_var.map(InferTy::Var))
            .unwrap_or(InferTy::Concrete(self.error_type))
    }

    fn infer_expr(&mut self, expr_id: ExprId, env: &Env) -> InferTy {
        let Some(expr) = self.program.expr(expr_id) else {
            return InferTy::Concrete(self.error_type);
        };

        let inferred = match &expr.kind {
            ExprKind::Literal(lit) => InferTy::Concrete(type_for_literal(lit, self.prim)),
            ExprKind::Var(var) => env
                .get(var)
                .copied()
                .map(|scheme| self.instantiate_scheme(scheme))
                .unwrap_or_else(|| {
                    self.diagnostics.error(
                        "TYPE_UNKNOWN_VAR",
                        "Unknown variable id in expression",
                        expr.span,
                    );
                    InferTy::Concrete(self.error_type)
                }),
            ExprKind::Unary { op, expr: inner } => {
                let inner_ty = self.infer_expr(*inner, env);
                match op {
                    UnaryOp::Neg => {
                        let numeric_ty = self.pick_numeric_type(inner_ty, None, expr.span);
                        let _ = self.unify_with(
                            inner_ty,
                            numeric_ty,
                            expr.span,
                            "TYPE_NUMERIC_REQUIRED",
                            "Unary negation requires Int or Float",
                        );
                        numeric_ty
                    }
                    UnaryOp::Not => {
                        let _ = self.unify_with(
                            inner_ty,
                            InferTy::Concrete(self.prim.bool_),
                            expr.span,
                            "TYPE_BOOL_REQUIRED",
                            "Logical negation requires Bool",
                        );
                        InferTy::Concrete(self.prim.bool_)
                    }
                }
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let lhs_ty = self.infer_expr(*lhs, env);
                let rhs_ty = self.infer_expr(*rhs, env);
                match op.category() {
                    OpCategory::Arithmetic => {
                        let numeric_ty = self.pick_numeric_type(lhs_ty, Some(rhs_ty), expr.span);
                        let _ = self.unify_with(
                            lhs_ty,
                            numeric_ty,
                            expr.span,
                            "TYPE_NUMERIC_REQUIRED",
                            "Arithmetic operands must be Int or Float",
                        );
                        let _ = self.unify_with(
                            rhs_ty,
                            numeric_ty,
                            expr.span,
                            "TYPE_NUMERIC_REQUIRED",
                            "Arithmetic operands must be Int or Float",
                        );
                        numeric_ty
                    }
                    OpCategory::Comparison => {
                        let numeric_ty = self.pick_numeric_type(lhs_ty, Some(rhs_ty), expr.span);
                        let _ = self.unify_with(
                            lhs_ty,
                            numeric_ty,
                            expr.span,
                            "TYPE_NUMERIC_REQUIRED",
                            "Comparison operands must be Int or Float",
                        );
                        let _ = self.unify_with(
                            rhs_ty,
                            numeric_ty,
                            expr.span,
                            "TYPE_NUMERIC_REQUIRED",
                            "Comparison operands must be Int or Float",
                        );
                        InferTy::Concrete(self.prim.bool_)
                    }
                    OpCategory::Equality => {
                        let _ = self.unify_with(
                            lhs_ty,
                            rhs_ty,
                            expr.span,
                            "TYPE_EQUALITY_MISMATCH",
                            "Equality operands must have the same type",
                        );
                        InferTy::Concrete(self.prim.bool_)
                    }
                    OpCategory::Logical => {
                        let _ = self.unify_with(
                            lhs_ty,
                            InferTy::Concrete(self.prim.bool_),
                            expr.span,
                            "TYPE_BOOL_REQUIRED",
                            "Logical operands must be Bool",
                        );
                        let _ = self.unify_with(
                            rhs_ty,
                            InferTy::Concrete(self.prim.bool_),
                            expr.span,
                            "TYPE_BOOL_REQUIRED",
                            "Logical operands must be Bool",
                        );
                        InferTy::Concrete(self.prim.bool_)
                    }
                }
            }
            ExprKind::PureCall { callee, args } => self.infer_call(*callee, args, env, expr.span),
            ExprKind::MakeStruct { ty, fields } => {
                let sig = self.struct_ctors.get(ty).cloned();
                if let Some(sig) = sig {
                    if sig.fields.len() != fields.len() {
                        self.diagnostics.error(
                            "TYPE_BAD_STRUCT_CTOR_ARITY",
                            format!(
                                "Struct constructor arity mismatch: expected {}, got {}",
                                sig.fields.len(),
                                fields.len()
                            ),
                            expr.span,
                        );
                    }
                    for (field_expr, expected_ty) in fields.iter().zip(sig.fields.iter()) {
                        let field_ty = self.infer_expr(*field_expr, env);
                        let _ = self.unify_with(
                            field_ty,
                            InferTy::Concrete(*expected_ty),
                            expr.span,
                            "TYPE_STRUCT_FIELD_MISMATCH",
                            "Struct field type mismatch",
                        );
                    }
                    InferTy::Concrete(sig.result)
                } else {
                    self.diagnostics.error(
                        "TYPE_UNKNOWN_STRUCT_CTOR",
                        "Unknown struct constructor",
                        expr.span,
                    );
                    for field in fields {
                        let _ = self.infer_expr(*field, env);
                    }
                    InferTy::Concrete(self.error_type)
                }
            }
            ExprKind::MakeEnum {
                ty: _,
                variant,
                fields,
            } => {
                let sig = self.enum_ctors.get(variant).cloned();
                if let Some(sig) = sig {
                    if sig.fields.len() != fields.len() {
                        self.diagnostics.error(
                            "TYPE_BAD_ENUM_CTOR_ARITY",
                            format!(
                                "Enum constructor arity mismatch: expected {}, got {}",
                                sig.fields.len(),
                                fields.len()
                            ),
                            expr.span,
                        );
                    }
                    for (field_expr, expected_ty) in fields.iter().zip(sig.fields.iter()) {
                        let field_ty = self.infer_expr(*field_expr, env);
                        let _ = self.unify_with(
                            field_ty,
                            InferTy::Concrete(*expected_ty),
                            expr.span,
                            "TYPE_ENUM_FIELD_MISMATCH",
                            "Enum field type mismatch",
                        );
                    }
                    InferTy::Concrete(sig.result)
                } else {
                    self.diagnostics.error(
                        "TYPE_UNKNOWN_ENUM_CTOR",
                        "Unknown enum constructor",
                        expr.span,
                    );
                    for field in fields {
                        let _ = self.infer_expr(*field, env);
                    }
                    InferTy::Concrete(self.error_type)
                }
            }
            ExprKind::Error(_) => InferTy::Concrete(self.error_type),
        };

        self.record_expr_type(expr_id, inferred, expr.span)
    }

    fn record_expr_type(&mut self, expr_id: ExprId, inferred: InferTy, span: Span) -> InferTy {
        if let Some(existing) = self.expr_tys.get(expr_id.index()).copied().flatten() {
            let merged = self.unify_with(
                existing,
                inferred,
                span,
                "TYPE_EXPR_CONTEXT_MISMATCH",
                "Expression inferred with incompatible types in different contexts",
            );
            if let Some(slot) = self.expr_tys.get_mut(expr_id.index()) {
                *slot = Some(merged);
            }
            merged
        } else {
            if let Some(slot) = self.expr_tys.get_mut(expr_id.index()) {
                *slot = Some(inferred);
            }
            inferred
        }
    }

    fn instantiate_template(
        &mut self,
        template: TypeTemplate,
        generic_inst: &[InferTy],
    ) -> InferTy {
        match template {
            TypeTemplate::Concrete(ty) => InferTy::Concrete(ty),
            TypeTemplate::Generic(idx) => generic_inst
                .get(idx as usize)
                .copied()
                .unwrap_or(InferTy::Concrete(self.error_type)),
        }
    }

    fn instantiate_scheme(&mut self, scheme: Scheme) -> InferTy {
        match scheme {
            Scheme::Concrete(ty) => InferTy::Concrete(ty),
            Scheme::MonoVar(var) => self.infer.resolve(InferTy::Var(var)),
            Scheme::Generic => self.infer.fresh_ty(),
        }
    }

    fn mono_scheme(&mut self, ty: InferTy) -> Scheme {
        match self.infer.resolve(ty) {
            InferTy::Concrete(ty) => Scheme::Concrete(ty),
            InferTy::Var(var) => Scheme::MonoVar(var),
        }
    }

    fn generalize_let(&mut self, ty: InferTy, env: &Env) -> Scheme {
        match self.infer.resolve(ty) {
            InferTy::Concrete(ty) => Scheme::Concrete(ty),
            InferTy::Var(var) => {
                let env_vars = self.env_mono_vars(env);
                if env_vars.contains(&var) {
                    Scheme::MonoVar(var)
                } else {
                    Scheme::Generic
                }
            }
        }
    }

    fn env_mono_vars(&mut self, env: &Env) -> HashSet<InferVarId> {
        let mut vars = HashSet::new();
        for scheme in env.values() {
            if let Scheme::MonoVar(var) = scheme {
                vars.insert(self.infer.find(*var));
            }
        }
        vars
    }

    fn pick_numeric_type(&mut self, left: InferTy, right: Option<InferTy>, span: Span) -> InferTy {
        for candidate in [left, right.unwrap_or(left)] {
            if let Some(ty) = self.infer.resolve_concrete(candidate)
                && (ty == self.prim.int || ty == self.prim.float)
            {
                return InferTy::Concrete(ty);
            }
        }

        self.diagnostics.error(
            "TYPE_NUMERIC_REQUIRED",
            "Expected numeric type (`Int` or `Float`)",
            span,
        );
        InferTy::Concrete(self.prim.int)
    }

    fn unify_with(
        &mut self,
        left: InferTy,
        right: InferTy,
        span: Span,
        code: &'static str,
        message: &str,
    ) -> InferTy {
        match self.infer.unify(left, right) {
            Ok(ty) => ty,
            Err(mismatch) => {
                self.diagnostics.error(
                    code,
                    format!(
                        "{}: {} vs {}",
                        message,
                        self.type_name(mismatch.left),
                        self.type_name(mismatch.right)
                    ),
                    span,
                );
                InferTy::Concrete(self.error_type)
            }
        }
    }

    fn materialize_ty(&mut self, ty: InferTy) -> TypeId {
        match self.infer.resolve(ty) {
            InferTy::Concrete(ty) => ty,
            InferTy::Var(var) => {
                let root = self.infer.find(var);
                if let Some(existing) = self.unresolved_type_params.get(&root).copied() {
                    return existing;
                }
                let param_index = self.unresolved_type_params.len();
                let param = self.store.intern(TypeKind::TypeParam(
                    (param_index.min(u16::MAX as usize)) as u16,
                ));
                self.unresolved_type_params.insert(root, param);
                param
            }
        }
    }

    fn type_name(&self, ty: TypeId) -> String {
        match self.store.get(ty) {
            Some(TypeKind::Primitive(PrimitiveType::Unit)) => "Unit".to_owned(),
            Some(TypeKind::Primitive(PrimitiveType::Bool)) => "Bool".to_owned(),
            Some(TypeKind::Primitive(PrimitiveType::Int)) => "Int".to_owned(),
            Some(TypeKind::Primitive(PrimitiveType::Float)) => "Float".to_owned(),
            Some(TypeKind::Primitive(PrimitiveType::Char)) => "Char".to_owned(),
            Some(TypeKind::Primitive(PrimitiveType::String)) => "String".to_owned(),
            Some(TypeKind::Struct { name, .. }) => format!("Struct#{}", name.as_u32()),
            Some(TypeKind::Enum { name, .. }) => format!("Enum#{}", name.as_u32()),
            Some(TypeKind::TypeParam(idx)) => format!("T{}", idx),
            Some(TypeKind::Function(_)) => "Function".to_owned(),
            Some(TypeKind::Error) | None => format!("t{}", ty.as_u32()),
        }
    }
}

pub fn typecheck_core(program: &CoreProgram, diagnostics: &mut DiagnosticBag) -> SemanticTables {
    TypeChecker::new(program, diagnostics).run(EffectConformance::Check)
}

/// For Core that has been through residualization, which erases every
/// `declared_effects` row. Conformance cannot be checked there: the
/// declarations it would compare against are gone.
pub fn typecheck_residual_core(
    program: &CoreProgram,
    diagnostics: &mut DiagnosticBag,
) -> SemanticTables {
    TypeChecker::new(program, diagnostics).run(EffectConformance::Skip)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EffectConformance {
    Check,
    Skip,
}

fn build_function_templates(
    program: &CoreProgram,
    adt_types: &HashMap<SymbolId, TypeId>,
    prim: PrimitiveTypeIds,
    error_type: TypeId,
    diagnostics: &mut DiagnosticBag,
) -> Vec<FunctionTemplate> {
    let mut templates = Vec::with_capacity(program.functions().len());

    for function in program.functions() {
        let mut generics = HashMap::<SymbolId, u16>::new();
        let mut params = Vec::with_capacity(function.param_types.len());

        for param_ty in &function.param_types {
            match template_type_from_ref(param_ty, adt_types, prim, &mut generics) {
                Some(template) => params.push(template),
                None => {
                    diagnostics.error(
                        "TYPE_PARAM_TYPE_UNKNOWN",
                        "Could not resolve parameter type annotation",
                        function.span,
                    );
                    params.push(TypeTemplate::Concrete(error_type));
                }
            }
        }

        let ret = match function.return_type {
            CoreTypeRef::Unknown => None,
            _ => {
                match template_type_from_ref(&function.return_type, adt_types, prim, &mut generics)
                {
                    Some(template) => Some(template),
                    None => {
                        diagnostics.error(
                            "TYPE_RETURN_TYPE_UNKNOWN",
                            "Could not resolve return type annotation",
                            function.span,
                        );
                        Some(TypeTemplate::Concrete(error_type))
                    }
                }
            }
        };

        templates.push(FunctionTemplate {
            params,
            ret,
            generic_count: generics.len(),
            inferred_ret_var: None,
        });
    }

    templates
}

fn template_type_from_ref(
    ty: &CoreTypeRef,
    adt_types: &HashMap<SymbolId, TypeId>,
    prim: PrimitiveTypeIds,
    generics: &mut HashMap<SymbolId, u16>,
) -> Option<TypeTemplate> {
    match ty {
        CoreTypeRef::Unit => Some(TypeTemplate::Concrete(prim.unit)),
        CoreTypeRef::Primitive(primitive) => Some(TypeTemplate::Concrete(match primitive {
            PrimitiveTypeRef::Bool => prim.bool_,
            PrimitiveTypeRef::Int => prim.int,
            PrimitiveTypeRef::Float => prim.float,
            PrimitiveTypeRef::Char => prim.char_,
            PrimitiveTypeRef::String => prim.string,
        })),
        CoreTypeRef::Named(name) => {
            if let Some(adt) = adt_types.get(name).copied() {
                Some(TypeTemplate::Concrete(adt))
            } else {
                let idx = if let Some(existing) = generics.get(name).copied() {
                    existing
                } else {
                    let next = generics.len();
                    let next = (next.min(u16::MAX as usize)) as u16;
                    generics.insert(*name, next);
                    next
                };
                Some(TypeTemplate::Generic(idx))
            }
        }
        CoreTypeRef::Unknown => None,
    }
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
                        .map(|ty| resolve_concrete_type_ref(ty, adt_types, prim))
                        .collect(),
                    return_type: resolve_concrete_type_ref(&operation.return_type, adt_types, prim),
                },
            );
        }
    }
    table
}

fn build_ctor_signatures(
    program: &CoreProgram,
    store: &TypeStore,
    adt_types: &HashMap<SymbolId, TypeId>,
) -> (
    HashMap<SymbolId, StructCtorSig>,
    HashMap<SymbolId, EnumCtorSig>,
) {
    let mut struct_ctors = HashMap::new();
    let mut enum_ctors = HashMap::new();

    for decl in program.structs() {
        let Some(result) = adt_types.get(&decl.name).copied() else {
            continue;
        };
        let Some(TypeKind::Struct { fields, .. }) = store.get(result) else {
            continue;
        };
        struct_ctors.insert(
            decl.name,
            StructCtorSig {
                result,
                fields: fields.iter().map(|field| field.ty).collect(),
            },
        );
    }

    for decl in program.enums() {
        let Some(result) = adt_types.get(&decl.name).copied() else {
            continue;
        };
        let Some(TypeKind::Enum { variants, .. }) = store.get(result) else {
            continue;
        };
        for variant in variants {
            enum_ctors.insert(
                variant.name,
                EnumCtorSig {
                    result,
                    fields: variant.fields.clone(),
                },
            );
        }
    }

    (struct_ctors, enum_ctors)
}

fn intern_program_adts(
    program: &CoreProgram,
    store: &mut TypeStore,
    prim: PrimitiveTypeIds,
    error_type: TypeId,
    diagnostics: &mut DiagnosticBag,
) -> HashMap<SymbolId, TypeId> {
    let mut adt_types = HashMap::new();

    for decl in program.structs() {
        if adt_types.contains_key(&decl.name) {
            continue;
        }
        let fields = decl
            .fields
            .iter()
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
                fields: variant.fields.iter().map(|_| TypeId::INVALID).collect(),
            })
            .collect();
        let ty = store.intern(TypeKind::Enum {
            name: decl.name,
            variants,
        });
        adt_types.insert(decl.name, ty);
    }

    for decl in program.structs() {
        let Some(ty_id) = adt_types.get(&decl.name).copied() else {
            continue;
        };
        let resolved_fields = decl
            .fields
            .iter()
            .map(|field_ty| {
                resolve_concrete_type_ref(field_ty, &adt_types, prim).unwrap_or_else(|| {
                    diagnostics.error(
                        "TYPE_UNKNOWN_ADT_FIELD_TYPE",
                        "Could not resolve struct field type",
                        decl.span,
                    );
                    error_type
                })
            })
            .collect::<Vec<_>>();

        if let Some(TypeKind::Struct { fields, .. }) = store.get_mut(ty_id) {
            for (field, resolved_ty) in fields.iter_mut().zip(resolved_fields) {
                field.ty = resolved_ty;
            }
        }
    }

    for decl in program.enums() {
        let Some(ty_id) = adt_types.get(&decl.name).copied() else {
            continue;
        };

        let resolved_variants = decl
            .variants
            .iter()
            .map(|variant| {
                let fields = variant
                    .fields
                    .iter()
                    .map(|field_ty| {
                        resolve_concrete_type_ref(field_ty, &adt_types, prim).unwrap_or_else(|| {
                            diagnostics.error(
                                "TYPE_UNKNOWN_ADT_FIELD_TYPE",
                                "Could not resolve enum variant field type",
                                variant.span,
                            );
                            error_type
                        })
                    })
                    .collect::<Vec<_>>();
                (variant.name, fields)
            })
            .collect::<Vec<_>>();

        if let Some(TypeKind::Enum { variants, .. }) = store.get_mut(ty_id) {
            for (variant_name, resolved_fields) in resolved_variants {
                if let Some(variant) = variants.iter_mut().find(|entry| entry.name == variant_name)
                {
                    variant.fields = resolved_fields;
                }
            }
        }
    }

    adt_types
}

fn resolve_concrete_type_ref(
    ty: &CoreTypeRef,
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
        CoreTypeRef::Named(name) => adt_types.get(name).copied(),
        CoreTypeRef::Unknown => None,
    }
}

fn assign_var_ownership(
    out: &mut DenseMap<VarId, OwnershipClass>,
    var: VarId,
    ownership: OwnershipClass,
) {
    if let Some(existing) = out.get(&var).copied() {
        out.insert(var, existing.merge(ownership));
        return;
    }
    out.insert(var, ownership);
}

fn infer_stmt_effects(program: &CoreProgram, out: &mut [SortedEffectRow]) {
    let mut memo: Vec<Option<SortedEffectRow>> = vec![None; out.len()];
    let mut visiting = HashSet::new();
    for idx in 0..program.stmts().len() {
        let stmt_id = StmtId::new(idx);
        let _ =
            program.fold_stmts(
                stmt_id,
                &mut memo,
                &mut visiting,
                &mut |stmt, children| match &stmt.kind {
                    StmtKind::Return(_) => SortedEffectRow::empty(),
                    StmtKind::Let { .. } => children.first().cloned().unwrap_or_default(),
                    StmtKind::Resume { .. } => children.first().cloned().unwrap_or_default(),
                    StmtKind::Val { .. } => children
                        .first()
                        .cloned()
                        .unwrap_or_default()
                        .union(&children.get(1).cloned().unwrap_or_default()),
                    StmtKind::Call { effects, .. } => {
                        effects.union(&children.first().cloned().unwrap_or_default())
                    }
                    StmtKind::Perform { effect, .. } => SortedEffectRow::singleton(*effect)
                        .union(&children.first().cloned().unwrap_or_default()),
                    StmtKind::If { .. } => children
                        .first()
                        .cloned()
                        .unwrap_or_default()
                        .union(&children.get(1).cloned().unwrap_or_default()),
                    StmtKind::Match { .. } => children
                        .iter()
                        .cloned()
                        .fold(SortedEffectRow::empty(), |acc, row| acc.union(&row)),
                    StmtKind::Handle { handler, next, .. } => {
                        let mut row = children.first().cloned().unwrap_or_default();
                        if let Some(effect) =
                            program.handlers().get(handler.index()).map(|h| h.effect)
                        {
                            row = row.subtract(&SortedEffectRow::singleton(effect));
                        }
                        if next.is_some() {
                            row = row.union(&children.get(1).cloned().unwrap_or_default());
                        }
                        row
                    }
                    StmtKind::Stage { next, .. } => {
                        let mut row = children.first().cloned().unwrap_or_default();
                        if next.is_some() {
                            row = row.union(&children.get(1).cloned().unwrap_or_default());
                        }
                        row
                    }
                    StmtKind::Hole { .. } | StmtKind::Error(_) => SortedEffectRow::empty(),
                },
            );
    }

    for (idx, row) in memo.into_iter().enumerate() {
        out[idx] = row.unwrap_or_default();
    }
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
