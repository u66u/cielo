//! Generic traversal shared by every IR level.
//!
//! Each node kind declares its own operands next to its definition
//! (`ExprKind::operands`, `LinearExpr::operands`, `CfgExpr::operands`,
//! `CfgTerminator::operands`). Everything in this module is written once against
//! those declarations, so adding a node kind does not mean adding an arm to
//! fifteen walkers -- only to the node's own listing.

use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use smallvec::SmallVec;

use crate::cfg::{CfgExpr, CfgExprNode, CfgProgram};
use crate::core::{CoreProgram, ExprKind, ExprNode, Literal, StmtNode};
use crate::linear::{LinearExpr, LinearExprNode, LinearProgram, LinearStmtNode};
use crate::ownership::OperandRole;
use cielo_base::densemap::DenseId;
use cielo_base::{CfgExprId, ExprId, LinearExprId, LinearStmtId, StmtId, SymbolId};

pub trait IrStmtNode {
    type StmtId: Copy + Eq + Hash;
    type ExprId: Copy + Eq + Hash;
    fn child_stmts(&self) -> SmallVec<[Self::StmtId; 4]>;
    fn child_exprs(&self) -> SmallVec<[Self::ExprId; 4]>;
}

pub trait IrExprNode {
    type ExprId: DenseId + Eq + Hash;
    fn operands(&self) -> SmallVec<[(Self::ExprId, OperandRole); 4]>;
    fn literal(&self) -> Option<&Literal>;
    fn ctor_fields(&self) -> Option<(SymbolId, SymbolId, &[Self::ExprId])>;
    /// Hashes the node's tag and payload but not its children, so that a
    /// fingerprint can fold children in separately and memoize.
    fn hash_own<H: Hasher>(&self, hasher: &mut H);

    fn child_exprs(&self) -> SmallVec<[Self::ExprId; 4]> {
        self.operands().into_iter().map(|(expr, _)| expr).collect()
    }
}

/// An arena of expressions addressed by dense ids. Split out from [`IrProgram`]
/// so that CFG -- which has blocks and terminators instead of statements -- can
/// share the expression walkers.
pub trait IrExprArena {
    type ExprId: DenseId + Eq + Hash;
    type ExprNode: IrExprNode<ExprId = Self::ExprId>;

    fn expr(&self, id: Self::ExprId) -> Option<&Self::ExprNode>;
    fn expr_count(&self) -> usize;

    fn expr_ids(&self) -> Vec<Self::ExprId> {
        (0..self.expr_count()).map(DenseId::from_index).collect()
    }
}

pub trait IrProgram: IrExprArena {
    type StmtId: Copy + Eq + Hash;
    type StmtNode: IrStmtNode<StmtId = Self::StmtId, ExprId = <Self as IrExprArena>::ExprId>;

    fn stmt(&self, id: Self::StmtId) -> Option<&Self::StmtNode>;
}

/// What a visitor wants a walk to do next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Walk {
    Descend,
    /// Leave this node's operands unvisited but keep walking the rest.
    Skip,
    /// Abandon the whole traversal.
    Halt,
}

/// Visits `root` and everything reachable from it, parents before operands and
/// operands left to right.
///
/// Each id is visited at most once. Inlining makes one arena node reachable from
/// several parents, so a visitor here counts *distinct nodes*; use
/// [`walk_operands`] where every occurrence has to be seen separately.
///
/// Returns `false` if a visitor halted the walk.
pub fn walk_exprs<A: IrExprArena>(
    arena: &A,
    root: A::ExprId,
    visit: &mut impl FnMut(A::ExprId, &A::ExprNode) -> Walk,
) -> bool {
    let mut seen = HashSet::new();
    walk_exprs_from(arena, root, &mut seen, visit)
}

