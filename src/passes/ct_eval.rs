use std::collections::HashMap;
use std::path::Path;

use crate::common::densemap::DenseMap;
use crate::common::ids::{ExprId, FuncId, StmtId, VarId};
use crate::ir::core::{
    BinaryOp, CoreProgram, ExprKind, Literal, OpCategory, StageDirective, UnaryOp,
};
use crate::passes::ct_propagate;
use crate::pipeline::compiler::TargetSpec;
use crate::pipeline::phases::{BranchDecision, CtPropagated, Monomorphized};

const MAX_CALL_EVAL_DEPTH: usize = 32;

pub fn run(mono: Monomorphized, target: TargetSpec) -> CtPropagated {
    run_with_query_cache(mono, target, None)
}

pub fn run_with_query_cache(
    mono: Monomorphized,
    target: TargetSpec,
    query_cache_path: Option<&Path>,
) -> CtPropagated {
    let seeded = ct_propagate::run_with_query_cache(mono, target, query_cache_path);
    let (program, diagnostics, sema, mono, mut ct) = seeded.into_parts();

    let call_folds = fold_known_pure_calls(&program, target, &mut ct.ct_cache);
    ct.eval_stats.iterations = ct
        .eval_stats
        .iterations
        .saturating_add(call_folds.iterations);
    ct.eval_stats.eval_attempts = ct
        .eval_stats
        .eval_attempts
        .saturating_add(call_folds.attempts);
    ct.eval_stats.cache_inserts = ct
        .eval_stats
        .cache_inserts
        .saturating_add(call_folds.inserts);
    ct.branch_decisions = rebuild_branch_decisions(&ct.ct_cache);

    CtPropagated::new(program, diagnostics, sema, mono, ct)
}

fn rebuild_branch_decisions(
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

#[derive(Default)]
struct CallFoldStats {
    iterations: u32,
    attempts: u32,
    inserts: u32,
}

fn fold_known_pure_calls(
    program: &CoreProgram,
    target: TargetSpec,
    cache: &mut DenseMap<ExprId, Literal>,
) -> CallFoldStats {
    let mut stats = CallFoldStats::default();
    let max_passes = program.exprs().len().saturating_add(1).max(1);

    for _ in 0..max_passes {
        stats.iterations = stats.iterations.saturating_add(1);
        let mut changed = false;

        for (idx, expr) in program.exprs().iter().enumerate() {
            let expr_id = ExprId::new(idx);
            if cache.contains_key(&expr_id) {
                continue;
            }
            let ExprKind::PureCall { callee, args } = &expr.kind else {
                continue;
            };

            stats.attempts = stats.attempts.saturating_add(1);
            let Some(arg_values) = args
                .iter()
                .map(|arg| cache.get(arg).cloned())
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };

            let mut evaluator = CallEvaluator {
                program,
                cache,
                target,
                call_stack: Vec::new(),
            };
            let Some(value) = evaluator.eval_call(*callee, arg_values.as_slice()) else {
                continue;
            };

            let _ = cache.insert(expr_id, value);
            stats.inserts = stats.inserts.saturating_add(1);
            changed = true;
        }

        if !changed {
            break;
        }
    }

    stats
}

struct CallEvaluator<'a> {
    program: &'a CoreProgram,
    cache: &'a DenseMap<ExprId, Literal>,
    target: TargetSpec,
    call_stack: Vec<FuncId>,
}

