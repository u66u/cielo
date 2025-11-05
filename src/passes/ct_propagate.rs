// Pass 4/8: ct_propagate (compile-time constant propagation)
//
// Inputs:
// - Monomorphized Core program
//
// Outputs:
// - CtPropagationTables (`ct_cache`, branch decisions, file deps)
//
// Invariants:
// - Only pure Expr nodes are evaluated
// - Cache entries are deterministic literals keyed by ExprId
//
// Diagnostics:
// - None in v0
//
// Complexity:
// - O(expr_count * fixpoint_iters), with small bounded iter count in practice

use crate::common::ids::ExprId;
use crate::ir::core::{BinaryOp, ExprKind, Literal, UnaryOp};
use crate::pipeline::phases::{CtPropagated, CtPropagationTables, Monomorphized};

pub fn run(mono: Monomorphized) -> CtPropagated {
    let mut ct = CtPropagationTables::default();

    let mut changed = true;
    while changed {
        changed = false;
        for (idx, expr) in mono.program.exprs().iter().enumerate() {
            let expr_id = ExprId::new(idx);
            if ct.ct_cache.contains_key(&expr_id) {
                continue;
            }
            let Some(value) = eval_expr(expr_id, expr, &ct.ct_cache) else {
                continue;
            };
            ct.ct_cache.insert(expr_id, value);
            changed = true;
        }
    }

    mono.into_ct_propagated(ct)
}

fn eval_expr(
    expr_id: ExprId,
    expr: &crate::ir::core::ExprNode,
    cache: &std::collections::HashMap<ExprId, Literal>,
) -> Option<Literal> {
    let _ = expr_id;
    match &expr.kind {
        ExprKind::Literal(value) => Some(value.clone()),
        ExprKind::Unary { op, expr } => {
            let value = cache.get(expr)?;
            eval_unary(*op, value)
        }
        ExprKind::Binary { op, lhs, rhs } => {
            let left = cache.get(lhs)?;
            let right = cache.get(rhs)?;
            eval_binary(*op, left, right)
        }
        _ => None,
    }
}

fn eval_unary(op: UnaryOp, value: &Literal) -> Option<Literal> {
    match (op, value) {
        (UnaryOp::Neg, Literal::Int(v)) => Some(Literal::Int(-v)),
        (UnaryOp::Neg, Literal::Float(v)) => Some(Literal::Float(-v)),
        (UnaryOp::Not, Literal::Bool(v)) => Some(Literal::Bool(!v)),
        _ => None,
    }
}

fn eval_binary(op: BinaryOp, left: &Literal, right: &Literal) -> Option<Literal> {
    match (op, left, right) {
        (BinaryOp::Add, Literal::Int(a), Literal::Int(b)) => Some(Literal::Int(a + b)),
        (BinaryOp::Sub, Literal::Int(a), Literal::Int(b)) => Some(Literal::Int(a - b)),
        (BinaryOp::Mul, Literal::Int(a), Literal::Int(b)) => Some(Literal::Int(a * b)),
        (BinaryOp::Div, Literal::Int(a), Literal::Int(b)) if *b != 0 => Some(Literal::Int(a / b)),
        (BinaryOp::Mod, Literal::Int(a), Literal::Int(b)) if *b != 0 => Some(Literal::Int(a % b)),
        (BinaryOp::Eq, Literal::Int(a), Literal::Int(b)) => Some(Literal::Bool(a == b)),
        (BinaryOp::Ne, Literal::Int(a), Literal::Int(b)) => Some(Literal::Bool(a != b)),
        (BinaryOp::Lt, Literal::Int(a), Literal::Int(b)) => Some(Literal::Bool(a < b)),
        (BinaryOp::Le, Literal::Int(a), Literal::Int(b)) => Some(Literal::Bool(a <= b)),
        (BinaryOp::Gt, Literal::Int(a), Literal::Int(b)) => Some(Literal::Bool(a > b)),
        (BinaryOp::Ge, Literal::Int(a), Literal::Int(b)) => Some(Literal::Bool(a >= b)),
        (BinaryOp::And, Literal::Bool(a), Literal::Bool(b)) => Some(Literal::Bool(*a && *b)),
        (BinaryOp::Or, Literal::Bool(a), Literal::Bool(b)) => Some(Literal::Bool(*a || *b)),
        _ => None,
    }
}