/// [`walk_exprs`] with a caller-owned visited set, so one traversal can span
/// several roots without revisiting shared sub-expressions.
pub fn walk_exprs_from<A: IrExprArena>(
    arena: &A,
    root: A::ExprId,
    seen: &mut HashSet<A::ExprId>,
    visit: &mut impl FnMut(A::ExprId, &A::ExprNode) -> Walk,
) -> bool {
    let mut stack = vec![root];
    while let Some(expr_id) = stack.pop() {
        if !seen.insert(expr_id) {
            continue;
        }
        let Some(node) = arena.expr(expr_id) else {
            continue;
        };
        match visit(expr_id, node) {
            Walk::Halt => return false,
            Walk::Skip => continue,
            Walk::Descend => {}
        }
        // Pushed in reverse so the stack pops operands left to right, matching
        // the recursive walkers this replaced.
        stack.extend(node.child_exprs().into_iter().rev());
    }
    true
}

/// True when `predicate` holds for `root` or for anything reachable from it.
pub fn any_expr<A: IrExprArena>(
    arena: &A,
    root: A::ExprId,
    predicate: &mut impl FnMut(A::ExprId, &A::ExprNode) -> bool,
) -> bool {
    let mut seen = HashSet::new();
    any_expr_from(arena, root, &mut seen, predicate)
}

/// [`any_expr`] with a caller-owned visited set. Ids already in `seen` are
/// treated as answered, so the search is only sound for a predicate whose
/// earlier answer was `false`.
pub fn any_expr_from<A: IrExprArena>(
    arena: &A,
    root: A::ExprId,
    seen: &mut HashSet<A::ExprId>,
    predicate: &mut impl FnMut(A::ExprId, &A::ExprNode) -> bool,
) -> bool {
    !walk_exprs_from(arena, root, seen, &mut |expr_id, node| {
        if predicate(expr_id, node) {
            Walk::Halt
        } else {
            Walk::Descend
        }
    })
}

/// Visits `root` and every reachable operand together with the role its
/// immediate parent gives it; `root` itself is visited with `role`.
///
/// Unlike [`walk_exprs`] a sub-expression shared by two parents is visited once
/// per parent, because two owners need two references. Repetition along a single
/// path is still cut, so a malformed arena cannot make this diverge.
pub fn walk_operands<A: IrExprArena>(
    arena: &A,
    root: A::ExprId,
    role: OperandRole,
    visit: &mut impl FnMut(A::ExprId, &A::ExprNode, OperandRole),
) {
    let mut path = HashSet::new();
    walk_operands_from(arena, root, role, &mut path, visit);
}

fn walk_operands_from<A: IrExprArena>(
    arena: &A,
    expr_id: A::ExprId,
    role: OperandRole,
    path: &mut HashSet<A::ExprId>,
    visit: &mut impl FnMut(A::ExprId, &A::ExprNode, OperandRole),
) {
    if !path.insert(expr_id) {
        return;
    }
    if let Some(node) = arena.expr(expr_id) {
        visit(expr_id, node, role);
        for (operand, operand_role) in node.operands() {
            walk_operands_from(arena, operand, operand_role, path, visit);
        }
    }
    path.remove(&expr_id);
}

/// Structural fingerprint of every expression in the arena, indexed by
/// expression index.
///
/// Two nodes fingerprint alike when their payloads and their whole operand trees
/// agree; arena positions never enter the hash. A node reachable only through a
/// cycle fingerprints as `0` instead of diverging.
pub fn fingerprint_exprs<A: IrExprArena>(arena: &A) -> Vec<u64> {
    let count = arena.expr_count();
    let mut memo = vec![None; count];
    let mut visiting = vec![false; count];
    for index in 0..count {
        expr_fingerprint(arena, DenseId::from_index(index), &mut memo, &mut visiting);
    }
    memo.into_iter().map(Option::unwrap_or_default).collect()
}

