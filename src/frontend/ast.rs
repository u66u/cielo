use crate::common::diagnostics::ErrorNode;
use crate::common::ids::SymbolId;
use crate::common::span::Span;

#[derive(Clone, Debug, Default)]
pub struct Program {
    pub items: Vec<Item>,
}

#[derive(Clone, Debug)]
pub enum Item {
    Function(FunctionDecl),
    Struct(StructDecl),
    Enum(EnumDecl),
    Effect(EffectDecl),
    Error(ErrorNode),
}

#[derive(Clone, Debug)]
pub struct FunctionDecl {
    pub name: SymbolId,
    pub params: Vec<Param>,
    pub return_type: Option<TypeExpr>,
    pub effects: Vec<SymbolId>,
    pub ct_only: bool,
    pub body: BlockExpr,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Param {
    pub name: SymbolId,
    pub ty: TypeExpr,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct StructDecl {
    pub name: SymbolId,
    pub fields: Vec<FieldDecl>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct EnumDecl {
    pub name: SymbolId,
    pub variants: Vec<EnumVariantDecl>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct EnumVariantDecl {
    pub name: SymbolId,
    pub fields: Vec<TypeExpr>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct FieldDecl {
    pub name: SymbolId,
    pub ty: TypeExpr,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct EffectDecl {
    pub name: SymbolId,
    pub properties: EffectPropertyHint,
    pub operations: Vec<EffectOperationDecl>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct EffectOperationDecl {
    pub name: SymbolId,
    pub params: Vec<Param>,
    pub return_type: Option<TypeExpr>,
    pub span: Span,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EffectCapabilityHint {
    Pure,
    Diverge,
    Alloc,
    LocalState,
    SharedState,
    Io,
    Ffi,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EffectPropertyHint {
    pub capability: EffectCapabilityHint,
    pub discardable: bool,
    pub commutative: bool,
    pub opaque_for_staging: bool,
    pub ct_only: bool,
}

impl Default for EffectPropertyHint {
    fn default() -> Self {
        Self {
            capability: EffectCapabilityHint::LocalState,
            discardable: false,
            commutative: false,
            opaque_for_staging: false,
            ct_only: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct HandleClause {
    pub operation: SymbolId,
    pub params: Vec<SymbolId>,
    pub body: BlockExpr,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct MatchClause {
    pub tag: SymbolId,
    pub binders: Vec<SymbolId>,
    pub body: BlockExpr,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct TypeExpr {
    pub kind: TypeExprKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum TypeExprKind {
    Builtin(BuiltinType),
    Path { name: SymbolId, args: Vec<TypeExpr> },
    Unit,
    Error(ErrorNode),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BuiltinType {
    Bool,
    Int,
    Float,
    Char,
    String,
}

#[derive(Clone, Debug)]
pub struct BlockExpr {
    pub statements: Vec<Stmt>,
    pub tail: Option<Box<Expr>>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum Stmt {
    Let {
        name: SymbolId,
        ty: Option<TypeExpr>,
        value: Expr,
        span: Span,
    },
    Perform {
        effect: SymbolId,
        operation: SymbolId,
        args: Vec<Expr>,
        span: Span,
    },
    Expr {
        value: Expr,
        span: Span,
    },
    Error(ErrorNode),
}

#[derive(Clone, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    Int(i64),
    Bool(bool),
    String(String),
    Var(SymbolId),
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    If {
        cond: Box<Expr>,
        then_branch: BlockExpr,
        else_branch: Option<BlockExpr>,
    },
    Match {
        scrutinee: Box<Expr>,
        clauses: Vec<MatchClause>,
        default: Option<BlockExpr>,
    },
    Block(BlockExpr),
    StageBlock {
        stage: StageMarker,
        block: BlockExpr,
    },
    Handle {
        body: Box<Expr>,
        effect: SymbolId,
        clauses: Vec<HandleClause>,
    },
    Error(ErrorNode),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StageMarker {
    Comptime,
    Runtime,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BinOp {
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

impl BinOp {
    pub const fn precedence(self) -> u8 {
        match self {
            Self::Or => 1,
            Self::And => 2,
            Self::Eq | Self::Ne => 3,
            Self::Lt | Self::Le | Self::Gt | Self::Ge => 4,
            Self::Add | Self::Sub => 5,
            Self::Mul | Self::Div | Self::Mod => 6,
        }
    }
}
