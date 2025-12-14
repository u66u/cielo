// Pass 4/9: ct_propagate (compile-time constant propagation)
//
// Inputs:
// - Monomorphized Core program
//
// Outputs:
// - CtPropagationTables (`ct_cache`, branch decisions, file deps, cache key)
//
// Invariants:
// - Only pure Expr nodes are evaluated
// - Cache entries are deterministic literals keyed by ExprId
// - Integer evaluation follows target bit width semantics
//
// Diagnostics:
// - None in v0
//
// Complexity:
// - O(expr_count * fixpoint_iters), with small bounded iter count in practice

use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::Path;

use crate::common::fixpoint::fixpoint;
use crate::common::ids::ExprId;
use crate::ir::core::{BinaryOp, CoreProgram, ExprKind, Literal, OpCategory, UnaryOp};
use crate::pipeline::compiler::{Endianness, TargetSpec};
use crate::pipeline::phases::{
    CtCacheKey, CtFileDep, CtPropagated, CtPropagationTables, Monomorphized,
};
use crate::sema::effect::EffectFlags;

const EVALUATOR_POLICY: &str = "v1-int-wrap";

pub fn run(mono: Monomorphized, target: TargetSpec) -> CtPropagated {
    let mut ct = CtPropagationTables::default();
    ct.cache_key = build_cache_key(target);
    ct.file_deps = collect_file_deps(mono.program(), mono.sema());

    let limit = mono.program().exprs().len().saturating_add(1).max(1);
    let cache = fixpoint(
        HashMap::new(),
        |cache| {
            let mut next = cache.clone();
            for (idx, expr) in mono.program().exprs().iter().enumerate() {
                let expr_id = ExprId::new(idx);
                if next.contains_key(&expr_id) {
                    continue;
                }
                let Some(value) = eval_expr(expr_id, expr, &next, target) else {
                    continue;
                };
                next.insert(expr_id, value);
            }
            next
        },
        limit,
    );
    ct.ct_cache = cache.into_iter().collect();

    ct.branch_decisions = ct
        .ct_cache
        .iter()
        .filter_map(|(expr_id, literal)| match literal {
            Literal::Bool(true) => Some((expr_id, crate::pipeline::phases::BranchDecision::LiveTrue)),
            Literal::Bool(false) => {
                Some((expr_id, crate::pipeline::phases::BranchDecision::LiveFalse))
            }
            _ => None,
        })
        .collect();

    mono.into_ct_propagated(ct)
}

fn eval_expr(
    expr_id: ExprId,
    expr: &crate::ir::core::ExprNode,
    cache: &std::collections::HashMap<ExprId, Literal>,
    target: TargetSpec,
) -> Option<Literal> {
    let _ = expr_id;
    match &expr.kind {
        ExprKind::Literal(value) => Some(value.clone()),
        ExprKind::Unary { op, expr } => {
            let value = cache.get(expr)?;
            eval_unary(*op, value, target)
        }
        ExprKind::Binary { op, lhs, rhs } => {
            let left = cache.get(lhs)?;
            let right = cache.get(rhs)?;
            eval_binary(*op, left, right, target)
        }
        _ => None,
    }
}

fn eval_unary(op: UnaryOp, value: &Literal, target: TargetSpec) -> Option<Literal> {
    match (op, value) {
        (UnaryOp::Neg, Literal::Int(v)) => {
            Some(Literal::Int(normalize_int(v.wrapping_neg(), target)))
        }
        (UnaryOp::Neg, Literal::Float(v)) => Some(Literal::Float(-v)),
        (UnaryOp::Not, Literal::Bool(v)) => Some(Literal::Bool(!v)),
        _ => None,
    }
}

fn eval_binary(
    op: BinaryOp,
    left: &Literal,
    right: &Literal,
    target: TargetSpec,
) -> Option<Literal> {
    match (op.category(), left, right) {
        (OpCategory::Arithmetic, Literal::Int(a), Literal::Int(b)) => {
            let lhs = normalize_int(*a, target);
            let rhs = normalize_int(*b, target);
            match op {
                BinaryOp::Add => Some(Literal::Int(normalize_int(lhs.wrapping_add(rhs), target))),
                BinaryOp::Sub => Some(Literal::Int(normalize_int(lhs.wrapping_sub(rhs), target))),
                BinaryOp::Mul => Some(Literal::Int(normalize_int(lhs.wrapping_mul(rhs), target))),
                BinaryOp::Div if rhs != 0 => lhs
                    .checked_div(rhs)
                    .map(|value| Literal::Int(normalize_int(value, target))),
                BinaryOp::Mod if rhs != 0 => lhs
                    .checked_rem(rhs)
                    .map(|value| Literal::Int(normalize_int(value, target))),
                _ => None,
            }
        }
        (OpCategory::Comparison, Literal::Int(a), Literal::Int(b)) => {
            let lhs = normalize_int(*a, target);
            let rhs = normalize_int(*b, target);
            let value = match op {
                BinaryOp::Lt => lhs < rhs,
                BinaryOp::Le => lhs <= rhs,
                BinaryOp::Gt => lhs > rhs,
                BinaryOp::Ge => lhs >= rhs,
                _ => return None,
            };
            Some(Literal::Bool(value))
        }
        (OpCategory::Equality, Literal::Int(a), Literal::Int(b)) => {
            let lhs = normalize_int(*a, target);
            let rhs = normalize_int(*b, target);
            let value = match op {
                BinaryOp::Eq => lhs == rhs,
                BinaryOp::Ne => lhs != rhs,
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

fn normalize_int(value: i64, target: TargetSpec) -> i64 {
    let bits = target.word_size_bits.clamp(1, 64);
    if bits >= 64 {
        return value;
    }
    let shift = 64u8.saturating_sub(bits);
    (value << shift) >> shift
}

fn build_cache_key(target: TargetSpec) -> CtCacheKey {
    CtCacheKey {
        target_word_size_bits: target.word_size_bits,
        target_endianness: match target.endianness {
            Endianness::Little => "little",
            Endianness::Big => "big",
        }
        .to_owned(),
        target_pointer_alignment: target.pointer_alignment,
        evaluator_policy: EVALUATOR_POLICY.to_owned(),
        compiler_version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

fn collect_file_deps(
    program: &CoreProgram,
    sema: &crate::pipeline::phases::SemanticTables,
) -> Vec<CtFileDep> {
    let mut deps = Vec::new();
    let mut seen = HashSet::new();

    for stmt in program.stmts() {
        let crate::ir::core::StmtKind::Perform { effect, args, .. } = &stmt.kind else {
            continue;
        };
        let is_ct_only = sema
            .effect_properties
            .get(effect)
            .is_some_and(|properties| properties.flags.contains(EffectFlags::CT_ONLY));
        if !is_ct_only {
            continue;
        }
        let Some(first_arg) = args.first() else {
            continue;
        };
        let Some(ExprKind::Literal(Literal::String(path))) =
            program.expr(*first_arg).map(|expr| &expr.kind)
        else {
            continue;
        };

        let normalized = normalize_path(path);
        let hash = hash_file_or_missing(&normalized);
        let key = format!("{normalized}:{hash}");
        if !seen.insert(key) {
            continue;
        }

        deps.push(CtFileDep {
            path: normalized,
            content_hash: hash,
        });
    }

    deps
}

fn normalize_path(path: &str) -> String {
    let raw = Path::new(path);
    raw.canonicalize()
        .unwrap_or_else(|_| raw.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn hash_file_or_missing(path: &str) -> String {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => return "missing".to_owned(),
    };

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}
