use crate::builtins::Builtin;
use crate::effect::{EffectProperties, SortedEffectRow};
use cielo_base::Span;
use cielo_base::diagnostics::ErrorNode;
use cielo_base::{EffectLabelId, ExprId, FuncId, HandlerId, StmtId, SymbolId, TypeId, VarId};
use serde::{Deserialize, Serialize};
use smallvec::{SmallVec, smallvec};
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Literal {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    Char(char),
    String(String),
}

impl PartialEq for Literal {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Unit, Self::Unit) => true,
            (Self::Bool(lhs), Self::Bool(rhs)) => lhs == rhs,
            (Self::Int(lhs), Self::Int(rhs)) => lhs == rhs,
            (Self::Float(lhs), Self::Float(rhs)) => {
                if lhs.is_nan() || rhs.is_nan() {
                    lhs.is_nan() && rhs.is_nan()
                } else {
                    lhs == rhs
                }
            }
            (Self::Char(lhs), Self::Char(rhs)) => lhs == rhs,
            (Self::String(lhs), Self::String(rhs)) => lhs == rhs,
            _ => false,
        }
    }
}

impl Eq for Literal {}

impl Hash for Literal {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Self::Unit => {}
            Self::Bool(value) => value.hash(state),
            Self::Int(value) => value.hash(state),
            Self::Float(value) => {
                let normalized = if value.is_nan() {
                    f64::NAN.to_bits()
                } else if *value == 0.0 {
                    0.0f64.to_bits()
                } else {
                    value.to_bits()
                };
                normalized.hash(state);
            }
            Self::Char(value) => value.hash(state),
            Self::String(value) => value.hash(state),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum UnaryOp {
    Neg,
    Not,
}

impl UnaryOp {
    pub const fn c_func(self) -> &'static str {
        match self {
            Self::Neg => "cv_neg",
            Self::Not => "cv_not",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum OpCategory {
    Arithmetic,
    Comparison,
    Equality,
    Logical,
}

impl BinaryOp {
    pub const fn category(self) -> OpCategory {
        match self {
            Self::Add | Self::Sub | Self::Mul | Self::Div | Self::Mod => OpCategory::Arithmetic,
            Self::Lt | Self::Le | Self::Gt | Self::Ge => OpCategory::Comparison,
            Self::Eq | Self::Ne => OpCategory::Equality,
            Self::And | Self::Or => OpCategory::Logical,
        }
    }

    pub const fn c_func(self) -> &'static str {
        match self {
            Self::Add => "cv_add",
            Self::Sub => "cv_sub",
            Self::Mul => "cv_mul",
            Self::Div => "cv_div",
            Self::Mod => "cv_mod",
            Self::Eq => "cv_eq",
            Self::Ne => "cv_ne",
            Self::Lt => "cv_lt",
            Self::Le => "cv_le",
            Self::Gt => "cv_gt",
            Self::Ge => "cv_ge",
            Self::And => "cv_and",
            Self::Or => "cv_or",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum StageDirective {
    Comptime,
    Runtime,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PrimitiveTypeRef {
    Bool,
    Int,
    Float,
    Char,
    String,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum CoreTypeRef {
    Unit,
    Primitive(PrimitiveTypeRef),
    Named(SymbolId),
    Unknown,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ExprNode {
    pub span: Span,
    pub kind: ExprKind,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum ExprKind {
    Var(VarId),
    Literal(Literal),
    Unary {
        op: UnaryOp,
        expr: ExprId,
    },
    Binary {
        op: BinaryOp,
        lhs: ExprId,
        rhs: ExprId,
    },
    PureCall {
        callee: FuncId,
        args: Vec<ExprId>,
    },
    /// A runtime-provided operation. Unlike `PureCall` there is no callee
    /// declaration, and unlike `Perform` it never consults the handler stack.
    BuiltinCall {
        builtin: Builtin,
        args: Vec<ExprId>,
    },
    /// Field name, not index: Core has no field names, so the index is
    /// resolved by typechecking into `SemanticTables::field_index_of_expr`.
    Field {
        base: ExprId,
        field: SymbolId,
    },
    MakeStruct {
        ty: SymbolId,
        fields: Vec<ExprId>,
    },
    MakeEnum {
        ty: SymbolId,
        variant: SymbolId,
        fields: Vec<ExprId>,
    },
    Error(ErrorNode),
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct StmtNode {
    pub span: Span,
    pub kind: StmtKind,
}

impl StmtNode {
    pub fn child_stmts(&self) -> SmallVec<[StmtId; 4]> {
        match &self.kind {
            StmtKind::Return(_) | StmtKind::Hole { .. } | StmtKind::Error(_) => SmallVec::new(),
            StmtKind::Let { next, .. }
            | StmtKind::Call { next, .. }
            | StmtKind::Resume { next, .. }
            | StmtKind::Perform { next, .. } => smallvec![*next],
            StmtKind::Val { value, next, .. } => smallvec![*value, *next],
            StmtKind::If {
                then_branch,
                else_branch,
                ..
            } => smallvec![*then_branch, *else_branch],
            StmtKind::Match { arms, default, .. } => {
                let mut children: SmallVec<[StmtId; 4]> = arms.iter().map(|arm| arm.body).collect();
                if let Some(default_stmt) = default {
                    children.push(*default_stmt);
                }
                children
            }
            StmtKind::Handle { body, next, .. } | StmtKind::Stage { body, next, .. } => {
                let mut children = smallvec![*body];
                if let Some(next_stmt) = next {
                    children.push(*next_stmt);
                }
                children
            }
        }
    }

    pub fn child_exprs(&self) -> SmallVec<[ExprId; 4]> {
        match &self.kind {
            StmtKind::Return(expr) => smallvec![*expr],
            StmtKind::Let { value, .. } => smallvec![*value],
            StmtKind::If { cond, .. }
            | StmtKind::Match {
                scrutinee: cond, ..
            } => {
                smallvec![*cond]
            }
            StmtKind::Resume { arg, .. } => smallvec![*arg],
            StmtKind::Call { args, .. } | StmtKind::Perform { args, .. } => {
                args.iter().copied().collect()
            }
            StmtKind::Val { .. }
            | StmtKind::Handle { .. }
            | StmtKind::Stage { .. }
            | StmtKind::Hole { .. }
            | StmtKind::Error(_) => SmallVec::new(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum StmtKind {
    Return(ExprId),
    Let {
        binding: VarId,
        value: ExprId,
        next: StmtId,
    },
    Val {
        binding: VarId,
        value: StmtId,
        next: StmtId,
    },
    Call {
        result: VarId,
        callee: FuncId,
        args: Vec<ExprId>,
        effects: SortedEffectRow,
        next: StmtId,
    },
    If {
        cond: ExprId,
        then_branch: StmtId,
        else_branch: StmtId,
    },
    Match {
        scrutinee: ExprId,
        arms: Vec<MatchArm>,
        default: Option<StmtId>,
    },
    Perform {
        result: Option<VarId>,
        effect: EffectLabelId,
        operation: SymbolId,
        args: Vec<ExprId>,
        next: StmtId,
    },
    Resume {
        result: VarId,
        resume: VarId,
        arg: ExprId,
        next: StmtId,
    },
    Handle {
        handler: HandlerId,
        body: StmtId,
        next: Option<StmtId>,
    },
    Stage {
        stage: StageDirective,
        body: StmtId,
        next: Option<StmtId>,
    },
    Hole {
        ty: TypeId,
    },
    Error(ErrorNode),
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct MatchArm {
    pub tag: SymbolId,
    pub binders: Vec<VarId>,
    pub body: StmtId,
    pub span: Span,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct HandlerClause {
    pub operation: SymbolId,
    pub params: Vec<VarId>,
    pub resume_param: Option<VarId>,
    pub body: StmtId,
    pub span: Span,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct EffectOperationDecl {
    pub name: SymbolId,
    pub param_types: Vec<CoreTypeRef>,
    pub return_type: CoreTypeRef,
    pub span: Span,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct EffectDecl {
    pub label: EffectLabelId,
    pub name: SymbolId,
    pub properties: EffectProperties,
    pub operations: Vec<EffectOperationDecl>,
    pub span: Span,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct HandlerDef {
    pub effect: EffectLabelId,
    pub return_param: VarId,
    pub return_body: StmtId,
    pub clauses: Vec<HandlerClause>,
    pub span: Span,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct FunctionDecl {
    pub name: SymbolId,
    pub params: Vec<VarId>,
    pub param_types: Vec<CoreTypeRef>,
    pub return_type: CoreTypeRef,
    pub declared_effects: SortedEffectRow,
    pub body: StmtId,
    pub ct_only: bool,
    pub span: Span,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct AdtStructDecl {
    pub name: SymbolId,
    pub fields: Vec<CoreTypeRef>,
    /// Parallel to `fields`. Needed so `base.field` can resolve to an index.
    pub field_names: Vec<SymbolId>,
    pub span: Span,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct AdtEnumVariantDecl {
    pub name: SymbolId,
    pub fields: Vec<CoreTypeRef>,
    pub span: Span,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct AdtEnumDecl {
    pub name: SymbolId,
    pub variants: Vec<AdtEnumVariantDecl>,
    pub span: Span,
}

#[derive(Clone, Debug, Default)]
pub struct CoreProgram {
    exprs: Vec<ExprNode>,
    stmts: Vec<StmtNode>,
    effects: Vec<EffectDecl>,
    handlers: Vec<HandlerDef>,
    functions: Vec<FunctionDecl>,
    structs: Vec<AdtStructDecl>,
    enums: Vec<AdtEnumDecl>,
    entrypoints: Vec<FuncId>,
}

impl CoreProgram {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_expr(&mut self, node: ExprNode) -> ExprId {
        let id = ExprId::new(self.exprs.len());
        self.exprs.push(node);
        id
    }

    pub fn push_stmt(&mut self, node: StmtNode) -> StmtId {
        let id = StmtId::new(self.stmts.len());
        self.stmts.push(node);
        id
    }

    pub fn add_handler(&mut self, handler: HandlerDef) -> HandlerId {
        let id = HandlerId::new(self.handlers.len());
        self.handlers.push(handler);
        id
    }

    pub fn add_effect(&mut self, effect: EffectDecl) -> EffectLabelId {
        let id = EffectLabelId::new(self.effects.len());
        debug_assert_eq!(
            id, effect.label,
            "effect label should be dense and match insertion order"
        );
        self.effects.push(effect);
        id
    }

    pub fn add_function(&mut self, function: FunctionDecl) -> FuncId {
        let id = FuncId::new(self.functions.len());
        self.functions.push(function);
        id
    }

    pub fn add_struct(&mut self, decl: AdtStructDecl) {
        self.structs.push(decl);
    }

    pub fn add_enum(&mut self, decl: AdtEnumDecl) {
        self.enums.push(decl);
    }

    pub fn set_entrypoints(&mut self, entrypoints: impl IntoIterator<Item = FuncId>) {
        self.entrypoints.clear();
        self.entrypoints.extend(entrypoints);
    }

    pub fn exprs(&self) -> &[ExprNode] {
        &self.exprs
    }

    pub fn stmts(&self) -> &[StmtNode] {
        &self.stmts
    }

    pub fn stmts_mut(&mut self) -> &mut [StmtNode] {
        &mut self.stmts
    }

    pub fn handlers(&self) -> &[HandlerDef] {
        &self.handlers
    }

    pub fn handler_mut(&mut self, id: HandlerId) -> Option<&mut HandlerDef> {
        self.handlers.get_mut(id.index())
    }

    pub fn effects(&self) -> &[EffectDecl] {
        &self.effects
    }

    pub fn functions(&self) -> &[FunctionDecl] {
        &self.functions
    }

    pub fn functions_mut(&mut self) -> &mut [FunctionDecl] {
        &mut self.functions
    }

    pub fn replace_functions(&mut self, functions: Vec<FunctionDecl>) {
        self.functions = functions;
    }

    pub fn structs(&self) -> &[AdtStructDecl] {
        &self.structs
    }

    pub fn enums(&self) -> &[AdtEnumDecl] {
        &self.enums
    }

    pub fn entrypoints(&self) -> &[FuncId] {
        &self.entrypoints
    }

    pub fn expr(&self, id: ExprId) -> Option<&ExprNode> {
        self.exprs.get(id.index())
    }

    pub fn expr_mut(&mut self, id: ExprId) -> Option<&mut ExprNode> {
        self.exprs.get_mut(id.index())
    }

    pub fn stmt(&self, id: StmtId) -> Option<&StmtNode> {
        self.stmts.get(id.index())
    }

    pub fn stmt_mut(&mut self, id: StmtId) -> Option<&mut StmtNode> {
        self.stmts.get_mut(id.index())
    }

    pub fn function(&self, id: FuncId) -> Option<&FunctionDecl> {
        self.functions.get(id.index())
    }

    pub fn function_mut(&mut self, id: FuncId) -> Option<&mut FunctionDecl> {
        self.functions.get_mut(id.index())
    }

    pub fn effect(&self, id: EffectLabelId) -> Option<&EffectDecl> {
        self.effects.get(id.index())
    }

    pub fn fold_stmts<T: Clone + Default>(
        &self,
        root: StmtId,
        memo: &mut [Option<T>],
        visiting: &mut HashSet<StmtId>,
        f: &mut impl FnMut(&StmtNode, &[T]) -> T,
    ) -> T {
        if let Some(cached) = memo.get(root.index()).and_then(Clone::clone) {
            return cached;
        }
        if !visiting.insert(root) {
            return T::default();
        }

        let Some(stmt) = self.stmt(root) else {
            visiting.remove(&root);
            return T::default();
        };

        let child_results: SmallVec<[T; 4]> = stmt
            .child_stmts()
            .into_iter()
            .map(|child| self.fold_stmts(child, memo, visiting, f))
            .collect();

        let result = f(stmt, &child_results);
        visiting.remove(&root);
        if let Some(slot) = memo.get_mut(root.index()) {
            *slot = Some(result.clone());
        }
        result
    }
}
