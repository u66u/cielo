use crate::builtins::Builtin;
use crate::core::{BinaryOp, Literal, StageDirective, UnaryOp};
use crate::ownership::OperandRole;
use cielo_base::{EffectLabelId, LinearExprId, LinearFuncId, LinearStmtId, Span, SymbolId, VarId};
use smallvec::{SmallVec, smallvec};
use std::hash::{Hash, Hasher};

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
/// `Dead` leave no handler behind at runtime; `Residual` leaves a working
/// runtime dispatcher, and the rest are failures that used to be
/// indistinguishable from success in the emitted C.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HandlerOutcome {
    Inlined,
    Dead,
    /// Erasure did not reach every perform, so the site kept a clause table.
    /// Not a failure: the program still runs, just through a dispatch.
    Residual,
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
            Self::Residual => "not erased: runtime clause table",
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
            LinearStmt::Stage { body, next, .. } => {
                let mut children = smallvec![*body];
                if let Some(next_stmt) = next {
                    children.push(*next_stmt);
                }
                children
            }
            LinearStmt::Handle {
                clauses,
                body,
                next,
                ..
            } => {
                let mut children = smallvec![*body];
                if let Some(next_stmt) = next {
                    children.push(*next_stmt);
                }
                children.extend(clauses.iter().map(|clause| clause.body));
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
    BuiltinCall {
        builtin: Builtin,
        args: Vec<LinearExprId>,
    },
    Field {
        base: LinearExprId,
        index: u32,
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

impl LinearExpr {
    /// See [`crate::core::ExprKind::operands`]: the single per-node listing that
    /// every Linear expression traversal is derived from.
    pub fn operands(&self) -> SmallVec<[(LinearExprId, OperandRole); 4]> {
        match self {
            Self::Var(_) | Self::Literal(_) | Self::Error => SmallVec::new(),
            Self::Unary { expr: operand, .. } | Self::Field { base: operand, .. } => {
                smallvec![(*operand, OperandRole::Read)]
            }
            Self::Binary { lhs, rhs, .. } => {
                smallvec![(*lhs, OperandRole::Read), (*rhs, OperandRole::Read)]
            }
            Self::BuiltinCall { args: operands, .. }
            | Self::PureCall { args: operands, .. }
            | Self::MakeStruct {
                fields: operands, ..
            }
            | Self::MakeEnum {
                fields: operands, ..
            } => operands
                .iter()
                .map(|operand| (*operand, OperandRole::Owned))
                .collect(),
        }
    }

    pub fn child_exprs(&self) -> SmallVec<[LinearExprId; 4]> {
        self.operands().into_iter().map(|(expr, _)| expr).collect()
    }

    pub const fn tag(&self) -> &'static str {
        match self {
            Self::Literal(_) => "lit",
            Self::Var(_) => "var",
            Self::Unary { .. } => "un",
            Self::Field { .. } => "field",
            Self::Binary { .. } => "bin",
            Self::PureCall { .. } => "call",
            Self::BuiltinCall { .. } => "builtin",
            Self::MakeStruct { .. } => "mk_struct",
            Self::MakeEnum { .. } => "mk_enum",
            Self::Error => "err",
        }
    }

    pub fn hash_own<H: Hasher>(&self, hasher: &mut H) {
        self.tag().hash(hasher);
        match self {
            Self::Var(var) => var.as_u32().hash(hasher),
            Self::Literal(literal) => literal.hash_structural(hasher),
            Self::Unary { op, .. } => std::mem::discriminant(op).hash(hasher),
            Self::Binary { op, .. } => std::mem::discriminant(op).hash(hasher),
            Self::Field { index, .. } => index.hash(hasher),
            Self::PureCall { callee, .. } => callee.as_u32().hash(hasher),
            // The name, not the discriminant: fingerprints reach snapshot
            // files, so reordering the enum must not change them.
            Self::BuiltinCall { builtin, .. } => builtin.name().hash(hasher),
            Self::MakeStruct { ty, .. } => ty.as_u32().hash(hasher),
            Self::MakeEnum { ty, variant, .. } => {
                ty.as_u32().hash(hasher);
                variant.as_u32().hash(hasher);
            }
            Self::Error => {}
        }
    }
}

#[derive(Clone, Debug)]
pub struct LinearMatchArm {
    pub tag: SymbolId,
    pub binders: Vec<VarId>,
    pub body: LinearStmtId,
}

/// A handler clause that survives erasure and runs at dispatch time.
///
/// `body` yields the *resumption argument*, not the clause's own value: the
/// dispatcher returns it to the perform site, which is what lets a residual
/// clause exist without reifying a continuation. Only tail-resumptive clauses
/// can be expressed this way, so linearize admits no others.
#[derive(Clone, Debug)]
pub struct LinearHandlerClause {
    pub operation: SymbolId,
    pub params: Vec<VarId>,
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
        /// Empty when inlining discharged every perform, which is the whole
        /// point of the erasure path: no table, no dispatch.
        clauses: Vec<LinearHandlerClause>,
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