fn expr_fingerprint<A: IrExprArena>(
    arena: &A,
    expr_id: A::ExprId,
    memo: &mut [Option<u64>],
    visiting: &mut [bool],
) -> u64 {
    let index = expr_id.index();
    if index >= memo.len() {
        return 0;
    }
    if let Some(cached) = memo[index] {
        return cached;
    }
    if visiting[index] {
        return 0;
    }
    visiting[index] = true;

    let value = match arena.expr(expr_id) {
        Some(node) => {
            let mut hasher = DefaultHasher::new();
            node.hash_own(&mut hasher);
            for operand in node.child_exprs() {
                expr_fingerprint(arena, operand, memo, visiting).hash(&mut hasher);
            }
            hasher.finish()
        }
        None => 0,
    };

    visiting[index] = false;
    memo[index] = Some(value);
    value
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

    fn operands(&self) -> SmallVec<[(Self::ExprId, OperandRole); 4]> {
        self.kind.operands()
    }

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
            | ExprKind::BuiltinCall { .. }
            | ExprKind::Field { .. }
            | ExprKind::Error(_) => None,
        }
    }

    /// Core keeps spans, and staging snapshots key on them, so they are part of
    /// a Core node's identity even though the other IRs have no equivalent.
    fn hash_own<H: Hasher>(&self, hasher: &mut H) {
        self.span.start.hash(hasher);
        self.span.end.hash(hasher);
        self.kind.hash_own(hasher);
    }
}

impl IrExprArena for CoreProgram {
    type ExprId = ExprId;
    type ExprNode = ExprNode;

    fn expr(&self, id: Self::ExprId) -> Option<&Self::ExprNode> {
        CoreProgram::expr(self, id)
    }

    fn expr_count(&self) -> usize {
        self.exprs().len()
    }
}

impl IrProgram for CoreProgram {
    type StmtId = StmtId;
    type StmtNode = StmtNode;

    fn stmt(&self, id: Self::StmtId) -> Option<&Self::StmtNode> {
        CoreProgram::stmt(self, id)
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

    fn operands(&self) -> SmallVec<[(Self::ExprId, OperandRole); 4]> {
        self.kind.operands()
    }

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
            | LinearExpr::BuiltinCall { .. }
            | LinearExpr::Field { .. }
            | LinearExpr::Error => None,
        }
    }

    fn hash_own<H: Hasher>(&self, hasher: &mut H) {
        self.kind.hash_own(hasher);
    }
}

impl IrExprArena for LinearProgram {
    type ExprId = LinearExprId;
    type ExprNode = LinearExprNode;

    fn expr(&self, id: Self::ExprId) -> Option<&Self::ExprNode> {
        LinearProgram::expr(self, id)
    }

    fn expr_count(&self) -> usize {
        self.exprs().len()
    }
}

impl IrProgram for LinearProgram {
    type StmtId = LinearStmtId;
    type StmtNode = LinearStmtNode;

    fn stmt(&self, id: Self::StmtId) -> Option<&Self::StmtNode> {
        LinearProgram::stmt(self, id)
    }
}

impl IrExprNode for CfgExprNode {
    type ExprId = CfgExprId;

    fn operands(&self) -> SmallVec<[(Self::ExprId, OperandRole); 4]> {
        self.kind.operands()
    }

    fn literal(&self) -> Option<&Literal> {
        let CfgExpr::Literal(literal) = &self.kind else {
            return None;
        };
        Some(literal)
    }

    fn ctor_fields(&self) -> Option<(SymbolId, SymbolId, &[Self::ExprId])> {
        match &self.kind {
            CfgExpr::MakeStruct { ty, fields } => Some((*ty, SymbolId::INVALID, fields)),
            CfgExpr::MakeEnum {
                ty,
                variant,
                fields,
            } => Some((*ty, *variant, fields)),
            CfgExpr::Value(_)
            | CfgExpr::Literal(_)
            | CfgExpr::Unary { .. }
            | CfgExpr::Binary { .. }
            | CfgExpr::PureCall { .. }
            | CfgExpr::Field { .. }
            | CfgExpr::Error => None,
        }
    }

    /// `source` is a Linear arena index, so it stays out of the hash.
    fn hash_own<H: Hasher>(&self, hasher: &mut H) {
        self.kind.hash_own(hasher);
    }
}

impl IrExprArena for CfgProgram {
    type ExprId = CfgExprId;
    type ExprNode = CfgExprNode;

    fn expr(&self, id: Self::ExprId) -> Option<&Self::ExprNode> {
        CfgProgram::expr(self, id)
    }

    fn expr_count(&self) -> usize {
        self.exprs().len()
    }
}
