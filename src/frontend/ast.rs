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

#[derive(Clone, Debug)]
pub struct TypeExpr {
    pub kind: TypeExprKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum TypeExprKind {
    Path { name: SymbolId, args: Vec<TypeExpr> },
    Unit,
    Error(ErrorNode),
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
    Expr {
        value: Expr,
        span: Span,
    },
    Error(ErrorNode),
}
