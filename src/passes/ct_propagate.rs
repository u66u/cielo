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

use std::collections::HashSet;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::Path as FsPath;
use std::path::Path;

use crate::common::densemap::DenseMap;
use crate::common::ids::ExprId;
use crate::ir::core::{BinaryOp, CoreProgram, ExprKind, Literal, OpCategory, UnaryOp};
use crate::pipeline::compiler::{Endianness, TargetSpec};
use crate::pipeline::ct_query_cache::{
    CtQueryCacheSnapshot, deps_match, fingerprint_program, load_snapshot as load_query_snapshot,
    normalized_file_deps, save_snapshot as save_query_snapshot,
};
use crate::pipeline::phases::{
    CtCacheKey, CtEvalStats, CtFileDep, CtPropagated, CtPropagationTables, Monomorphized,
};
use crate::sema::effect::EffectFlags;

const EVALUATOR_POLICY: &str = "v1-int-wrap-litnorm";

pub fn run(mono: Monomorphized, target: TargetSpec) -> CtPropagated {
    run_with_query_cache(mono, target, None)
}

pub fn run_with_query_cache(
    mono: Monomorphized,
    target: TargetSpec,
    query_cache_path: Option<&FsPath>,
) -> CtPropagated {
    let mut ct = CtPropagationTables::default();
    ct.cache_key = build_cache_key(target);
    ct.file_deps = collect_file_deps(mono.program(), mono.sema());
    ct.file_deps = normalized_file_deps(ct.file_deps);

    let mut used_query_cache = false;
    if let Some(path) = query_cache_path {
        let program_fingerprint = fingerprint_program(mono.program());
        if let Ok(snapshot) = load_query_snapshot(path)
            && snapshot.program_fingerprint == program_fingerprint
            && snapshot.cache_key == ct.cache_key
            && deps_match(snapshot.file_deps.as_slice(), ct.file_deps.as_slice())
        {
            ct.ct_cache = snapshot.ct_cache;
            used_query_cache = true;
            ct.eval_stats.iterations = 1;
            ct.eval_stats.cache_hits = ct.ct_cache.len().try_into().unwrap_or(u32::MAX);
        }
    }

    if !used_query_cache {
        let (ct_cache, eval_stats) = compute_ct_cache(mono.program(), target);
        ct.ct_cache = ct_cache;
        ct.eval_stats = eval_stats;
        if let Some(path) = query_cache_path {
            let snapshot = CtQueryCacheSnapshot {
                cache_key: ct.cache_key.clone(),
                file_deps: ct.file_deps.clone(),
                program_fingerprint: fingerprint_program(mono.program()),
                ct_cache: ct.ct_cache.clone(),
            };
            let _ = save_query_snapshot(path, &snapshot);
        }
    }

    ct.branch_decisions = ct
        .ct_cache
        .iter()
        .filter_map(|(expr_id, literal)| match literal {
            Literal::Bool(true) => {
                Some((expr_id, crate::pipeline::phases::BranchDecision::LiveTrue))
            }
            Literal::Bool(false) => {
                Some((expr_id, crate::pipeline::phases::BranchDecision::LiveFalse))
            }
            _ => None,
        })
        .collect();

    mono.into_ct_propagated(ct)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FoldKind {
    Literal,
    Unary,
    Binary,
}

#[derive(Clone, Debug)]
enum EvalOutcome {
    Folded {
        value: Literal,
        kind: FoldKind,
        used_host_float: bool,
    },
    MissingInputs,
    Unsupported,
}

fn eval_expr(
    expr: &crate::ir::core::ExprNode,
    cache: &DenseMap<ExprId, Literal>,
    target: TargetSpec,
) -> EvalOutcome {
    match &expr.kind {
        ExprKind::Literal(value) => {
            let value = match value {
                Literal::Int(raw) => Literal::Int(normalize_int(*raw, target)),
                _ => value.clone(),
            };
            EvalOutcome::Folded {
                value,
                kind: FoldKind::Literal,
                used_host_float: false,
            }
        }
        ExprKind::Unary { op, expr } => {
            let Some(value) = cache.get(expr) else {
                return EvalOutcome::MissingInputs;
            };
            let Some((value, used_host_float)) = eval_unary(*op, value, target) else {
                return EvalOutcome::Unsupported;
            };
            EvalOutcome::Folded {
                value,
                kind: FoldKind::Unary,
                used_host_float,
            }
        }
        ExprKind::Binary { op, lhs, rhs } => {
            let Some(left) = cache.get(lhs) else {
                return EvalOutcome::MissingInputs;
            };
            let Some(right) = cache.get(rhs) else {
                return EvalOutcome::MissingInputs;
            };
            let Some((value, used_host_float)) = eval_binary(*op, left, right, target) else {
                return EvalOutcome::Unsupported;
            };
            EvalOutcome::Folded {
                value,
                kind: FoldKind::Binary,
                used_host_float,
            }
        }
        _ => EvalOutcome::Unsupported,
    }
}

fn compute_ct_cache(
    program: &CoreProgram,
    target: TargetSpec,
) -> (DenseMap<ExprId, Literal>, CtEvalStats) {
    let mut cache = DenseMap::default();
    let mut stats = CtEvalStats::default();
    let limit = program.exprs().len().saturating_add(1).max(1);
    for _ in 0..limit {
        stats.iterations = stats.iterations.saturating_add(1);
        let mut changed = false;
        for (idx, expr) in program.exprs().iter().enumerate() {
            let expr_id = ExprId::new(idx);
            if cache.contains_key(&expr_id) {
                stats.cache_hits = stats.cache_hits.saturating_add(1);
                continue;
            }
            stats.eval_attempts = stats.eval_attempts.saturating_add(1);
            match eval_expr(expr, &cache, target) {
                EvalOutcome::Folded {
                    value,
                    kind,
                    used_host_float,
                } => {
                    let _ = cache.insert(expr_id, value);
                    stats.cache_inserts = stats.cache_inserts.saturating_add(1);
                    match kind {
                        FoldKind::Literal => {
                            stats.folded_literals = stats.folded_literals.saturating_add(1)
                        }
                        FoldKind::Unary => {
                            stats.folded_unary = stats.folded_unary.saturating_add(1)
                        }
                        FoldKind::Binary => {
                            stats.folded_binary = stats.folded_binary.saturating_add(1)
                        }
                    }
                    if used_host_float {
                        stats.folded_float_host = stats.folded_float_host.saturating_add(1);
                    }
                    changed = true;
                }
                EvalOutcome::MissingInputs => {
                    stats.miss_missing_inputs = stats.miss_missing_inputs.saturating_add(1);
                }
                EvalOutcome::Unsupported => {
                    stats.miss_unsupported = stats.miss_unsupported.saturating_add(1);
                }
            }
        }
        if !changed {
            break;
        }
    }
    (cache, stats)
}

fn eval_unary(op: UnaryOp, value: &Literal, target: TargetSpec) -> Option<(Literal, bool)> {
    match (op, value) {
        (UnaryOp::Neg, Literal::Int(v)) => {
            Some((Literal::Int(normalize_int(v.wrapping_neg(), target)), false))
        }
        (UnaryOp::Neg, Literal::Float(v)) if v.is_finite() => Some((Literal::Float(-v), true)),
        (UnaryOp::Not, Literal::Bool(v)) => Some((Literal::Bool(!v), false)),
        _ => None,
    }
}

fn eval_binary(
    op: BinaryOp,
    left: &Literal,
    right: &Literal,
    target: TargetSpec,
) -> Option<(Literal, bool)> {
    match (op.category(), left, right) {
        (OpCategory::Arithmetic, Literal::Int(a), Literal::Int(b)) => {
            let lhs = normalize_int(*a, target);
            let rhs = normalize_int(*b, target);
            match op {
                BinaryOp::Add => {
                    Some((Literal::Int(normalize_int(lhs.wrapping_add(rhs), target)), false))
                }
                BinaryOp::Sub => {
                    Some((Literal::Int(normalize_int(lhs.wrapping_sub(rhs), target)), false))
                }
                BinaryOp::Mul => {
                    Some((Literal::Int(normalize_int(lhs.wrapping_mul(rhs), target)), false))
                }
                BinaryOp::Div if rhs != 0 => lhs
                    .checked_div(rhs)
                    .map(|value| (Literal::Int(normalize_int(value, target)), false)),
                BinaryOp::Mod if rhs != 0 => lhs
                    .checked_rem(rhs)
                    .map(|value| (Literal::Int(normalize_int(value, target)), false)),
                _ => None,
            }
        }
        (OpCategory::Arithmetic, Literal::Float(a), Literal::Float(b)) => {
            if !host_float_operands_supported(*a, *b) {
                return None;
            }
            let value = match op {
                BinaryOp::Add => Some(*a + *b),
                BinaryOp::Sub => Some(*a - *b),
                BinaryOp::Mul => Some(*a * *b),
                BinaryOp::Div if *b != 0.0 => Some(*a / *b),
                BinaryOp::Mod if *b != 0.0 => Some(*a % *b),
                _ => None,
            }?;
            if !value.is_finite() {
                return None;
            }
            Some((Literal::Float(value), true))
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
            Some((Literal::Bool(value), false))
        }
        (OpCategory::Comparison, Literal::Float(a), Literal::Float(b)) => {
            if !host_float_operands_supported(*a, *b) {
                return None;
            }
            let value = match op {
                BinaryOp::Lt => *a < *b,
                BinaryOp::Le => *a <= *b,
                BinaryOp::Gt => *a > *b,
                BinaryOp::Ge => *a >= *b,
                _ => return None,
            };
            Some((Literal::Bool(value), true))
        }
        (OpCategory::Comparison, Literal::Char(a), Literal::Char(b)) => {
            let value = match op {
                BinaryOp::Lt => *a < *b,
                BinaryOp::Le => *a <= *b,
                BinaryOp::Gt => *a > *b,
                BinaryOp::Ge => *a >= *b,
                _ => return None,
            };
            Some((Literal::Bool(value), false))
        }
        (OpCategory::Equality, Literal::Int(a), Literal::Int(b)) => {
            let lhs = normalize_int(*a, target);
            let rhs = normalize_int(*b, target);
            let value = match op {
                BinaryOp::Eq => lhs == rhs,
                BinaryOp::Ne => lhs != rhs,
                _ => return None,
            };
            Some((Literal::Bool(value), false))
        }
        (OpCategory::Equality, Literal::Bool(a), Literal::Bool(b)) => {
            let value = match op {
                BinaryOp::Eq => *a == *b,
                BinaryOp::Ne => *a != *b,
                _ => return None,
            };
            Some((Literal::Bool(value), false))
        }
        (OpCategory::Equality, Literal::Float(a), Literal::Float(b)) => {
            if !host_float_operands_supported(*a, *b) {
                return None;
            }
            let value = match op {
                BinaryOp::Eq => *a == *b,
                BinaryOp::Ne => *a != *b,
                _ => return None,
            };
            Some((Literal::Bool(value), true))
        }
        (OpCategory::Equality, Literal::Char(a), Literal::Char(b)) => {
            let value = match op {
                BinaryOp::Eq => *a == *b,
                BinaryOp::Ne => *a != *b,
                _ => return None,
            };
            Some((Literal::Bool(value), false))
        }
        (OpCategory::Equality, Literal::String(a), Literal::String(b)) => {
            let value = match op {
                BinaryOp::Eq => *a == *b,
                BinaryOp::Ne => *a != *b,
                _ => return None,
            };
            Some((Literal::Bool(value), false))
        }
        (OpCategory::Equality, Literal::Unit, Literal::Unit) => {
            let value = match op {
                BinaryOp::Eq => true,
                BinaryOp::Ne => false,
                _ => return None,
            };
            Some((Literal::Bool(value), false))
        }
        (OpCategory::Logical, Literal::Bool(a), Literal::Bool(b)) => {
            let value = match op {
                BinaryOp::And => *a && *b,
                BinaryOp::Or => *a || *b,
                _ => return None,
            };
            Some((Literal::Bool(value), false))
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

fn host_float_operands_supported(lhs: f64, rhs: f64) -> bool {
    lhs.is_finite() && rhs.is_finite()
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
