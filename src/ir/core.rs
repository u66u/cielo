use crate::common::diagnostics::ErrorNode;
use crate::common::ids::{
    EffectLabelId, ExprId, FuncId, HandlerId, StmtId, SymbolId, TypeId, VarId,
};
use crate::common::span::Span;
use crate::sema::effect::SortedEffectRow;

#[derive(Clone, PartialEq, Debug)]
pub enum Literal {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    Char(char),
    String(String),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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

#[derive(Clone, Debug)]
pub struct ExprNode {
    pub span: Span,
    pub kind: ExprKind,
}

#[derive(Clone, Debug)]
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
    MakeStruct {
        ty: TypeId,
        fields: Vec<ExprId>,
    },
    MakeEnum {
        ty: TypeId,
        variant: SymbolId,
        fields: Vec<ExprId>,
    },
    Error(ErrorNode),
}

#[derive(Clone, Debug)]
pub struct StmtNode {
    pub span: Span,
    pub kind: StmtKind,
}

#[derive(Clone, Debug)]
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
    Handle {
        handler: HandlerId,
        body: StmtId,
        next: Option<StmtId>,
    },
    Hole {
        ty: TypeId,
    },
    Error(ErrorNode),
}

#[derive(Clone, Debug)]
pub struct MatchArm {
    pub tag: SymbolId,
