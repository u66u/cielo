use crate::common::ids::{EffectLabelId, FuncId, SymbolId, VarId};
use crate::ir::core::{BinaryOp, Literal, StageDirective, UnaryOp};

#[derive(Clone, Debug, Default)]
pub struct LinearProgram {
    pub functions: Vec<LinearFunction>,
    pub entrypoints: Vec<FuncId>,
}

#[derive(Clone, Debug)]
pub struct LinearFunction {
    pub id: FuncId,
    pub name: SymbolId,
    pub params: Vec<VarId>,
    pub body: LinearStmt,
}

#[derive(Clone, Debug)]
pub enum LinearExpr {
    Var(VarId),
    Literal(Literal),
    Unary {
        op: UnaryOp,
        expr: Box<LinearExpr>,
    },
    Binary {
        op: BinaryOp,
        lhs: Box<LinearExpr>,
        rhs: Box<LinearExpr>,
    },
    PureCall {
        callee: SymbolId,
        args: Vec<LinearExpr>,
    },
    MakeStruct {
        ty: SymbolId,
        fields: Vec<LinearExpr>,
    },
    MakeEnum {
        ty: SymbolId,
        variant: SymbolId,
        fields: Vec<LinearExpr>,
    },
    Error,
}

#[derive(Clone, Debug)]
pub struct LinearMatchArm {
    pub tag: SymbolId,
    pub binders: Vec<VarId>,
    pub body: Box<LinearStmt>,
}

#[derive(Clone, Debug)]
pub enum LinearStmt {
    Return(LinearExpr),
    Let {
        binding: VarId,
        value: LinearExpr,
        next: Box<LinearStmt>,
    },
    Val {
        binding: VarId,
        value: Box<LinearStmt>,
        next: Box<LinearStmt>,
    },
    Call {
        result: VarId,
        callee: SymbolId,
        args: Vec<LinearExpr>,
        next: Box<LinearStmt>,
    },
    If {
        cond: LinearExpr,
        then_branch: Box<LinearStmt>,
        else_branch: Box<LinearStmt>,
    },
    Match {
        scrutinee: LinearExpr,
        arms: Vec<LinearMatchArm>,
        default: Option<Box<LinearStmt>>,
    },
    Perform {
        result: Option<VarId>,
        effect: EffectLabelId,
        operation: SymbolId,
        args: Vec<LinearExpr>,
        next: Box<LinearStmt>,
    },
    Handle {
        effect: EffectLabelId,
        body: Box<LinearStmt>,
        next: Option<Box<LinearStmt>>,
    },
    Stage {
        stage: StageDirective,
        body: Box<LinearStmt>,
        next: Option<Box<LinearStmt>>,
    },
    Hole,
    Error,
}
