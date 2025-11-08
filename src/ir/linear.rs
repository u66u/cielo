use crate::common::ids::{EffectLabelId, FuncId, LinearExprId, LinearStmtId, SymbolId, VarId};
use crate::ir::core::{BinaryOp, Literal, StageDirective, UnaryOp};

#[derive(Clone, Debug, Default)]
pub struct LinearProgram {
    pub functions: Vec<LinearFunction>,
    pub entrypoints: Vec<FuncId>,
    exprs: Vec<LinearExprNode>,
    stmts: Vec<LinearStmtNode>,
}

impl LinearProgram {
    pub fn push_expr(&mut self, kind: LinearExpr) -> LinearExprId {
        let id = LinearExprId::new(self.exprs.len());
        self.exprs.push(LinearExprNode { kind });
        id
    }

    pub fn push_stmt(&mut self, kind: LinearStmt) -> LinearStmtId {
        let id = LinearStmtId::new(self.stmts.len());
        self.stmts.push(LinearStmtNode { kind });
        id
    }

    pub fn expr(&self, id: LinearExprId) -> Option<&LinearExprNode> {
        self.exprs.get(id.index())
    }

    pub fn stmt(&self, id: LinearStmtId) -> Option<&LinearStmtNode> {
        self.stmts.get(id.index())
    }

    pub fn exprs(&self) -> &[LinearExprNode] {
        &self.exprs
    }

    pub fn stmts(&self) -> &[LinearStmtNode] {
        &self.stmts
    }
}

#[derive(Clone, Debug)]
pub struct LinearExprNode {
    pub kind: LinearExpr,
}

#[derive(Clone, Debug)]
pub struct LinearStmtNode {
    pub kind: LinearStmt,
}

#[derive(Clone, Debug)]
pub struct LinearFunction {
    pub id: FuncId,
    pub name: SymbolId,
    pub params: Vec<VarId>,
    pub body: LinearStmtId,
}

#[derive(Clone, Debug)]
pub enum LinearExpr {
    Var(VarId),
    Literal(Literal),
    Unary {
        op: UnaryOp,
        expr: LinearExprId,
    },
    Binary {
        op: BinaryOp,
        lhs: LinearExprId,
        rhs: LinearExprId,
    },
    PureCall {
        callee: SymbolId,
        args: Vec<LinearExprId>,
    },
    MakeStruct {
        ty: SymbolId,
        fields: Vec<LinearExprId>,
    },
    MakeEnum {
        ty: SymbolId,
        variant: SymbolId,
        fields: Vec<LinearExprId>,
    },
    Error,
}

#[derive(Clone, Debug)]
pub struct LinearMatchArm {
    pub tag: SymbolId,
    pub binders: Vec<VarId>,
    pub body: LinearStmtId,
}

#[derive(Clone, Debug)]
pub enum LinearStmt {
    Return(LinearExprId),
    Let {
        binding: VarId,
        value: LinearExprId,
        next: LinearStmtId,
    },
    Val {
        binding: VarId,
        value: LinearStmtId,
        next: LinearStmtId,
    },
    Call {
        result: VarId,
        callee: SymbolId,
        args: Vec<LinearExprId>,
        next: LinearStmtId,
    },
    If {
        cond: LinearExprId,
        then_branch: LinearStmtId,
        else_branch: LinearStmtId,
    },
    Match {
        scrutinee: LinearExprId,
        arms: Vec<LinearMatchArm>,
        default: Option<LinearStmtId>,
    },
    Perform {
        result: Option<VarId>,
        effect: EffectLabelId,
        operation: SymbolId,
        args: Vec<LinearExprId>,
        next: LinearStmtId,
    },
    Handle {
        effect: EffectLabelId,
        body: LinearStmtId,
        next: Option<LinearStmtId>,
    },
    Stage {
        stage: StageDirective,
        body: LinearStmtId,
        next: Option<LinearStmtId>,
    },
    Hole,
    Error,
}
