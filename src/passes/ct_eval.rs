use std::collections::HashMap;
use std::path::Path;

use crate::common::densemap::DenseMap;
use crate::common::ids::{ExprId, FuncId, StmtId, VarId};
use crate::ir::core::{BinaryOp, CoreProgram, ExprKind, Literal, StageDirective, UnaryOp};
use crate::passes::ct_common;
use crate::pipeline::compiler::TargetSpec;
use crate::pipeline::ct_query_cache::{
    CtQueryCacheSnapshot, deps_match, fingerprint_program, load_snapshot as load_query_snapshot,
    normalized_file_deps, save_snapshot as save_query_snapshot,
};
use crate::pipeline::phases::{CtPropagated, CtPropagationTables, Monomorphized};

const MAX_CALL_EVAL_DEPTH: usize = 32;

pub fn run(mono: Monomorphized, target: TargetSpec) -> CtPropagated {
    run_with_query_cache(mono, target, None)
}

pub fn run_with_query_cache(
    mono: Monomorphized,
    target: TargetSpec,
    query_cache_path: Option<&Path>,
) -> CtPropagated {
    ct_common::assert_pre_staging_effects_concrete(mono.program());

    let mut ct = CtPropagationTables::default();
    ct.cache_key = ct_common::build_cache_key(target);
    ct.file_deps = ct_common::collect_file_deps(mono.program(), mono.sema());
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
        let (ct_cache, eval_stats) = ct_common::compute_ct_cache(mono.program(), target);
        ct.ct_cache = ct_cache;
        ct.eval_stats = eval_stats;

        let call_folds = fold_known_pure_calls(mono.program(), target, &mut ct.ct_cache);
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

    ct.branch_decisions = ct_common::rebuild_branch_decisions(&ct.ct_cache);

    mono.into_ct_propagated(ct)
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
            ExprKind::Literal(lit) => Some(ct_common::normalize_literal(lit.clone(), self.target)),
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

fn eval_unary(op: UnaryOp, value: &Literal, target: TargetSpec) -> Option<Literal> {
    ct_common::eval_unary(op, value, target).map(|(value, _)| value)
}

fn eval_binary(
    op: BinaryOp,
    left: &Literal,
    right: &Literal,
    target: TargetSpec,
) -> Option<Literal> {
    ct_common::eval_binary(op, left, right, target).map(|(value, _)| value)
}
