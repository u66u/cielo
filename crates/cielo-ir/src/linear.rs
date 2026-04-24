use crate::core::{BinaryOp, Literal, StageDirective, UnaryOp};
use cielo_base::{EffectLabelId, LinearExprId, LinearFuncId, LinearStmtId, Span, SymbolId, VarId};
use smallvec::{SmallVec, smallvec};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CallConvention {
    Pure,
    Direct,
    Control,
}

#[derive(Clone, Debug, Default)]
pub struct LinearProgram {
    pub functions: Vec<LinearFunction>,
    pub entrypoints: Vec<LinearFuncId>,
    /// One entry per `handle` site, in lowering order.
    pub handler_sites: Vec<HandlerSite>,
    exprs: Vec<LinearExprNode>,
    stmts: Vec<LinearStmtNode>,
}

/// Outcome of trying to compile a `handle` site away. Only `Inlined` and
/// `Dead` leave no handler behind at runtime; the rest are failures that used
/// to be indistinguishable from success in the emitted C.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HandlerOutcome {
    Inlined,
    Dead,
    EscapedThroughCall,
    LeakedAfterInlining,
    UnresolvedHandler,
}

impl HandlerOutcome {
    pub fn erased(self) -> bool {
        matches!(self, Self::Inlined | Self::Dead)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inlined => "inlined",
            Self::Dead => "dead, eliminated",
            Self::EscapedThroughCall => "not erased: effect performed in a callee",
            Self::LeakedAfterInlining => "not erased: perform survived inlining",
            Self::UnresolvedHandler => "not erased: handler could not be resolved",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HandlerSite {
    pub effect: EffectLabelId,
    pub span: Span,
    pub outcome: HandlerOutcome,
}

impl LinearProgram {
    pub fn push_expr(&mut self, kind: LinearExpr) -> LinearExprId {
        let id = LinearExprId::new(self.exprs.len());
        self.exprs.push(LinearExprNode { kind });
        id
    }

    pub fn push_stmt(&mut self, kind: LinearStmt) -> LinearStmtId {
        self.push_stmt_at(kind, Span::synthetic())
    }

    pub fn push_stmt_at(&mut self, kind: LinearStmt, span: Span) -> LinearStmtId {
        let id = LinearStmtId::new(self.stmts.len());
        self.stmts.push(LinearStmtNode { kind, span });
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
    pub span: Span,
}

impl LinearStmtNode {
    pub fn child_stmts(&self) -> SmallVec<[LinearStmtId; 4]> {
        match &self.kind {
            LinearStmt::Return(_) | LinearStmt::Hole | LinearStmt::Error => SmallVec::new(),
            LinearStmt::Let { next, .. }
            | LinearStmt::PureCall { next, .. }
            | LinearStmt::DirectCall { next, .. }
            | LinearStmt::ControlCall { next, .. }
            | LinearStmt::Perform { next, .. } => smallvec![*next],
            LinearStmt::Val { value, next, .. } => smallvec![*value, *next],
            LinearStmt::If {
                then_branch,
                else_branch,
                ..
            } => smallvec![*then_branch, *else_branch],
            LinearStmt::Match { arms, default, .. } => {
                let mut children: SmallVec<[LinearStmtId; 4]> =
                    arms.iter().map(|arm| arm.body).collect();
                if let Some(default_stmt) = default {
                    children.push(*default_stmt);
                }
                children
            }
            LinearStmt::Handle { body, next, .. } | LinearStmt::Stage { body, next, .. } => {
                let mut children = smallvec![*body];
                if let Some(next_stmt) = next {
                    children.push(*next_stmt);
                }
                children
            }
        }
    }

    pub fn child_exprs(&self) -> SmallVec<[LinearExprId; 4]> {
        match &self.kind {
            LinearStmt::Return(expr) => smallvec![*expr],
            LinearStmt::Let { value, .. } => smallvec![*value],
            LinearStmt::If { cond, .. }
            | LinearStmt::Match {
                scrutinee: cond, ..
            } => {
                smallvec![*cond]
            }
            LinearStmt::PureCall { args, .. }
            | LinearStmt::DirectCall { args, .. }
            | LinearStmt::ControlCall { args, .. }
            | LinearStmt::Perform { args, .. } => args.iter().copied().collect(),
            LinearStmt::Val { .. }
            | LinearStmt::Handle { .. }
            | LinearStmt::Stage { .. }
            | LinearStmt::Hole
            | LinearStmt::Error => SmallVec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct LinearFunction {
    pub id: LinearFuncId,
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
        callee_fn: LinearFuncId,
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
    PureCall {
        result: VarId,
        callee: SymbolId,
        callee_fn: LinearFuncId,
        args: Vec<LinearExprId>,
        next: LinearStmtId,
    },
    DirectCall {
        result: VarId,
        callee: SymbolId,
        callee_fn: LinearFuncId,
        args: Vec<LinearExprId>,
        next: LinearStmtId,
    },
    ControlCall {
        result: VarId,
        callee: SymbolId,
        callee_fn: LinearFuncId,
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
