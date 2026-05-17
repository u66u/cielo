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

use crate::facts::{CallSite, SemanticTables};
use crate::ownership::{classify_core_type_ref, classify_type_kind};
use crate::ty::{EnumVariant, FunctionType, PrimitiveType, StructField, TypeKind, TypeStore};
use cielo_base::Span;
use cielo_base::densemap::DenseMap;
use cielo_base::diagnostics::DiagnosticBag;
use cielo_base::{
    EffectLabelId, ExprId, FuncId, HandlerId, Interner, StmtId, SymbolId, TypeId, VarId,
};
use cielo_ir::core::{
    CoreProgram, CoreTypeRef, ExprKind, Literal, OpCategory, PrimitiveTypeRef, StmtKind, UnaryOp,
};
use cielo_ir::effect::SortedEffectRow;
use cielo_ir::function_graph::closure_body_functions;
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

/// Instantiating a declaration deeper than this means it instantiates itself at
/// an ever larger type, which never terminates.
const MAX_ADT_INSTANTIATION_DEPTH: usize = 16;

#[derive(Clone, Copy)]
struct CtorCodes {
    unknown: &'static str,
    unknown_message: &'static str,
    arity: &'static str,
    arity_message: &'static str,
    field: &'static str,
    field_message: &'static str,
}

const STRUCT_CTOR_CODES: CtorCodes = CtorCodes {
    unknown: "TYPE_UNKNOWN_STRUCT_CTOR",
    unknown_message: "Unknown struct constructor",
    arity: "TYPE_BAD_STRUCT_CTOR_ARITY",
    arity_message: "Struct constructor arity mismatch",
    field: "TYPE_STRUCT_FIELD_MISMATCH",
    field_message: "Struct field type mismatch",
};

const ENUM_CTOR_CODES: CtorCodes = CtorCodes {
    unknown: "TYPE_UNKNOWN_ENUM_CTOR",
    unknown_message: "Unknown enum constructor",
    arity: "TYPE_BAD_ENUM_CTOR_ARITY",
    arity_message: "Enum constructor arity mismatch",
    field: "TYPE_ENUM_FIELD_MISMATCH",
    field_message: "Enum field type mismatch",
};

#[derive(Clone, Debug)]
struct EffectSignature {
    param_types: Vec<Option<TypeId>>,
    return_type: Option<TypeId>,
}

type EffectSignatureTable = HashMap<(EffectLabelId, SymbolId), EffectSignature>;

/// The declared shape of one ADT, kept in `CoreTypeRef` form so a generic
/// declaration can be instantiated more than once.
#[derive(Clone, Debug)]
struct AdtTemplate {
    type_params: Vec<SymbolId>,
    shape: AdtShape,
    span: Span,
}

#[derive(Clone, Debug)]
enum AdtShape {
    Struct {
        field_names: Vec<SymbolId>,
        fields: Vec<CoreTypeRef>,
    },
    Enum {
        variants: Vec<(SymbolId, Vec<CoreTypeRef>)>,
    },
}

/// A constructor call resolved back to its declaring ADT.
#[derive(Clone, Debug)]
struct CtorTemplate {
    adt: SymbolId,
    fields: Vec<CoreTypeRef>,
}

#[derive(Clone, Debug)]
struct FunctionTemplate {
    params: Vec<CoreTypeRef>,
    ret: Option<CoreTypeRef>,
    /// Type-parameter names in first-occurrence order across the signature.
    generic_names: Vec<SymbolId>,
    inferred_ret_var: Option<InferVarId>,
    /// One slot per parameter whose type is `Unknown`, which is how a lifted
    /// closure body arrives: nothing writes a lambda's parameter types, so
    /// they are inferred from the body exactly like a missing return type.
    inferred_param_vars: Vec<Option<InferVarId>>,
}

