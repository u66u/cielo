use std::hash::Hash;

use smallvec::SmallVec;

use crate::common::ids::{ExprId, LinearExprId, LinearStmtId, StmtId, SymbolId};
use crate::ir::core::{CoreProgram, ExprKind, ExprNode, Literal, StmtNode};
use crate::ir::linear::{LinearExpr, LinearExprNode, LinearProgram, LinearStmtNode};

pub trait IrStmtNode {
    type StmtId: Copy + Eq + Hash;
    type ExprId: Copy + Eq + Hash;
    fn child_stmts(&self) -> SmallVec<[Self::StmtId; 4]>;
    fn child_exprs(&self) -> SmallVec<[Self::ExprId; 4]>;
}

pub trait IrExprNode {
    type ExprId: Copy + Eq + Hash;
    fn literal(&self) -> Option<&Literal>;
    fn ctor_fields(&self) -> Option<(SymbolId, SymbolId, &[Self::ExprId])>;
}

pub trait IrProgram {
    type StmtId: Copy + Eq + Hash;
    type ExprId: Copy + Eq + Hash;
    type StmtNode: IrStmtNode<StmtId = Self::StmtId, ExprId = Self::ExprId>;
    type ExprNode: IrExprNode<ExprId = Self::ExprId>;

    fn stmt(&self, id: Self::StmtId) -> Option<&Self::StmtNode>;
    fn expr(&self, id: Self::ExprId) -> Option<&Self::ExprNode>;
    fn expr_ids(&self) -> Vec<Self::ExprId>;
}

impl IrStmtNode for StmtNode {
    type StmtId = StmtId;
    type ExprId = ExprId;

    fn child_stmts(&self) -> SmallVec<[Self::StmtId; 4]> {
        StmtNode::child_stmts(self)
    }

    fn child_exprs(&self) -> SmallVec<[Self::ExprId; 4]> {
        StmtNode::child_exprs(self)
    }
}

impl IrExprNode for ExprNode {
    type ExprId = ExprId;

    fn literal(&self) -> Option<&Literal> {
        let ExprKind::Literal(literal) = &self.kind else {
            return None;
        };
        Some(literal)
    }

    fn ctor_fields(&self) -> Option<(SymbolId, SymbolId, &[Self::ExprId])> {
        match &self.kind {
            ExprKind::MakeStruct { ty, fields } => Some((*ty, SymbolId::INVALID, fields)),
            ExprKind::MakeEnum {
                ty,
                variant,
                fields,
            } => Some((*ty, *variant, fields)),
            ExprKind::Var(_)
            | ExprKind::Literal(_)
            | ExprKind::Unary { .. }
            | ExprKind::Binary { .. }
            | ExprKind::PureCall { .. }
            | ExprKind::Error(_) => None,
        }
    }
}

impl IrProgram for CoreProgram {
    type StmtId = StmtId;
    type ExprId = ExprId;
    type StmtNode = StmtNode;
    type ExprNode = ExprNode;

    fn stmt(&self, id: Self::StmtId) -> Option<&Self::StmtNode> {
        CoreProgram::stmt(self, id)
    }

    fn expr(&self, id: Self::ExprId) -> Option<&Self::ExprNode> {
        CoreProgram::expr(self, id)
    }

    fn expr_ids(&self) -> Vec<Self::ExprId> {
        (0..self.exprs().len()).map(ExprId::new).collect()
    }
}

impl IrStmtNode for LinearStmtNode {
    type StmtId = LinearStmtId;
    type ExprId = LinearExprId;

    fn child_stmts(&self) -> SmallVec<[Self::StmtId; 4]> {
        LinearStmtNode::child_stmts(self)
    }

    fn child_exprs(&self) -> SmallVec<[Self::ExprId; 4]> {
        LinearStmtNode::child_exprs(self)
    }
}

impl IrExprNode for LinearExprNode {
    type ExprId = LinearExprId;

    fn literal(&self) -> Option<&Literal> {
        let LinearExpr::Literal(literal) = &self.kind else {
            return None;
        };
        Some(literal)
    }

    fn ctor_fields(&self) -> Option<(SymbolId, SymbolId, &[Self::ExprId])> {
        match &self.kind {
            LinearExpr::MakeStruct { ty, fields } => Some((*ty, SymbolId::INVALID, fields)),
            LinearExpr::MakeEnum {
                ty,
                variant,
                fields,
            } => Some((*ty, *variant, fields)),
            LinearExpr::Var(_)
            | LinearExpr::Literal(_)
            | LinearExpr::Unary { .. }
            | LinearExpr::Binary { .. }
            | LinearExpr::PureCall { .. }
            | LinearExpr::Error => None,
        }
    }
}

impl IrProgram for LinearProgram {
    type StmtId = LinearStmtId;
    type ExprId = LinearExprId;
    type StmtNode = LinearStmtNode;
    type ExprNode = LinearExprNode;

    fn stmt(&self, id: Self::StmtId) -> Option<&Self::StmtNode> {
        LinearProgram::stmt(self, id)
    }

    fn expr(&self, id: Self::ExprId) -> Option<&Self::ExprNode> {
        LinearProgram::expr(self, id)
    }

    fn expr_ids(&self) -> Vec<Self::ExprId> {
        (0..self.exprs().len()).map(LinearExprId::new).collect()
    }
}
