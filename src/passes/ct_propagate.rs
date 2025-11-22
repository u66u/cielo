// Pass 4/9: ct_propagate (compile-time constant propagation)
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

use std::collections::HashMap;

use crate::common::fixpoint::fixpoint;
use crate::common::ids::ExprId;
use crate::ir::core::{BinaryOp, ExprKind, Literal, OpCategory, UnaryOp};
use crate::pipeline::phases::{CtPropagated, CtPropagationTables, Monomorphized};

pub fn run(mono: Monomorphized) -> CtPropagated {
    let mut ct = CtPropagationTables::default();
    let limit = mono.program().exprs().len().saturating_add(1).max(1);
    ct.ct_cache = fixpoint(
        HashMap::new(),
        |cache| {
            let mut next = cache.clone();
            for (idx, expr) in mono.program().exprs().iter().enumerate() {
                let expr_id = ExprId::new(idx);
                if next.contains_key(&expr_id) {
                    continue;
                }
                let Some(value) = eval_expr(expr_id, expr, &next) else {
                    continue;
                };
                next.insert(expr_id, value);
            }
            next
        },
        limit,
    );

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
    match (op.category(), left, right) {
        (OpCategory::Arithmetic, Literal::Int(a), Literal::Int(b)) => match op {
            BinaryOp::Add => Some(Literal::Int(a + b)),
            BinaryOp::Sub => Some(Literal::Int(a - b)),
            BinaryOp::Mul => Some(Literal::Int(a * b)),
            BinaryOp::Div if *b != 0 => Some(Literal::Int(a / b)),
            BinaryOp::Mod if *b != 0 => Some(Literal::Int(a % b)),
            _ => None,
        },
        (OpCategory::Comparison, Literal::Int(a), Literal::Int(b)) => {
            let value = match op {
                BinaryOp::Lt => a < b,
                BinaryOp::Le => a <= b,
                BinaryOp::Gt => a > b,
                BinaryOp::Ge => a >= b,
                _ => return None,
            };
            Some(Literal::Bool(value))
        }
        (OpCategory::Equality, Literal::Int(a), Literal::Int(b)) => {
            let value = match op {
                BinaryOp::Eq => a == b,
                BinaryOp::Ne => a != b,
                _ => return None,
            };
            Some(Literal::Bool(value))
        }
        (OpCategory::Logical, Literal::Bool(a), Literal::Bool(b)) => {
            let value = match op {
                BinaryOp::And => *a && *b,
                BinaryOp::Or => *a || *b,
                _ => return None,
            };
            Some(Literal::Bool(value))
        }
        _ => None,
    }
}