impl CallEvaluator<'_> {
    fn eval_call(&mut self, callee: FuncId, args: &[Literal]) -> Option<Literal> {
        let function = self.program.function(callee)?;
        if !function.declared_effects.is_empty() || function.params.len() != args.len() {
            return None;
        }
        if self.call_stack.contains(&callee) || self.call_stack.len() >= MAX_CALL_EVAL_DEPTH {
            return None;
        }

        let mut env = HashMap::new();
        for (param, arg) in function.params.iter().zip(args.iter()) {
            env.insert(*param, arg.clone());
        }

        self.call_stack.push(callee);
        let outcome = self.eval_stmt(function.body, &mut env);
        let _ = self.call_stack.pop();
        outcome
    }

    fn eval_stmt(&mut self, stmt_id: StmtId, env: &mut HashMap<VarId, Literal>) -> Option<Literal> {
        let stmt = self.program.stmt(stmt_id)?;
        match &stmt.kind {
            crate::ir::core::StmtKind::Return(expr) => self.eval_expr(*expr, env),
            crate::ir::core::StmtKind::Let {
                binding,
                value,
                next,
            } => {
                let lit = self.eval_expr(*value, env)?;
                env.insert(*binding, lit);
                self.eval_stmt(*next, env)
            }
            crate::ir::core::StmtKind::Val {
                binding,
                value,
                next,
            } => {
                let lit = self.eval_stmt(*value, env)?;
                env.insert(*binding, lit);
                self.eval_stmt(*next, env)
            }
            crate::ir::core::StmtKind::If {
                cond,
                then_branch,
                else_branch,
            } => match self.eval_expr(*cond, env)? {
                Literal::Bool(true) => {
                    let mut branch_env = env.clone();
                    self.eval_stmt(*then_branch, &mut branch_env)
                }
                Literal::Bool(false) => {
                    let mut branch_env = env.clone();
                    self.eval_stmt(*else_branch, &mut branch_env)
                }
                _ => None,
            },
            crate::ir::core::StmtKind::Stage { stage, body, next } => {
                if !matches!(stage, StageDirective::Comptime) {
                    return None;
                }
                let mut stage_env = env.clone();
                let value = self.eval_stmt(*body, &mut stage_env)?;
                if let Some(next_stmt) = next {
                    self.eval_stmt(*next_stmt, env)
                } else {
                    Some(value)
                }
            }
            crate::ir::core::StmtKind::Match { .. }
            | crate::ir::core::StmtKind::Call { .. }
            | crate::ir::core::StmtKind::Perform { .. }
            | crate::ir::core::StmtKind::Resume { .. }
            | crate::ir::core::StmtKind::Handle { .. }
            | crate::ir::core::StmtKind::Hole { .. }
            | crate::ir::core::StmtKind::Error(_) => None,
        }
    }

    fn eval_expr(&mut self, expr_id: ExprId, env: &HashMap<VarId, Literal>) -> Option<Literal> {
        if let Some(cached) = self.cache.get(&expr_id) {
            return Some(cached.clone());
        }

        let expr = self.program.expr(expr_id)?;
        match &expr.kind {
            ExprKind::Literal(lit) => Some(normalize_literal(lit.clone(), self.target)),
            ExprKind::Var(var) => env.get(var).cloned(),
            ExprKind::Unary { op, expr } => {
                let value = self.eval_expr(*expr, env)?;
                eval_unary(*op, &value, self.target)
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let lhs = self.eval_expr(*lhs, env)?;
                let rhs = self.eval_expr(*rhs, env)?;
                eval_binary(*op, &lhs, &rhs, self.target)
            }
            ExprKind::PureCall { callee, args } => {
                let args = args
                    .iter()
                    .map(|arg| self.eval_expr(*arg, env))
                    .collect::<Option<Vec<_>>>()?;
                self.eval_call(*callee, args.as_slice())
            }
            ExprKind::MakeStruct { .. } | ExprKind::MakeEnum { .. } | ExprKind::Error(_) => None,
        }
    }
}

fn normalize_literal(literal: Literal, target: TargetSpec) -> Literal {
    match literal {
        Literal::Int(raw) => Literal::Int(normalize_int(raw, target)),
        _ => literal,
    }
}

fn eval_unary(op: UnaryOp, value: &Literal, target: TargetSpec) -> Option<Literal> {
    match (op, value) {
        (UnaryOp::Neg, Literal::Int(v)) => {
            Some(Literal::Int(normalize_int(v.wrapping_neg(), target)))
        }
        (UnaryOp::Neg, Literal::Float(v)) if v.is_finite() => Some(Literal::Float(-v)),
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
    use BinaryOp::*;
    use Literal::*;
    use OpCategory::*;

    match (op.category(), left, right) {
        (Arithmetic, Int(a), Int(b)) => eval_int_arith(op, *a, *b, target),
        (Arithmetic, Float(a), Float(b)) => eval_float_arith(op, *a, *b),

        (Comparison, Int(a), Int(b)) => {
            let lhs = normalize_int(*a, target);
            let rhs = normalize_int(*b, target);
            eval_cmp(op, &lhs, &rhs)
        }
        (Comparison, Float(a), Float(b)) if host_float_operands_supported(*a, *b) => {
            eval_cmp(op, a, b)
        }
        (Comparison, Char(a), Char(b)) => eval_cmp(op, a, b),

        (Equality, Int(a), Int(b)) => {
            let lhs = normalize_int(*a, target);
            let rhs = normalize_int(*b, target);
            eval_eq(op, &lhs, &rhs)
        }
        (Equality, Bool(a), Bool(b)) => eval_eq(op, a, b),
        (Equality, Float(a), Float(b)) if host_float_operands_supported(*a, *b) => {
            eval_eq(op, a, b)
        }
        (Equality, Char(a), Char(b)) => eval_eq(op, a, b),
        (Equality, String(a), String(b)) => eval_eq(op, a, b),
        (Equality, Unit, Unit) => eval_eq(op, &(), &()),

        (Logical, Bool(a), Bool(b)) => match op {
            And => Some(Literal::Bool(*a && *b)),
            Or => Some(Literal::Bool(*a || *b)),
            _ => None,
        },

        _ => None,
    }
}

fn eval_cmp<T: PartialOrd>(op: BinaryOp, lhs: &T, rhs: &T) -> Option<Literal> {
    let value = match op {
        BinaryOp::Lt => lhs < rhs,
        BinaryOp::Le => lhs <= rhs,
        BinaryOp::Gt => lhs > rhs,
        BinaryOp::Ge => lhs >= rhs,
        _ => return None,
    };
    Some(Literal::Bool(value))
}

fn eval_eq<T: PartialEq>(op: BinaryOp, lhs: &T, rhs: &T) -> Option<Literal> {
    let value = match op {
        BinaryOp::Eq => lhs == rhs,
        BinaryOp::Ne => lhs != rhs,
        _ => return None,
    };
    Some(Literal::Bool(value))
}

fn eval_int_arith(op: BinaryOp, left: i64, right: i64, target: TargetSpec) -> Option<Literal> {
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
    Some(Literal::Int(normalize_int(value, target)))
}

fn eval_float_arith(op: BinaryOp, left: f64, right: f64) -> Option<Literal> {
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
    value.is_finite().then_some(Literal::Float(value))
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
