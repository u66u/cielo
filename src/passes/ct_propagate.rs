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
use std::fs::File;
use std::io::Read;
use std::path::Path as FsPath;
use std::path::Path;

use crate::common::densemap::DenseMap;
use crate::common::ids::{EffectLabelId, ExprId};
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
    assert_pre_staging_effects_concrete(mono.program());

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

fn assert_pre_staging_effects_concrete(program: &CoreProgram) {
    let effect_count = program.effects().len();
    let assert_effect = |effect: EffectLabelId, context: &str| {
        assert!(
            effect.is_valid() && effect.index() < effect_count,
            "compiler bug: unresolved/non-concrete effect at pre-staging boundary: {context} references e{} but only {effect_count} effect declarations exist",
            effect.as_u32()
        );
    };

    for (func_idx, function) in program.functions().iter().enumerate() {
        for effect in function.declared_effects.iter() {
            assert_effect(
                effect,
                format!("function f{func_idx} declared_effects").as_str(),
            );
        }
    }

    for (handler_idx, handler) in program.handlers().iter().enumerate() {
        assert_effect(
            handler.effect,
            format!("handler h{handler_idx} effect").as_str(),
        );
    }

    for (stmt_idx, stmt) in program.stmts().iter().enumerate() {
        match &stmt.kind {
            crate::ir::core::StmtKind::Call { effects, .. } => {
                for effect in effects.iter() {
                    assert_effect(effect, format!("stmt s{stmt_idx} call effect row").as_str());
                }
            }
            crate::ir::core::StmtKind::Perform { effect, .. } => {
                assert_effect(*effect, format!("stmt s{stmt_idx} perform effect").as_str());
            }
            _ => {}
        }
    }
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
    let exprs = program.exprs();
    let mut cache = DenseMap::default();
    let mut stats = CtEvalStats::default();

    macro_rules! inc {
        ($field:ident) => {
            stats.$field = stats.$field.saturating_add(1)
        };
    }

    let max_passes = exprs.len().saturating_add(1).max(1);
    for _ in 0..max_passes {
        inc!(iterations);
        let mut changed = false;
        for (i, expr) in exprs.iter().enumerate() {
            let id = ExprId::new(i);
            if cache.contains_key(&id) {
                inc!(cache_hits);
                continue;
            }
            inc!(eval_attempts);

            match eval_expr(expr, &cache, target) {
                EvalOutcome::Folded {
                    value,
                    kind,
                    used_host_float,
                } => {
                    cache.insert(id, value);
                    inc!(cache_inserts);

                    match kind {
                        FoldKind::Literal => inc!(folded_literals),
                        FoldKind::Unary => inc!(folded_unary),
                        FoldKind::Binary => inc!(folded_binary),
                    }
                    if used_host_float {
                        inc!(folded_float_host);
                    }
                    changed = true;
                }
                EvalOutcome::MissingInputs => inc!(miss_missing_inputs),
                EvalOutcome::Unsupported => inc!(miss_unsupported),
            }
        }

        // if cache.len() == exprs.len() { break; }

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
    use BinaryOp::*;
    use Literal::*;
    use OpCategory::*;

    match (op.category(), left, right) {
        (Arithmetic, Int(a), Int(b)) => eval_int_arith(op, *a, *b, target),
        (Arithmetic, Float(a), Float(b)) => eval_float_arith(op, *a, *b),

        (Comparison, Int(a), Int(b)) => {
            let lhs = normalize_int(*a, target);
            let rhs = normalize_int(*b, target);
            eval_cmp(op, &lhs, &rhs, false)
        }
        (Comparison, Float(a), Float(b)) if host_float_operands_supported(*a, *b) => {
            eval_cmp(op, a, b, true)
        }
        (Comparison, Char(a), Char(b)) => eval_cmp(op, a, b, false),

        (Equality, Int(a), Int(b)) => {
            let lhs = normalize_int(*a, target);
            let rhs = normalize_int(*b, target);
            eval_eq(op, &lhs, &rhs, false)
        }
        (Equality, Bool(a), Bool(b)) => eval_eq(op, a, b, false),
        (Equality, Float(a), Float(b)) if host_float_operands_supported(*a, *b) => {
            eval_eq(op, a, b, true)
        }
        (Equality, Char(a), Char(b)) => eval_eq(op, a, b, false),
        (Equality, String(a), String(b)) => eval_eq(op, a, b, false),
        (Equality, Unit, Unit) => eval_eq(op, &(), &(), false),

        (Logical, Bool(a), Bool(b)) => match op {
            And => folded(Literal::Bool(*a && *b), false),
            Or => folded(Literal::Bool(*a || *b), false),
            _ => None,
        },

        _ => None,
    }
}

#[inline]
fn folded(lit: Literal, used_host_float: bool) -> Option<(Literal, bool)> {
    Some((lit, used_host_float))
}

#[inline]
fn eval_cmp<T: PartialOrd>(
    op: BinaryOp,
    lhs: &T,
    rhs: &T,
    used_host_float: bool,
) -> Option<(Literal, bool)> {
    let value = match op {
        BinaryOp::Lt => lhs < rhs,
        BinaryOp::Le => lhs <= rhs,
        BinaryOp::Gt => lhs > rhs,
        BinaryOp::Ge => lhs >= rhs,
        _ => return None,
    };
    folded(Literal::Bool(value), used_host_float)
}

#[inline]
fn eval_eq<T: PartialEq>(
    op: BinaryOp,
    lhs: &T,
    rhs: &T,
    used_host_float: bool,
) -> Option<(Literal, bool)> {
    let value = match op {
        BinaryOp::Eq => lhs == rhs,
        BinaryOp::Ne => lhs != rhs,
        _ => return None,
    };
    folded(Literal::Bool(value), used_host_float)
}

#[inline]
fn eval_int_arith(
    op: BinaryOp,
    left: i64,
    right: i64,
    target: TargetSpec,
) -> Option<(Literal, bool)> {
    let lhs = normalize_int(left, target);
    let rhs = normalize_int(right, target);
    let value = match op {
        BinaryOp::Add => lhs.wrapping_add(rhs),
        BinaryOp::Sub => lhs.wrapping_sub(rhs),
        BinaryOp::Mul => lhs.wrapping_mul(rhs),
        BinaryOp::Div if rhs != 0 => lhs.wrapping_div(rhs),
        BinaryOp::Mod if rhs != 0 => lhs.wrapping_rem(rhs),
        _ => return None,
    };
    folded(Literal::Int(normalize_int(value, target)), false)
}

#[inline]
fn eval_float_arith(op: BinaryOp, left: f64, right: f64) -> Option<(Literal, bool)> {
    if !host_float_operands_supported(left, right) {
        return None;
    }
    let value = match op {
        BinaryOp::Add => left + right,
        BinaryOp::Sub => left - right,
        BinaryOp::Mul => left * right,
        BinaryOp::Div if right != 0.0 => left / right,
        BinaryOp::Mod if right != 0.0 => left % right,
        _ => return None,
    };
    value.is_finite().then(|| (Literal::Float(value), true))
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
        compiler_version: crate::CIELO_VERSION.to_owned(),
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

// maybe better to use option
fn hash_file_or_missing(path: &str) -> String {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return "missing".to_owned(),
    };
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let read = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => return "missing".to_owned(),
        };
        hasher.update(&buf[..read]);
    }
    hasher.finalize().to_hex().to_string()
}
