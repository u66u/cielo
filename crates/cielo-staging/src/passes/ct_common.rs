use std::collections::HashSet;

use crate::pipeline::phases::{BranchDecision, CtCacheKey, CtEvalStats};
use cielo_base::densemap::DenseMap;
use cielo_base::{EffectLabelId, ExprId};
use cielo_ir::core::{BinaryOp, CoreProgram, ExprKind, Literal, OpCategory, UnaryOp};
use cielo_ir::target::{Endianness, TargetSpec};

pub(super) const EVALUATOR_POLICY: &str = "v1-int-wrap-litnorm";

pub(super) fn assert_pre_staging_effects_concrete(program: &CoreProgram) {
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
            cielo_ir::core::StmtKind::Call { effects, .. } => {
                for effect in effects.iter() {
                    assert_effect(effect, format!("stmt s{stmt_idx} call effect row").as_str());
                }
            }
            cielo_ir::core::StmtKind::Perform { effect, .. } => {
                assert_effect(*effect, format!("stmt s{stmt_idx} perform effect").as_str());
            }
            _ => {}
        }
    }
}

pub(super) fn rebuild_branch_decisions(
    ct_cache: &DenseMap<ExprId, Literal>,
) -> DenseMap<ExprId, BranchDecision> {
    ct_cache
        .iter()
        .filter_map(|(expr_id, literal)| match literal {
            Literal::Bool(true) => Some((expr_id, BranchDecision::LiveTrue)),
            Literal::Bool(false) => Some((expr_id, BranchDecision::LiveFalse)),
            _ => None,
        })
        .collect()
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

pub(super) fn compute_ct_cache(
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

        if !changed {
            break;
        }
    }

    (cache, stats)
}

fn eval_expr(
    expr: &cielo_ir::core::ExprNode,
    cache: &DenseMap<ExprId, Literal>,
    target: TargetSpec,
) -> EvalOutcome {
    match &expr.kind {
        ExprKind::Literal(value) => EvalOutcome::Folded {
            value: normalize_literal(value.clone(), target),
            kind: FoldKind::Literal,
            used_host_float: false,
        },
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

pub(super) fn normalize_literal(literal: Literal, target: TargetSpec) -> Literal {
    match literal {
        Literal::Int(raw) => Literal::Int(normalize_int(raw, target)),
        _ => literal,
    }
}

pub(super) fn eval_unary(
    op: UnaryOp,
    value: &Literal,
    target: TargetSpec,
) -> Option<(Literal, bool)> {
    match (op, value) {
        (UnaryOp::Neg, Literal::Int(v)) => {
            Some((Literal::Int(normalize_int(v.wrapping_neg(), target)), false))
        }
        (UnaryOp::Neg, Literal::Float(v)) if v.is_finite() => Some((Literal::Float(-v), true)),
        (UnaryOp::Not, Literal::Bool(v)) => Some((Literal::Bool(!v), false)),
        _ => None,
    }
}

pub(super) fn eval_binary(
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
    value.is_finite().then_some((Literal::Float(value), true))
}

pub(super) fn normalize_int(value: i64, target: TargetSpec) -> i64 {
    let bits = target.word_size_bits.clamp(1, 64);
    if bits >= 64 {
        return value;
    }
    let shift = 64u8.saturating_sub(bits);
    (value << shift) >> shift
}

pub(super) fn host_float_operands_supported(lhs: f64, rhs: f64) -> bool {
    lhs.is_finite() && rhs.is_finite()
}

pub(super) fn build_cache_key(target: TargetSpec) -> CtCacheKey {
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