/// A call whose callee is generic, held until inference finishes: the
/// instantiation variables are only meaningful once every constraint is in.
struct PendingCallTypeArgs {
    site: CallSite,
    bindings: Vec<(SymbolId, InferTy)>,
    span: Span,
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

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct AppId(u32);

impl AppId {
    fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum InferTy {
    Concrete(TypeId),
    Var(InferVarId),
    /// `Name[..]` whose arguments are not all known yet. Collapses to
    /// `Concrete` once every argument resolves and the instance is interned.
    App(AppId),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Scheme {
    Concrete(TypeId),
    MonoVar(InferVarId),
    Generic,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct TypeMismatch {
    left: InferTy,
    right: InferTy,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ResumeExpectation {
    arg: InferTy,
    result: InferTy,
}

type Env = HashMap<VarId, Scheme>;
type ResumeCtx = HashMap<VarId, ResumeExpectation>;

#[derive(Clone, Debug)]
struct AppTy {
    name: SymbolId,
    args: Vec<InferTy>,
}

#[derive(Clone, Debug, Default)]
struct InferState {
    parent: Vec<InferVarId>,
    rank: Vec<u8>,
    binding: Vec<Option<InferTy>>,
    /// Which declared type parameter a variable stands for, when it is a
    /// function's own generic slot. Monomorphization needs the name to write
    /// substituted signatures, and the index alone would not survive union.
    label: Vec<Option<SymbolId>>,
    apps: Vec<AppTy>,
    /// Identity `(name, args)` of every interned ADT instance. Kept beside the
    /// solver so `unify` can decompose `Name[..] ~ instance` without the store.
    instance_shape: HashMap<TypeId, (SymbolId, Vec<TypeId>)>,
}

impl InferState {
    fn fresh_var(&mut self) -> InferVarId {
        let id = InferVarId::from_index(self.parent.len());
        self.parent.push(id);
        self.rank.push(0);
        self.binding.push(None);
        self.label.push(None);
        id
    }

    fn fresh_ty(&mut self) -> InferTy {
        InferTy::Var(self.fresh_var())
    }

    fn app(&mut self, name: SymbolId, args: Vec<InferTy>) -> InferTy {
        let id = AppId(self.apps.len() as u32);
        self.apps.push(AppTy { name, args });
        InferTy::App(id)
    }

    fn set_label(&mut self, ty: InferTy, name: SymbolId) {
        if let InferTy::Var(var) = ty {
            let root = self.find(var);
            self.label[root.index()].get_or_insert(name);
        }
    }

    fn label_of(&mut self, var: InferVarId) -> Option<SymbolId> {
        let root = self.find(var);
        self.label[root.index()]
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

    /// Follows variable bindings to a head that is `Concrete`, an unbound
    /// `Var`, or an `App`. `App` arguments are left unresolved; callers that
    /// need them resolve recursively.
    fn resolve(&mut self, ty: InferTy) -> InferTy {
        match ty {
            InferTy::Concrete(ty) => InferTy::Concrete(ty),
            InferTy::App(app) => InferTy::App(app),
            InferTy::Var(var) => {
                let root = self.find(var);
                match self.binding[root.index()] {
                    Some(bound) => self.resolve(bound),
                    None => InferTy::Var(root),
                }
            }
        }
    }

    fn resolve_concrete(&mut self, ty: InferTy) -> Option<TypeId> {
        match self.resolve(ty) {
            InferTy::Concrete(ty) => Some(ty),
            InferTy::Var(_) | InferTy::App(_) => None,
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
                    Err(TypeMismatch { left, right })
                }
            }
            (InferTy::Var(lhs), InferTy::Var(rhs)) => self.union_vars(lhs, rhs),
            (InferTy::Var(var), other) | (other, InferTy::Var(var)) => {
                if self.occurs_in(var, other) {
                    return Err(TypeMismatch {
                        left: InferTy::Var(var),
                        right: other,
                    });
                }
                self.binding[var.index()] = Some(other);
                Ok(other)
            }
            (InferTy::App(lhs), InferTy::App(rhs)) => {
                let left_app = self.apps[lhs.index()].clone();
                let right_app = self.apps[rhs.index()].clone();
                if left_app.name != right_app.name || left_app.args.len() != right_app.args.len() {
                    return Err(TypeMismatch { left, right });
                }
                for (lhs_arg, rhs_arg) in left_app.args.iter().zip(right_app.args.iter()) {
                    self.unify(*lhs_arg, *rhs_arg)?;
                }
                Ok(left)
            }
            (InferTy::App(app), InferTy::Concrete(ty))
            | (InferTy::Concrete(ty), InferTy::App(app)) => {
                self.unify_app_with_instance(app, ty)?;
                Ok(InferTy::Concrete(ty))
            }
        }
    }

    /// Decomposes `Name[..] ~ <interned instance of Name>` through the
    /// instance's recorded arguments. Instance identity is `(name, args)`, so
    /// this is the only place structure re-enters the solver.
    fn unify_app_with_instance(
        &mut self,
        app: AppId,
        instance: TypeId,
    ) -> Result<(), TypeMismatch> {
        let mismatch = TypeMismatch {
            left: InferTy::App(app),
            right: InferTy::Concrete(instance),
        };
        let Some((name, args)) = self.instance_shape.get(&instance).cloned() else {
            return Err(mismatch);
        };
        let pending = self.apps[app.index()].clone();
        if pending.name != name || pending.args.len() != args.len() {
            return Err(mismatch);
        }
        for (arg, instance_arg) in pending.args.iter().zip(args.iter()) {
            self.unify(*arg, InferTy::Concrete(*instance_arg))?;
        }
        Ok(())
    }

    fn occurs_in(&mut self, var: InferVarId, ty: InferTy) -> bool {
        match self.resolve(ty) {
            InferTy::Concrete(_) => false,
            InferTy::Var(other) => other == var,
            InferTy::App(app) => {
                let args = self.apps[app.index()].args.clone();
                args.into_iter().any(|arg| self.occurs_in(var, arg))
            }
        }
    }

    fn union_vars(&mut self, left: InferVarId, right: InferVarId) -> Result<InferTy, TypeMismatch> {
        let left_root = self.find(left);
        let right_root = self.find(right);
        if left_root == right_root {
            return Ok(InferTy::Var(left_root));
        }

        let left_idx = left_root.index();
        let right_idx = right_root.index();
        let (root, child) = match self.rank[left_idx].cmp(&self.rank[right_idx]) {
            std::cmp::Ordering::Less => (right_root, left_root),
            std::cmp::Ordering::Greater => (left_root, right_root),
            std::cmp::Ordering::Equal => {
                self.rank[left_idx] = self.rank[left_idx].saturating_add(1);
                (left_root, right_root)
            }
        };

        self.parent[child.index()] = root;
        if self.label[root.index()].is_none() {
            self.label[root.index()] = self.label[child.index()];
        }
        Ok(InferTy::Var(root))
    }
}

struct TypeChecker<'a> {
    program: &'a CoreProgram,
    diagnostics: &'a mut DiagnosticBag,
    /// Diagnostic text only. Core carries no names, so without an interner a
    /// declaration is rendered as its symbol id.
    names: Option<&'a Interner>,
    store: TypeStore,
    prim: PrimitiveTypeIds,
    error_type: TypeId,
    adts: HashMap<SymbolId, AdtTemplate>,
    struct_ctors: HashMap<SymbolId, CtorTemplate>,
    /// Keyed by `(enum, variant)`: a variant name alone does not identify a
    /// constructor, since two enums may declare the same one.
    enum_ctors: HashMap<(SymbolId, SymbolId), CtorTemplate>,
    /// Every enum declaring a given variant name, in declaration order. Used
    /// only where the owning enum is not already known.
    variant_owners: HashMap<SymbolId, Vec<SymbolId>>,
    instances: HashMap<(SymbolId, Vec<TypeId>), TypeId>,
    function_instances: HashMap<(Vec<TypeId>, TypeId), TypeId>,
    effect_signatures: EffectSignatureTable,
    function_templates: Vec<FunctionTemplate>,
    infer: InferState,
    expr_tys: Vec<Option<InferTy>>,
    unresolved_type_params: HashMap<InferVarId, TypeId>,
    field_indices: HashMap<ExprId, u32>,
    pending_call_type_args: Vec<PendingCallTypeArgs>,
    /// Instantiation variables of the function currently being inferred, so a
    /// `let` annotation naming one of its type parameters resolves to the same
    /// variable its signature uses.
    generic_inst: HashMap<SymbolId, InferTy>,
}

impl<'a> TypeChecker<'a> {
    fn new(
        program: &'a CoreProgram,
        diagnostics: &'a mut DiagnosticBag,
        names: Option<&'a Interner>,
    ) -> Self {
        let mut store = TypeStore::new();
        let prim = intern_primitives(&mut store);
        let error_type = store.intern(TypeKind::Error);

        let mut checker = Self {
            program,
            diagnostics,
            names,
            store,
            prim,
            error_type,
            adts: HashMap::new(),
            struct_ctors: HashMap::new(),
            enum_ctors: HashMap::new(),
            variant_owners: HashMap::new(),
            instances: HashMap::new(),
            function_instances: HashMap::new(),
            effect_signatures: EffectSignatureTable::new(),
            function_templates: Vec::new(),
            infer: InferState::default(),
            expr_tys: vec![None; program.exprs().len()],
            unresolved_type_params: HashMap::new(),
            field_indices: HashMap::new(),
            pending_call_type_args: Vec::new(),
            generic_inst: HashMap::new(),
        };
        checker.register_adts();
        checker.intern_non_generic_adts();
        checker.build_effect_signatures();
        checker.build_function_templates();
        checker
    }

    fn register_adts(&mut self) {
        for decl in self.program.structs() {
            self.adts.entry(decl.name).or_insert_with(|| AdtTemplate {
                type_params: decl.type_params.clone(),
                shape: AdtShape::Struct {
                    field_names: decl.field_names.clone(),
                    fields: decl.fields.clone(),
                },
                span: decl.span,
            });
            self.struct_ctors.insert(
                decl.name,
                CtorTemplate {
                    adt: decl.name,
                    fields: decl.fields.clone(),
                },
            );
        }

        for decl in self.program.enums() {
            self.adts.entry(decl.name).or_insert_with(|| AdtTemplate {
                type_params: decl.type_params.clone(),
                shape: AdtShape::Enum {
                    variants: decl
                        .variants
                        .iter()
                        .map(|variant| (variant.name, variant.fields.clone()))
                        .collect(),
                },
                span: decl.span,
            });
            for variant in &decl.variants {
                let owners = self.variant_owners.entry(variant.name).or_default();
                if !owners.contains(&decl.name) {
                    owners.push(decl.name);
                }
                self.enum_ctors.insert(
                    (decl.name, variant.name),
                    CtorTemplate {
                        adt: decl.name,
                        fields: variant.fields.clone(),
                    },
                );
            }
        }
    }

    /// Non-generic declarations are interned up front so their `TypeId`s exist
    /// even when nothing in the program mentions them. Declaration order, not
    /// map order: `TypeId`s must not vary between runs.
    fn intern_non_generic_adts(&mut self) {
        let names = self
            .program
            .structs()
            .iter()
            .map(|decl| (decl.name, decl.span))
            .chain(
                self.program
                    .enums()
                    .iter()
                    .map(|decl| (decl.name, decl.span)),
            )
            .filter(|(name, _)| {
                self.adts
                    .get(name)
                    .is_some_and(|decl| decl.type_params.is_empty())
            })
            .collect::<Vec<_>>();
        for (name, span) in names {
            let _ = self.adt_instance(name, Vec::new(), span, 0);
        }
    }

    /// Interns `Name[args]` as a distinct type. The placeholder is registered
    /// before field resolution so a recursive declaration terminates; the depth
    /// bound catches a declaration whose instantiation grows without limit.
    fn adt_instance(
        &mut self,
        name: SymbolId,
        args: Vec<TypeId>,
        span: Span,
        depth: usize,
    ) -> Option<TypeId> {
        if let Some(existing) = self.instances.get(&(name, args.clone())).copied() {
            return Some(existing);
        }
        let decl = self.adts.get(&name)?.clone();
        if decl.type_params.len() != args.len() {
            self.diagnostics.error(
                "TYPE_BAD_TYPE_ARG_COUNT",
                format!(
                    "Type argument count mismatch: expected {}, got {}",
                    decl.type_params.len(),
                    args.len()
                ),
                span,
            );
            return None;
        }
        if depth > MAX_ADT_INSTANTIATION_DEPTH {
            self.diagnostics.error(
                "TYPE_ADT_INSTANTIATION_DEPTH",
                "Type instantiation does not terminate: the declaration instantiates itself at an ever larger type",
                span,
            );
            return None;
        }

        let placeholder = match &decl.shape {
            AdtShape::Struct { field_names, .. } => TypeKind::Struct {
                name,
                args: args.clone(),
                fields: field_names
                    .iter()
                    .map(|field| StructField {
                        name: *field,
                        ty: TypeId::INVALID,
                    })
                    .collect(),
            },
            AdtShape::Enum { variants } => TypeKind::Enum {
                name,
                args: args.clone(),
                variants: variants
                    .iter()
                    .map(|(variant, fields)| EnumVariant {
                        name: *variant,
                        fields: fields.iter().map(|_| TypeId::INVALID).collect(),
                    })
                    .collect(),
            },
        };
        let id = self.store.intern(placeholder);
        self.instances.insert((name, args.clone()), id);
        self.infer.instance_shape.insert(id, (name, args.clone()));

        let subst = param_substitution(&decl.type_params, &args);
        match &decl.shape {
            AdtShape::Struct { fields, .. } => {
                let resolved = fields
                    .iter()
                    .map(|field| self.adt_field_type(field, &subst, decl.span, depth + 1))
                    .collect::<Vec<_>>();
                if let Some(TypeKind::Struct { fields, .. }) = self.store.get_mut(id) {
                    for (field, ty) in fields.iter_mut().zip(resolved) {
                        field.ty = ty;
                    }
                }
            }
            AdtShape::Enum { variants } => {
                let resolved = variants
                    .iter()
                    .map(|(_, fields)| {
                        fields
                            .iter()
                            .map(|field| self.adt_field_type(field, &subst, decl.span, depth + 1))
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>();
                if let Some(TypeKind::Enum { variants, .. }) = self.store.get_mut(id) {
                    for (variant, fields) in variants.iter_mut().zip(resolved) {
                        variant.fields = fields;
                    }
                }
            }
        }
        Some(id)
    }

    fn adt_field_type(
        &mut self,
        ty: &CoreTypeRef,
        subst: &HashMap<SymbolId, TypeId>,
        span: Span,
        depth: usize,
    ) -> TypeId {
        match self.concrete_type_ref(ty, subst, span, depth) {
            Some(id) => id,
            None => {
                self.diagnostics.error(
                    "TYPE_UNKNOWN_ADT_FIELD_TYPE",
                    "Could not resolve declared field type",
                    span,
                );
                self.error_type
            }
        }
    }

    /// A `CoreTypeRef` resolved all the way to a `TypeId`. Only valid where
    /// every type parameter already has a concrete binding: ADT fields and
    /// effect operation signatures. `None` means the name does not resolve.
    fn concrete_type_ref(
        &mut self,
        ty: &CoreTypeRef,
        subst: &HashMap<SymbolId, TypeId>,
        span: Span,
        depth: usize,
    ) -> Option<TypeId> {
        match ty {
            CoreTypeRef::Unit => Some(self.prim.unit),
            CoreTypeRef::Primitive(primitive) => Some(self.primitive_type(*primitive)),
            CoreTypeRef::Named(name) => self.adt_instance(*name, Vec::new(), span, depth),
            CoreTypeRef::Param(name) => subst.get(name).copied(),
            CoreTypeRef::Applied { name, args } => {
                let args = args
                    .iter()
                    .map(|arg| self.concrete_type_ref(arg, subst, span, depth))
                    .collect::<Option<Vec<_>>>()?;
                self.adt_instance(*name, args, span, depth)
            }
            CoreTypeRef::Func { params, ret } => {
                let params = params
                    .iter()
                    .map(|param| self.concrete_type_ref(param, subst, span, depth))
                    .collect::<Option<Vec<_>>>()?;
                let ret = self.concrete_type_ref(ret, subst, span, depth)?;
                Some(self.function_instance(params, ret))
            }
            CoreTypeRef::Unknown => None,
        }
    }

    /// A `CoreTypeRef` as a solver type. Type parameters map to the caller's
    /// instantiation variables, and an unresolved application stays an `App`
    /// until its arguments are known.
    fn infer_type_ref(&mut self, ty: &CoreTypeRef, subst: &HashMap<SymbolId, InferTy>) -> InferTy {
        match ty {
            CoreTypeRef::Unit => InferTy::Concrete(self.prim.unit),
            CoreTypeRef::Primitive(primitive) => InferTy::Concrete(self.primitive_type(*primitive)),
            CoreTypeRef::Named(name) => match self.instances.get(&(*name, Vec::new())).copied() {
                Some(id) => InferTy::Concrete(id),
                None => InferTy::Concrete(self.error_type),
            },
            CoreTypeRef::Param(name) => subst
                .get(name)
                .copied()
                .unwrap_or(InferTy::Concrete(self.error_type)),
            CoreTypeRef::Applied { name, args } => {
                let args = args
                    .iter()
                    .map(|arg| self.infer_type_ref(arg, subst))
                    .collect();
                self.infer.app(*name, args)
            }
            // A function type has no `App` form, so an open component collapses
            // to the error type rather than staying a variable: a closure type
            // is only ever checked structurally against another written one.
            CoreTypeRef::Func { params, ret } => {
                let params = params
                    .iter()
                    .map(|param| {
                        let ty = self.infer_type_ref(param, subst);
                        self.materialize_ty(ty)
                    })
                    .collect::<Vec<_>>();
                let ret = self.infer_type_ref(ret, subst);
                let ret = self.materialize_ty(ret);
                InferTy::Concrete(self.function_instance(params, ret))
            }
            CoreTypeRef::Unknown => InferTy::Concrete(self.error_type),
        }
    }

    /// Interns `Fn(params) -> ret`, deduplicated by structure. `unify` compares
    /// concrete types by id, so two written occurrences of one function type
    /// have to land on the same `TypeId` or a closure never matches its
    /// parameter.
    fn function_instance(&mut self, params: Vec<TypeId>, ret: TypeId) -> TypeId {
        let key = (params.clone(), ret);
        if let Some(existing) = self.function_instances.get(&key).copied() {
            return existing;
        }
        let id = self.store.intern(TypeKind::Function(FunctionType {
            params,
            ret,
            effects: SortedEffectRow::empty(),
        }));
        self.function_instances.insert(key, id);
        id
    }

    fn primitive_type(&self, primitive: PrimitiveTypeRef) -> TypeId {
        match primitive {
            PrimitiveTypeRef::Bool => self.prim.bool_,
            PrimitiveTypeRef::Int => self.prim.int,
            PrimitiveTypeRef::Float => self.prim.float,
            PrimitiveTypeRef::Char => self.prim.char_,
            PrimitiveTypeRef::String => self.prim.string,
        }
    }

    fn build_effect_signatures(&mut self) {
        let effects = self.program.effects().to_vec();
        let empty = HashMap::new();
        for effect in &effects {
            for operation in &effect.operations {
                let param_types = operation
                    .param_types
                    .iter()
                    .map(|ty| self.optional_concrete_ref(ty, &empty, operation.span))
                    .collect();
                let return_type =
                    self.optional_concrete_ref(&operation.return_type, &empty, operation.span);
                self.effect_signatures.insert(
                    (effect.label, operation.name),
                    EffectSignature {
                        param_types,
                        return_type,
                    },
                );
            }
        }
    }

    fn optional_concrete_ref(
        &mut self,
        ty: &CoreTypeRef,
        subst: &HashMap<SymbolId, TypeId>,
        span: Span,
    ) -> Option<TypeId> {
        self.concrete_type_ref(ty, subst, span, 0)
    }

    /// A function is generic exactly when its signature mentions a declared
    /// type parameter. An undeclared capitalized name stays `Named`, so it
    /// reaches `concrete_type_ref` and is reported instead of silently
    /// becoming a type variable.
    fn build_function_templates(&mut self) {
        let functions = self.program.functions().to_vec();
        let mut templates = Vec::with_capacity(functions.len());
        for function in &functions {
            let mut generic_names = Vec::new();
            for ty in function.param_types.iter().chain([&function.return_type]) {
                collect_type_params(ty, &mut generic_names);
            }
            for ty in function.param_types.iter().chain([&function.return_type]) {
                self.check_signature_type(ty, function.span);
            }
            templates.push(FunctionTemplate {
                params: function.param_types.clone(),
                ret: match function.return_type {
                    CoreTypeRef::Unknown => None,
                    _ => Some(function.return_type.clone()),
                },
                generic_names,
                inferred_ret_var: None,
                inferred_param_vars: vec![None; function.param_types.len()],
            });
        }
        self.function_templates = templates;
    }

    /// A signature is checked structurally before inference runs: an
    /// uninstantiated signature never reaches `adt_instance`, so this is the
    /// only place its type names and arities are validated.
    fn check_signature_type(&mut self, ty: &CoreTypeRef, span: Span) {
        let (name, args) = match ty {
            CoreTypeRef::Named(name) => (*name, [].as_slice()),
            CoreTypeRef::Applied { name, args } => (*name, args.as_slice()),
            CoreTypeRef::Func { params, ret } => {
                for param in params {
                    self.check_signature_type(param, span);
                }
                self.check_signature_type(ret, span);
                return;
            }
            CoreTypeRef::Unit
            | CoreTypeRef::Primitive(_)
            | CoreTypeRef::Param(_)
            | CoreTypeRef::Unknown => return,
        };

        let Some(decl) = self.adts.get(&name) else {
            self.diagnostics.error(
                "TYPE_UNKNOWN_TYPE_NAME",
                "Unknown type name; declare it, or add it to the enclosing `[..]` type parameter list",
                span,
            );
            return;
        };
        if decl.type_params.len() != args.len() {
            let expected = decl.type_params.len();
            self.diagnostics.error(
                "TYPE_BAD_TYPE_ARG_COUNT",
                format!(
                    "Type argument count mismatch: expected {expected}, got {}",
                    args.len()
                ),
                span,
            );
        }
        for arg in args {
            self.check_signature_type(arg, span);
        }
    }

    fn run(mut self, conformance: EffectConformance) -> SemanticTables {
        for index in 0..self.function_templates.len() {
            if self.function_templates[index].ret.is_none() {
                let var = self.infer.fresh_var();
                self.function_templates[index].inferred_ret_var = Some(var);
            }
            for slot in 0..self.function_templates[index].params.len() {
                if self.function_templates[index].params[slot] == CoreTypeRef::Unknown {
                    let var = self.infer.fresh_var();
                    self.function_templates[index].inferred_param_vars[slot] = Some(var);
                }
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
        // A closure allocates, and a call through one hands back whatever the
        // lifted body returns. Neither type is pinned down when the context
        // leaves it open, and an unmanaged answer there leaks the allocation.
        for (index, expr) in self.program.exprs().iter().enumerate() {
            if matches!(
                expr.kind,
                ExprKind::MakeClosure { .. } | ExprKind::CallClosure { .. }
            ) && let Some(slot) = sema.ownership_of_expr.get_mut(index)
            {
                *slot = OwnershipClass::Managed;
            }
        }
        sema.ownership_of_var = self.classify_var_ownership(&sema);

        sema.field_index_of_expr = std::mem::take(&mut self.field_indices);
        sema.type_args_of_call = self.finalize_call_type_args();
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

        let closure_bodies = closure_body_functions(self.program);
        for (index, function) in self.program.functions().iter().enumerate() {
            // A lifted closure body's parameter types are written nowhere, and
            // its arguments arrive through the same sink ABI as any other
            // call's. Managed is the only safe answer: retain and release are
            // no-ops on an unmanaged runtime tag, while skipping them on a
            // managed value drops a reference someone else still holds.
            let lifted = closure_bodies.contains(&FuncId::new(index));
            for (idx, param) in function.params.iter().copied().enumerate() {
                let ownership = if lifted {
                    OwnershipClass::Managed
                } else {
                    function
                        .param_types
                        .get(idx)
                        .map(classify_core_type_ref)
                        .unwrap_or(OwnershipClass::BorrowedView)
                };
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
        let params = function.params.clone();
        let body = function.body;
        let span = function.span;
        let Some(template) = self.function_templates.get(func_id.index()).cloned() else {
            return;
        };

        let generic_inst = self.instantiate_generics(&template.generic_names);
        self.generic_inst.clone_from(&generic_inst);

        let mut env = Env::new();
        for (idx, param_var) in params.iter().copied().enumerate() {
            let param_ty = match template.inferred_param_vars.get(idx).copied().flatten() {
                Some(var) => InferTy::Var(var),
                None => template
                    .params
                    .get(idx)
                    .map(|tpl| self.infer_type_ref(tpl, &generic_inst))
                    .unwrap_or(InferTy::Concrete(self.error_type)),
            };
            env.insert(param_var, self.mono_scheme(param_ty));
        }

        let expected_return = template
            .ret
            .as_ref()
            .map(|tpl| self.infer_type_ref(tpl, &generic_inst))
            .or_else(|| template.inferred_ret_var.map(InferTy::Var))
            .unwrap_or(InferTy::Concrete(self.error_type));

        let mut resume_ctx = ResumeCtx::new();
        let body_ty = self.infer_stmt(body, &mut env, &mut resume_ctx);
        let _ = self.unify_with(
            body_ty,
            expected_return,
            span,
            "TYPE_RETURN_MISMATCH",
            "Function body type does not match return type",
        );
    }

    /// A `let x: T` annotation is a constraint, not a coercion: it unifies with
    /// the inferred type, which is what lets an explicit type argument reach a
    /// generic call that nothing else constrains.
    fn check_declared_binding(&mut self, binding: VarId, value_ty: InferTy, span: Span) -> InferTy {
        let Some(declared) = self.program.declared_var_type(binding).cloned() else {
            return value_ty;
        };
        self.check_signature_type(&declared, span);
        let inst = self.generic_inst.clone();
        let declared_ty = self.infer_type_ref(&declared, &inst);
        self.unify_with(
            value_ty,
            declared_ty,
            span,
            "TYPE_LET_ANNOTATION_MISMATCH",
            "`let` binding does not match its declared type",
        )
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
                let value_ty = self.check_declared_binding(*binding, value_ty, stmt.span);
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
                let value_ty = self.check_declared_binding(*binding, value_ty, stmt.span);
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
                let ret_ty =
                    self.infer_call(CallSite::Stmt(stmt_id), *callee, args, env, stmt.span);
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
                                let expected_name = self.stored_type_name(*expected_ty);
                                let actual_name = self.stored_type_name(actual);
                                self.diagnostics.error(
                                    "TYPE_EFFECT_ARG_MISMATCH",
                                    format!(
                                        "Effect argument #{} type mismatch: expected {expected_name}, got {actual_name}",
                                        idx + 1,
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

            if let Some(ctor) = self.resolve_arm_ctor(scrutinee_ty, arm.tag, arm.span) {
                let (result, field_tys) = self.instantiate_ctor(&ctor);
                let _ = self.unify_with(
                    scrutinee_ty,
                    result,
                    arm.span,
                    "TYPE_MATCH_SCRUTINEE_MISMATCH",
                    "Match arm variant does not match scrutinee type",
                );

                if arm.binders.len() != field_tys.len() {
                    self.diagnostics.error(
                        "TYPE_MATCH_ARM_ARITY",
                        format!(
                            "Match arm binder count mismatch: expected {}, got {}",
                            field_tys.len(),
                            arm.binders.len()
                        ),
                        arm.span,
                    );
                }

                for (binder, field_ty) in arm.binders.iter().zip(field_tys.iter()) {
                    let scheme = self.mono_scheme(*field_ty);
                    arm_env.insert(*binder, scheme);
                }
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

    /// Resolves `base.field` against the base's struct type and records the
    /// positional index for runtime lowering.
    fn infer_field_expr(
        &mut self,
        expr_id: ExprId,
        base: ExprId,
        field: SymbolId,
        env: &Env,
        span: Span,
    ) -> InferTy {
        let base_ty = self.infer_expr(base, env);
        let Some((adt, args)) = self.adt_shape_of(base_ty) else {
            self.diagnostics.error(
                "TYPE_FIELD_ON_NON_STRUCT",
                "Field access requires a struct value",
                span,
            );
            return InferTy::Concrete(self.error_type);
        };
        let Some(AdtTemplate {
            type_params,
            shape:
                AdtShape::Struct {
                    field_names,
                    fields,
                },
            ..
        }) = self.adts.get(&adt).cloned()
        else {
            self.diagnostics.error(
                "TYPE_FIELD_ON_NON_STRUCT",
                "Field access requires a struct value",
                span,
            );
            return InferTy::Concrete(self.error_type);
        };

        let Some(index) = field_names.iter().position(|name| *name == field) else {
            self.diagnostics.error(
                "TYPE_UNKNOWN_FIELD",
                "Struct has no field with this name",
                span,
            );
            return InferTy::Concrete(self.error_type);
        };

        self.field_indices.insert(expr_id, index as u32);
        let subst = param_substitution(&type_params, &args);
        self.infer_type_ref(&fields[index], &subst)
    }

    /// The declaring name and instantiation of an ADT-valued type, whether it
    /// is already interned or still an open application.
    fn adt_shape_of(&mut self, ty: InferTy) -> Option<(SymbolId, Vec<InferTy>)> {
        match self.infer.resolve(ty) {
            InferTy::Concrete(id) => match self.store.get(id)? {
                TypeKind::Struct { name, args, .. } | TypeKind::Enum { name, args, .. } => Some((
                    *name,
                    args.iter().map(|arg| InferTy::Concrete(*arg)).collect(),
                )),
                _ => None,
            },
            InferTy::App(app) => {
                let pending = &self.infer.apps[app.index()];
                Some((pending.name, pending.args.clone()))
            }
            InferTy::Var(_) => None,
        }
    }

    /// The scrutinee's own enum decides which variant an arm tag names. Only
    /// when the scrutinee type is still open does the tag have to identify an
    /// enum on its own; a tag the scrutinee does not declare still resolves
    /// elsewhere so the mismatch is reported against the scrutinee.
    fn resolve_arm_ctor(
        &mut self,
        scrutinee: InferTy,
        tag: SymbolId,
        span: Span,
    ) -> Option<CtorTemplate> {
        if let Some((adt, _)) = self.adt_shape_of(scrutinee)
            && let Some(ctor) = self.enum_ctors.get(&(adt, tag))
        {
            return Some(ctor.clone());
        }
        match self.variant_owners.get(&tag).map(Vec::as_slice) {
            Some([owner]) => self.enum_ctors.get(&(*owner, tag)).cloned(),
            Some(owners) if owners.len() > 1 => {
                let names = render_symbols(self.names, owners);
                self.diagnostics.error(
                    "TYPE_AMBIGUOUS_MATCH_VARIANT",
                    format!(
                        "Match arm variant is declared by more than one enum ({names}) and the scrutinee type is unknown"
                    ),
                    span,
                );
                None
            }
            _ => {
                self.diagnostics.error(
                    "TYPE_UNKNOWN_MATCH_VARIANT",
                    "Unknown enum variant in match arm",
                    span,
                );
                None
            }
        }
    }

    /// Fresh instantiation variables for a constructor's ADT, giving the
    /// result type and the field types it expects.
    fn instantiate_ctor(&mut self, ctor: &CtorTemplate) -> (InferTy, Vec<InferTy>) {
        let type_params = self
            .adts
            .get(&ctor.adt)
            .map(|decl| decl.type_params.clone())
            .unwrap_or_default();
        let slots = type_params
            .iter()
            .map(|_| self.infer.fresh_ty())
            .collect::<Vec<_>>();
        let subst = param_substitution(&type_params, &slots);
        let fields = ctor
            .fields
            .iter()
            .map(|field| self.infer_type_ref(field, &subst))
            .collect();
        let result = if slots.is_empty() {
            match self.instances.get(&(ctor.adt, Vec::new())).copied() {
                Some(id) => InferTy::Concrete(id),
                None => InferTy::Concrete(self.error_type),
            }
        } else {
            self.infer.app(ctor.adt, slots)
        };
        (result, fields)
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
        let Some((adt, _)) = self.adt_shape_of(scrutinee_ty) else {
            return;
        };
        let Some(AdtTemplate {
            shape: AdtShape::Enum { variants },
            ..
        }) = self.adts.get(&adt).cloned()
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
            .filter(|(name, _)| !covered.contains(name))
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

    fn infer_call(
        &mut self,
        site: CallSite,
        callee: FuncId,
        args: &[ExprId],
        env: &Env,
        span: Span,
    ) -> InferTy {
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

        let generic_inst = self.instantiate_generics(&template.generic_names);

        for (arg_expr, expected_tpl) in args.iter().zip(template.params.iter()) {
            let arg_ty = self.infer_expr(*arg_expr, env);
            let expected_ty = self.infer_type_ref(expected_tpl, &generic_inst);
            let _ = self.unify_with(
                arg_ty,
                expected_ty,
                span,
                "TYPE_CALL_ARG_MISMATCH",
                "Call argument type mismatch",
            );
        }

        if !template.generic_names.is_empty() {
            self.pending_call_type_args.push(PendingCallTypeArgs {
                site,
                bindings: template
                    .generic_names
                    .iter()
                    .map(|name| (*name, generic_inst[name]))
                    .collect(),
                span,
            });
        }

        template
            .ret
            .as_ref()
            .map(|ret| self.infer_type_ref(ret, &generic_inst))
            .or_else(|| template.inferred_ret_var.map(InferTy::Var))
            .unwrap_or(InferTy::Concrete(self.error_type))
    }

    fn core_type_ref_id(&self, ty: &CoreTypeRef) -> TypeId {
        match ty {
            CoreTypeRef::Unit => self.prim.unit,
            CoreTypeRef::Primitive(primitive) => self.primitive_type(*primitive),
            // Only builtin return types reach here, and no builtin returns a
            // named or generic type.
            CoreTypeRef::Named(_)
            | CoreTypeRef::Param(_)
            | CoreTypeRef::Applied { .. }
            | CoreTypeRef::Func { .. }
            | CoreTypeRef::Unknown => self.error_type,
        }
    }

    fn instantiate_generics(&mut self, names: &[SymbolId]) -> HashMap<SymbolId, InferTy> {
        names
            .iter()
            .map(|name| {
                let ty = self.infer.fresh_ty();
                self.infer.set_label(ty, *name);
                (*name, ty)
            })
            .collect()
    }

    /// Runs once inference is complete: a call's instantiation variables are
    /// only fully constrained after every body has been walked.
    fn finalize_call_type_args(&mut self) -> HashMap<CallSite, Vec<(SymbolId, CoreTypeRef)>> {
        let pending = std::mem::take(&mut self.pending_call_type_args);
        let mut out = HashMap::new();
        for entry in pending {
            let mut bindings = Vec::with_capacity(entry.bindings.len());
            let mut resolved = true;
            for (name, ty) in entry.bindings {
                match self.core_type_ref_of(ty) {
                    Some(core) => bindings.push((name, core)),
                    None => {
                        self.diagnostics.error(
                            "TYPE_UNINFERRED_TYPE_ARG",
                            "Could not infer the type arguments of this call; nothing at the call site determines them",
                            entry.span,
                        );
                        resolved = false;
                        break;
                    }
                }
            }
            if resolved {
                out.insert(entry.site, bindings);
            }
        }
        out
    }

    /// Solver type back to the syntactic form monomorphization substitutes
    /// into signatures. `None` means the type is still open.
    fn core_type_ref_of(&mut self, ty: InferTy) -> Option<CoreTypeRef> {
        match self.infer.resolve(ty) {
            InferTy::Concrete(id) => self.core_type_ref_of_id(id, 0),
            InferTy::Var(var) => self.infer.label_of(var).map(CoreTypeRef::Param),
            InferTy::App(app) => {
                let pending = self.infer.apps[app.index()].clone();
                let mut args = Vec::with_capacity(pending.args.len());
                for arg in pending.args {
                    args.push(self.core_type_ref_of(arg)?);
                }
                Some(CoreTypeRef::Applied {
                    name: pending.name,
                    args,
                })
            }
        }
    }

    fn core_type_ref_of_id(&self, id: TypeId, depth: usize) -> Option<CoreTypeRef> {
        if depth > MAX_ADT_INSTANTIATION_DEPTH {
            return None;
        }
        match self.store.get(id)? {
            TypeKind::Primitive(PrimitiveType::Unit) => Some(CoreTypeRef::Unit),
            TypeKind::Primitive(PrimitiveType::Bool) => {
                Some(CoreTypeRef::Primitive(PrimitiveTypeRef::Bool))
            }
            TypeKind::Primitive(PrimitiveType::Int) => {
                Some(CoreTypeRef::Primitive(PrimitiveTypeRef::Int))
            }
            TypeKind::Primitive(PrimitiveType::Float) => {
                Some(CoreTypeRef::Primitive(PrimitiveTypeRef::Float))
            }
            TypeKind::Primitive(PrimitiveType::Char) => {
                Some(CoreTypeRef::Primitive(PrimitiveTypeRef::Char))
            }
            TypeKind::Primitive(PrimitiveType::String) => {
                Some(CoreTypeRef::Primitive(PrimitiveTypeRef::String))
            }
            TypeKind::Struct { name, args, .. } | TypeKind::Enum { name, args, .. } => {
                if args.is_empty() {
                    return Some(CoreTypeRef::Named(*name));
                }
                let name = *name;
                let args = args
                    .iter()
                    .map(|arg| self.core_type_ref_of_id(*arg, depth + 1))
                    .collect::<Option<Vec<_>>>()?;
                Some(CoreTypeRef::Applied { name, args })
            }
            TypeKind::Function(_) | TypeKind::TypeParam(_) | TypeKind::Error => None,
        }
    }

    fn infer_expr(&mut self, expr_id: ExprId, env: &Env) -> InferTy {
        let Some(expr) = self.program.expr(expr_id) else {
            return InferTy::Concrete(self.error_type);
        };

        let inferred = match &expr.kind {
            ExprKind::Field { base, field } => {
                self.infer_field_expr(expr_id, *base, *field, env, expr.span)
            }
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
            ExprKind::PureCall { callee, args } => {
                self.infer_call(CallSite::Expr(expr_id), *callee, args, env, expr.span)
            }
            ExprKind::BuiltinCall { builtin, args } => {
                if args.len() != builtin.arity() {
                    self.diagnostics.error(
                        "TYPE_BAD_BUILTIN_ARITY",
                        format!(
                            "Builtin `{}` expects {} argument(s), got {}",
                            builtin.name(),
                            builtin.arity(),
                            args.len()
                        ),
                        expr.span,
                    );
                }
                let params = builtin.param_types();
                for (index, arg) in args.iter().enumerate() {
                    let arg_ty = self.infer_expr(*arg, env);
                    // A `None` parameter accepts any type, so inference still
                    // runs but nothing is unified against it.
                    let Some(Some(expected)) = params.get(index).copied() else {
                        continue;
                    };
                    let _ = self.unify_with(
                        arg_ty,
                        InferTy::Concrete(self.primitive_type(expected)),
                        expr.span,
                        "TYPE_BUILTIN_ARG_MISMATCH",
                        "Builtin argument type mismatch",
                    );
                }
                InferTy::Concrete(self.core_type_ref_id(&builtin.return_type()))
            }
            ExprKind::MakeStruct { ty, fields } => {
                let ctor = self.struct_ctors.get(ty).cloned();
                self.infer_ctor_expr(ctor, fields, env, expr.span, STRUCT_CTOR_CODES)
            }
            // Lowering already picked the enum, so the variant is looked up
            // inside it rather than by name across the program.
            ExprKind::MakeEnum {
                ty,
                variant,
                fields,
            } => {
                let ctor = self.enum_ctors.get(&(*ty, *variant)).cloned();
                self.infer_ctor_expr(ctor, fields, env, expr.span, ENUM_CTOR_CODES)
            }
            // The lifted body's own parameter and result types are inferred
            // inside it, and inference does not cross a function boundary, so
            // the closure's type is whatever its context pins it to.
            ExprKind::MakeClosure { captures, .. } => {
                for capture in captures {
                    let _ = self.infer_expr(*capture, env);
                }
                self.infer.fresh_ty()
            }
            ExprKind::CallClosure { callee, args } => {
                self.infer_call_closure(*callee, args, env, expr.span)
            }
            ExprKind::Error(_) => InferTy::Concrete(self.error_type),
        };

        self.record_expr_type(expr_id, inferred, expr.span)
    }

    /// Checks a call through a value. A callee whose type is still open is left
    /// alone: the closure's own signature is inferred in a separate function
    /// walk, so nothing here can constrain it, and the emitted adapter checks
    /// arity at runtime.
    fn infer_call_closure(
        &mut self,
        callee: ExprId,
        args: &[ExprId],
        env: &Env,
        span: Span,
    ) -> InferTy {
        let callee_ty = self.infer_expr(callee, env);
        let arg_tys = args
            .iter()
            .map(|arg| self.infer_expr(*arg, env))
            .collect::<Vec<_>>();
        let Some(resolved) = self.infer.resolve_concrete(callee_ty) else {
            return self.infer.fresh_ty();
        };
        let Some(TypeKind::Function(signature)) = self.store.get(resolved).cloned() else {
            if resolved != self.error_type {
                self.diagnostics.error(
                    "TYPE_NOT_CALLABLE",
                    "This value is not a function and cannot be called",
                    span,
                );
            }
            return InferTy::Concrete(self.error_type);
        };
        if signature.params.len() != arg_tys.len() {
            self.diagnostics.error(
                "TYPE_BAD_CLOSURE_ARITY",
                format!(
                    "Closure argument count mismatch: expected {}, got {}",
                    signature.params.len(),
                    arg_tys.len()
                ),
                span,
            );
        }
        for (arg_ty, param) in arg_tys.iter().zip(signature.params.iter()) {
            let _ = self.unify_with(
                *arg_ty,
                InferTy::Concrete(*param),
                span,
                "TYPE_CLOSURE_ARG_MISMATCH",
                "Closure argument type mismatch",
            );
        }
        InferTy::Concrete(signature.ret)
    }

    fn infer_ctor_expr(
        &mut self,
        ctor: Option<CtorTemplate>,
        fields: &[ExprId],
        env: &Env,
        span: Span,
        codes: CtorCodes,
    ) -> InferTy {
        let Some(ctor) = ctor else {
            self.diagnostics
                .error(codes.unknown, codes.unknown_message, span);
            for field in fields {
                let _ = self.infer_expr(*field, env);
            }
            return InferTy::Concrete(self.error_type);
        };

        let (result, expected) = self.instantiate_ctor(&ctor);
        if expected.len() != fields.len() {
            self.diagnostics.error(
                codes.arity,
                format!(
                    "{}: expected {}, got {}",
                    codes.arity_message,
                    expected.len(),
                    fields.len()
                ),
                span,
            );
        }
        for (field_expr, expected_ty) in fields.iter().zip(expected.iter()) {
            let field_ty = self.infer_expr(*field_expr, env);
            let _ = self.unify_with(
                field_ty,
                *expected_ty,
                span,
                codes.field,
                codes.field_message,
            );
        }
        result
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

    fn instantiate_scheme(&mut self, scheme: Scheme) -> InferTy {
        match scheme {
            Scheme::Concrete(ty) => InferTy::Concrete(ty),
            Scheme::MonoVar(var) => self.infer.resolve(InferTy::Var(var)),
            Scheme::Generic => self.infer.fresh_ty(),
        }
    }

    /// An open application cannot be held in a `Scheme`, so it is pinned to a
    /// fresh variable: instantiating it again would lose the sharing.
    fn mono_scheme(&mut self, ty: InferTy) -> Scheme {
        match self.infer.resolve(ty) {
            InferTy::Concrete(ty) => Scheme::Concrete(ty),
            InferTy::Var(var) => Scheme::MonoVar(var),
            InferTy::App(_) => {
                let var = self.infer.fresh_var();
                let _ = self.infer.unify(InferTy::Var(var), ty);
                Scheme::MonoVar(var)
            }
        }
    }

    fn generalize_let(&mut self, ty: InferTy, env: &Env) -> Scheme {
        match self.infer.resolve(ty) {
            InferTy::Var(var) => {
                let env_vars = self.env_mono_vars(env);
                if env_vars.contains(&var) {
                    Scheme::MonoVar(var)
                } else {
                    Scheme::Generic
                }
            }
            resolved => self.mono_scheme(resolved),
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
                let left = self.type_name(mismatch.left);
                let right = self.type_name(mismatch.right);
                self.diagnostics
                    .error(code, format!("{message}: {left} vs {right}"), span);
                InferTy::Concrete(self.error_type)
            }
        }
    }

    /// Collapses a solver type to a stored `TypeId`. An application whose
    /// arguments are all known becomes its interned instance; anything still
    /// open becomes a `TypeParam` placeholder shared by that variable.
    fn materialize_ty(&mut self, ty: InferTy) -> TypeId {
        match self.infer.resolve(ty) {
            InferTy::Concrete(ty) => ty,
            InferTy::App(app) => {
                let pending = self.infer.apps[app.index()].clone();
                let mut args = Vec::with_capacity(pending.args.len());
                for arg in pending.args {
                    args.push(self.materialize_ty(arg));
                }
                let span = self
                    .adts
                    .get(&pending.name)
                    .map(|decl| decl.span)
                    .unwrap_or_else(Span::synthetic);
                match self.adt_instance(pending.name, args, span, 0) {
                    Some(id) => id,
                    None => self.error_type,
                }
            }
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

    fn type_name(&mut self, ty: InferTy) -> String {
        match self.infer.resolve(ty) {
            InferTy::Var(var) => match self.infer.label_of(var) {
                Some(name) => match self.names.and_then(|names| names.resolve(name)) {
                    Some(text) => format!("type parameter {text}"),
                    None => format!("type parameter #{}", name.as_u32()),
                },
                None => "?".to_owned(),
            },
            InferTy::App(app) => {
                let pending = self.infer.apps[app.index()].clone();
                let args = pending
                    .args
                    .into_iter()
                    .map(|arg| self.type_name(arg))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{}[{args}]", render_symbol(self.names, pending.name))
            }
            InferTy::Concrete(ty) => self.stored_type_name(ty),
        }
    }

    fn stored_type_name(&self, ty: TypeId) -> String {
        match self.store.get(ty) {
            Some(TypeKind::Primitive(PrimitiveType::Unit)) => "Unit".to_owned(),
            Some(TypeKind::Primitive(PrimitiveType::Bool)) => "Bool".to_owned(),
            Some(TypeKind::Primitive(PrimitiveType::Int)) => "Int".to_owned(),
            Some(TypeKind::Primitive(PrimitiveType::Float)) => "Float".to_owned(),
            Some(TypeKind::Primitive(PrimitiveType::Char)) => "Char".to_owned(),
            Some(TypeKind::Primitive(PrimitiveType::String)) => "String".to_owned(),
            Some(TypeKind::Struct { name, args, .. }) | Some(TypeKind::Enum { name, args, .. }) => {
                let rendered = args
                    .iter()
                    .map(|arg| self.stored_type_name(*arg))
                    .collect::<Vec<_>>();
                let name = render_symbol(self.names, *name);
                if rendered.is_empty() {
                    name
                } else {
                    format!("{name}[{}]", rendered.join(", "))
                }
            }
            Some(TypeKind::TypeParam(idx)) => format!("T{idx}"),
            Some(TypeKind::Function(_)) => "Function".to_owned(),
            Some(TypeKind::Error) | None => format!("t{}", ty.as_u32()),
        }
    }
}

pub fn typecheck_core(
    program: &CoreProgram,
    diagnostics: &mut DiagnosticBag,
    names: Option<&Interner>,
) -> SemanticTables {
    TypeChecker::new(program, diagnostics, names).run(EffectConformance::Check)
}

/// For Core that has been through residualization, which erases every
/// `declared_effects` row. Conformance cannot be checked there: the
/// declarations it would compare against are gone.
pub fn typecheck_residual_core(
    program: &CoreProgram,
    diagnostics: &mut DiagnosticBag,
    names: Option<&Interner>,
) -> SemanticTables {
    TypeChecker::new(program, diagnostics, names).run(EffectConformance::Skip)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EffectConformance {
    Check,
    Skip,
}

fn render_symbols(names: Option<&Interner>, symbols: &[SymbolId]) -> String {
    symbols
        .iter()
        .map(|symbol| render_symbol(names, *symbol))
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_symbol(names: Option<&Interner>, symbol: SymbolId) -> String {
    match names.and_then(|names| names.resolve(symbol)) {
        Some(text) => text.to_owned(),
        None => format!("Adt#{}", symbol.as_u32()),
    }
}

fn param_substitution<T: Clone>(names: &[SymbolId], values: &[T]) -> HashMap<SymbolId, T> {
    names
        .iter()
        .copied()
        .zip(values.iter().cloned())
        .collect::<HashMap<_, _>>()
}

fn collect_type_params(ty: &CoreTypeRef, out: &mut Vec<SymbolId>) {
    match ty {
        CoreTypeRef::Param(name) => {
            if !out.contains(name) {
                out.push(*name);
            }
        }
        CoreTypeRef::Applied { args, .. } => {
            for arg in args {
                collect_type_params(arg, out);
            }
        }
        CoreTypeRef::Func { params, ret } => {
            for param in params {
                collect_type_params(param, out);
            }
            collect_type_params(ret, out);
        }
        CoreTypeRef::Unit
        | CoreTypeRef::Primitive(_)
        | CoreTypeRef::Named(_)
        | CoreTypeRef::Unknown => {}
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
        let _ = stmt_effect_row(program, StmtId::new(idx), &mut memo, &mut visiting);
    }

    for (idx, row) in memo.into_iter().enumerate() {
        out[idx] = row.unwrap_or_default();
    }
}

/// `CoreProgram::fold_stmts` cannot express this: a handler's clause bodies are
/// not `child_stmts` of its `Handle`, yet they contribute to its row. A clause
/// that performs some *other* effect escapes outward, and handler inlining in
/// `linearize` splices exactly those performs into the handled body. Since the
/// dead-handler test reads this row, dropping them elides the handler that the
/// spliced performs still need.
fn stmt_effect_row(
    program: &CoreProgram,
    stmt_id: StmtId,
    memo: &mut [Option<SortedEffectRow>],
    visiting: &mut HashSet<StmtId>,
) -> SortedEffectRow {
    if let Some(cached) = memo.get(stmt_id.index()).and_then(Clone::clone) {
        return cached;
    }
    if !visiting.insert(stmt_id) {
        return SortedEffectRow::empty();
    }

    let row = match program.stmt(stmt_id).map(|stmt| &stmt.kind) {
        None
        | Some(StmtKind::Return(_))
        | Some(StmtKind::Hole { .. })
        | Some(StmtKind::Error(_)) => SortedEffectRow::empty(),
        Some(StmtKind::Let { next, .. }) | Some(StmtKind::Resume { next, .. }) => {
            stmt_effect_row(program, *next, memo, visiting)
        }
        Some(StmtKind::Val { value, next, .. }) => stmt_effect_row(program, *value, memo, visiting)
            .union(&stmt_effect_row(program, *next, memo, visiting)),
        Some(StmtKind::Call { effects, next, .. }) => {
            effects.union(&stmt_effect_row(program, *next, memo, visiting))
        }
        Some(StmtKind::Perform { effect, next, .. }) => SortedEffectRow::singleton(*effect)
            .union(&stmt_effect_row(program, *next, memo, visiting)),
        Some(StmtKind::If {
            then_branch,
            else_branch,
            ..
        }) => stmt_effect_row(program, *then_branch, memo, visiting).union(&stmt_effect_row(
            program,
            *else_branch,
            memo,
            visiting,
        )),
        Some(StmtKind::Match { arms, default, .. }) => {
            let mut row = SortedEffectRow::empty();
            for arm in arms {
                row = row.union(&stmt_effect_row(program, arm.body, memo, visiting));
            }
            if let Some(default_stmt) = default {
                row = row.union(&stmt_effect_row(program, *default_stmt, memo, visiting));
            }
            row
        }
        Some(StmtKind::Handle {
            handler,
            body,
            next,
        }) => {
            let mut row = stmt_effect_row(program, *body, memo, visiting);
            if let Some(def) = program.handlers().get(handler.index()) {
                row = row.union(&stmt_effect_row(program, def.return_body, memo, visiting));
                for clause in &def.clauses {
                    row = row.union(&stmt_effect_row(program, clause.body, memo, visiting));
                }
                // A clause performing the handled effect is discharged by the
                // same inlining, and `linearize` errors if any survives.
                row = row.subtract(&SortedEffectRow::singleton(def.effect));
            }
            if let Some(next_stmt) = next {
                row = row.union(&stmt_effect_row(program, *next_stmt, memo, visiting));
            }
            row
        }
        Some(StmtKind::Stage { body, next, .. }) => {
            let mut row = stmt_effect_row(program, *body, memo, visiting);
            if let Some(next_stmt) = next {
                row = row.union(&stmt_effect_row(program, *next_stmt, memo, visiting));
            }
            row
        }
    };

    visiting.remove(&stmt_id);
    if let Some(slot) = memo.get_mut(stmt_id.index()) {
        *slot = Some(row.clone());
    }
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
