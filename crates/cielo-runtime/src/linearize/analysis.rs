use std::collections::{HashMap, HashSet};

use cielo_base::ids::{EffectLabelId, ExprId, LinearStmtId, StmtId, VarId};
use cielo_ir::core::{CoreProgram, ExprKind, HandlerClause, HandlerDef, StmtKind};
use cielo_ir::linear::{LinearProgram, LinearStmt};
use cielo_ir::walk::any_expr;
use cielo_sema::SemanticTables;

use super::types::{
    ClauseConvention, ClausePolicy, ClauseResumeAnalysis, PerformReach, ResumeQualifier,
    ResumeStrategy, ResumeUseBound, ResumeUseRange,
};

pub(super) fn is_identity_return_of_var(
    program: &CoreProgram,
    stmt_id: StmtId,
    var: VarId,
) -> bool {
    let Some(stmt) = program.stmt(stmt_id) else {
        return false;
    };
    let StmtKind::Return(expr_id) = stmt.kind else {
        return false;
    };
    let Some(expr) = program.expr(expr_id) else {
        return false;
    };
    matches!(expr.kind, ExprKind::Var(bound) if bound == var)
}

pub(super) fn is_identity_handler_return_clause(
    program: &CoreProgram,
    handler: &HandlerDef,
) -> bool {
    is_identity_return_of_var(program, handler.return_body, handler.return_param)
}

pub(super) fn stmt_effect_row_contains(
    sema: &SemanticTables,
    stmt_id: StmtId,
    effect: EffectLabelId,
) -> bool {
    sema.effects_of_stmt
        .get(stmt_id.index())
        .is_some_and(|row| row.contains(effect))
}

/// True when `effect` reaches `root` through a call rather than a lexical
/// `Perform`. Inlining only rewrites performs it can see, so an effect coming
/// from a callee is never discharged. `Call.effects` is already transitive, so
/// no call-graph walk is needed.
pub(super) fn core_stmt_calls_performing_effect(
    program: &CoreProgram,
    root: StmtId,
    effect: EffectLabelId,
) -> bool {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        match &stmt.kind {
            StmtKind::Call { effects, .. } if effects.contains(effect) => return true,
            StmtKind::Handle {
                handler,
                body,
                next,
            } => {
                let inner = program.handlers().get(handler.index());
                let discharged_by_inner = inner.is_some_and(|inner| inner.effect == effect);
                if !discharged_by_inner {
                    stack.push(*body);
                    // Inlining the inner handler splices its clause bodies into
                    // `body`, so a call they make reaches this handler too.
                    if let Some(inner) = inner {
                        stack.push(inner.return_body);
                        stack.extend(inner.clauses.iter().map(|clause| clause.body));
                    }
                }
                if let Some(next_stmt) = next {
                    stack.push(*next_stmt);
                }
                continue;
            }
            _ => {}
        }
        stack.extend(stmt.child_stmts());
    }
    false
}

pub(super) fn linear_stmt_contains_perform_effect(
    program: &LinearProgram,
    root: LinearStmtId,
    effect: EffectLabelId,
) -> bool {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if matches!(stmt.kind, LinearStmt::Perform { effect: found, .. } if found == effect) {
            return true;
        }
        for child in stmt.child_stmts() {
            stack.push(child);
        }
    }
    false
}

pub(super) fn analyze_clause_resume(
    program: &CoreProgram,
    clause: &HandlerClause,
) -> ClauseResumeAnalysis {
    let Some(resume_var) = clause.resume_param else {
        return ClauseResumeAnalysis {
            qualifier: ResumeQualifier::Abortive,
            min_uses: ResumeUseBound::Zero,
            max_uses: ResumeUseBound::Zero,
            tail_resumptive: true,
            sites: 0,
        };
    };
    let range = clause_resume_use_range(program, clause.body, resume_var);
    let tail_resumptive = is_tail_resumptive_clause(program, clause.body, resume_var);
    let sites = count_resume_sites(program, clause.body, resume_var);
    let qualifier = if range.max.is_many() {
        ResumeQualifier::Multi
    } else if matches!(range.max, ResumeUseBound::Zero) {
        ResumeQualifier::Abortive
    } else if matches!(range.min, ResumeUseBound::One) {
        ResumeQualifier::Linear
    } else {
        ResumeQualifier::Affine
    };

    ClauseResumeAnalysis {
        qualifier,
        min_uses: range.min,
        max_uses: range.max,
        tail_resumptive,
        sites,
    }
}

/// The one place erasure and dispatch are chosen between.
///
/// `Err` is a clause nothing can lower. Only [`PerformReach::Runtime`] produces
/// one: splicing the clause is what discharges a lexical perform, so there is
/// no shape left to refuse on that side.
pub(super) fn clause_policy(
    program: &CoreProgram,
    handler: &HandlerDef,
    clause: &HandlerClause,
    resume: ClauseResumeAnalysis,
    reach: PerformReach,
) -> Result<ClausePolicy, ResidualBlocker> {
    match reach {
        PerformReach::Lexical => Ok(erase_policy(program, clause, resume)),
        PerformReach::Runtime => {
            match residual_clause_blocker(program, handler, clause, resume) {
                Some(blocker) => Err(blocker),
                None => Ok(ClausePolicy::Dispatch),
            }
        }
    }
}

/// Merging is only sound when every path resumes in tail position: an arm that
/// performs or calls before resuming would have those effects hoisted past the
/// join. One site already shares the continuation, so leave it inlined.
///
/// A non-tail clause with several sites is the shape that expands as `k^n`.
/// Defunctionalising shares the continuation without moving anything past it,
/// so it is preferred over inlining wherever the sites can agree on one copy.
fn erase_policy(
    program: &CoreProgram,
    clause: &HandlerClause,
    resume: ClauseResumeAnalysis,
) -> ClausePolicy {
    if resume.sites <= 1 {
        return ClausePolicy::Erase(ResumeStrategy::Inline);
    }
    if resume.tail_resumptive {
        return ClausePolicy::Erase(ResumeStrategy::Join);
    }
    if defunctionalisable(program, clause) {
        return ClausePolicy::Defunctionalise;
    }
    ClausePolicy::Erase(ResumeStrategy::Inline)
}

/// Whether one shared copy of the continuation can serve every `resume`.
///
/// Sharing makes the code after each site reachable from every *other* site, so
/// anything a site reads after resuming has to be defined on all of them. The
/// clause parameters are, being bound once before the clause branches; a
/// variable bound on one site's path only is not, and reading it on another
/// path is an uninitialised read that no later check catches.
///
/// Multi-shot is excluded upstream, and has to stay excluded: two live entries
/// into one continuation copy would need the second to restore what the first
/// consumed (CIELO-42).
fn defunctionalisable(program: &CoreProgram, clause: &HandlerClause) -> bool {
    let Some(resume_var) = clause.resume_param else {
        return false;
    };
    let mut available = clause.params.iter().copied().collect::<HashSet<_>>();
    available.insert(resume_var);
    resume_sites(program, clause.body, resume_var)
        .into_iter()
        .all(|(result, next)| {
            let mut allowed = available.clone();
            allowed.insert(result);
            !stmt_reads_outside(program, next, &allowed)
        })
}

/// Every `resume` of `resume_var` under `root`, as the value it binds and the
/// code that follows it.
fn resume_sites(program: &CoreProgram, root: StmtId, resume_var: VarId) -> Vec<(VarId, StmtId)> {
    let mut sites = Vec::new();
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if let StmtKind::Resume {
            result,
            resume,
            next,
            ..
        } = stmt.kind
            && resume == resume_var
        {
            sites.push((result, next));
        }
        stack.extend(stmt.child_stmts());
    }
    sites
}

/// True when `root` reads a variable neither `bound` nor a binder on the way in
/// introduces.
///
/// A `Handle`'s clauses are not `child_stmts`, so they are walked explicitly:
/// a clause body reading an enclosing binding is exactly the capture this has
/// to see, and missing it would admit a shared continuation that reads it
/// undefined.
fn stmt_reads_outside(program: &CoreProgram, root: StmtId, bound: &HashSet<VarId>) -> bool {
    let Some(stmt) = program.stmt(root) else {
        return false;
    };
    let extended = |binders: &[VarId]| {
        let mut inner = bound.clone();
        inner.extend(binders.iter().copied());
        inner
    };
    let reads = |expr: &ExprId| reads_var_outside(program, *expr, bound);
    match &stmt.kind {
        StmtKind::Return(expr) => reads(expr),
        StmtKind::Let {
            binding,
            value,
            next,
        } => reads(value) || stmt_reads_outside(program, *next, &extended(&[*binding])),
        StmtKind::Val {
            binding,
            value,
            next,
        } => {
            stmt_reads_outside(program, *value, bound)
                || stmt_reads_outside(program, *next, &extended(&[*binding]))
        }
        StmtKind::Call {
            result, args, next, ..
        } => {
            args.iter().any(reads) || stmt_reads_outside(program, *next, &extended(&[*result]))
        }
        StmtKind::Perform {
            result, args, next, ..
        } => {
            args.iter().any(reads)
                || stmt_reads_outside(
                    program,
                    *next,
                    &extended(result.as_ref().map(std::slice::from_ref).unwrap_or_default()),
                )
        }
        StmtKind::Resume {
            result,
            resume,
            arg,
            next,
        } => {
            !bound.contains(resume)
                || reads(arg)
                || stmt_reads_outside(program, *next, &extended(&[*result]))
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            reads(cond)
                || stmt_reads_outside(program, *then_branch, bound)
                || stmt_reads_outside(program, *else_branch, bound)
        }
        StmtKind::Match {
            scrutinee,
            arms,
            default,
        } => {
            reads(scrutinee)
                || arms
                    .iter()
                    .any(|arm| stmt_reads_outside(program, arm.body, &extended(&arm.binders)))
                || default.is_some_and(|stmt| stmt_reads_outside(program, stmt, bound))
        }
        StmtKind::Handle {
            handler,
            body,
            next,
        } => {
            let clauses = program.handlers().get(handler.index()).is_some_and(|def| {
                stmt_reads_outside(program, def.return_body, &extended(&[def.return_param]))
                    || def.clauses.iter().any(|clause| {
                        let mut binders = clause.params.clone();
                        binders.extend(clause.resume_param);
                        stmt_reads_outside(program, clause.body, &extended(&binders))
                    })
            });
            clauses
                || stmt_reads_outside(program, *body, bound)
                || next.is_some_and(|stmt| stmt_reads_outside(program, stmt, bound))
        }
        StmtKind::Stage { body, next, .. } => {
            stmt_reads_outside(program, *body, bound)
                || next.is_some_and(|stmt| stmt_reads_outside(program, stmt, bound))
        }
        StmtKind::Hole { .. } | StmtKind::Error(_) => false,
    }
}

pub(super) fn classify_clause_convention(
    clause: &HandlerClause,
    resume: ClauseResumeAnalysis,
) -> ClauseConvention {
    if clause.resume_param.is_none() {
        return ClauseConvention::Pure;
    }

    match resume.qualifier {
        ResumeQualifier::Multi => ClauseConvention::Control,
        ResumeQualifier::Abortive => ClauseConvention::Direct,
        ResumeQualifier::Affine | ResumeQualifier::Linear => {
            if resume.tail_resumptive {
                ClauseConvention::Direct
            } else {
                ClauseConvention::Control
            }
        }
    }
}

pub(super) fn resume_convention_reason(
    resume: ClauseResumeAnalysis,
    convention: ClauseConvention,
) -> &'static str {
    match convention {
        ClauseConvention::Pure => "clause has no resume parameter",
        ClauseConvention::Direct => match resume.qualifier {
            ResumeQualifier::Abortive => "resume is not used by clause",
            ResumeQualifier::Affine | ResumeQualifier::Linear => {
                "resume is tail-resumptive and can stay on direct path"
            }
            ResumeQualifier::Multi => {
                "unreachable: multi-shot clauses are never direct in v1 lowering"
            }
        },
        ClauseConvention::Control => match resume.qualifier {
            ResumeQualifier::Multi => "resume may be called more than once (v1 rejects multi-shot)",
            ResumeQualifier::Affine | ResumeQualifier::Linear => {
                "resume is not tail-resumptive, so control-path lowering is required"
            }
            ResumeQualifier::Abortive => "unreachable: abortive clauses lower directly in v1",
        },
    }
}

/// Why a clause cannot be left to the runtime dispatcher.
///
/// Deliberately a predicate over the whole clause rather than a match on
/// [`ResumeQualifier`]: `docs/v2/plan_notes.md` carries Effekt's caveat that a
/// clause containing a mutable definition is not tail-resumptive however many
/// times it resumes, so `Stmt::Var`/`Get`/`Put` will add an arm here rather
/// than change the qualifier's meaning.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ResidualBlocker {
    /// Abortive. Discarding the continuation means unwinding out of the frames
    /// between the perform and the handle, and nothing here can do that
    /// without longjmp or a status return on every call.
    Abortive,
    /// The clause's value is not the resumption's value, so the dispatcher
    /// would have to return twice. Needs a real `CieloContinuation`.
    NotTailResumptive,
    /// Replaying the continuation needs a cloned environment (CIELO-42).
    MultiShot,
    /// The clause reads a variable bound outside it. A dispatched clause runs
    /// in its own frame, so that read needs an environment (CIELO-25).
    CapturesEnvironment,
    /// The clause can reach the effect it discharges. Its own frame is still on
    /// the handler stack while it runs, so that perform would dispatch straight
    /// back into it. Inlining diverges on the same shape, but at compile time.
    ReentersHandler,
}

impl ResidualBlocker {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Abortive => {
                "clause discards the continuation, which cannot unwind out of a callee"
            }
            Self::NotTailResumptive => "clause does not resume in tail position",
            Self::MultiShot => "clause may resume more than once",
            Self::CapturesEnvironment => "clause reads a variable bound outside it",
            Self::ReentersHandler => "clause can perform the effect it handles",
        }
    }
}

/// Whether `clause` can run as a dispatched function instead of being inlined.
/// Reached only through [`clause_policy`], so the two answers cannot drift.
fn residual_clause_blocker(
    program: &CoreProgram,
    handler: &HandlerDef,
    clause: &HandlerClause,
    resume: ClauseResumeAnalysis,
) -> Option<ResidualBlocker> {
    if clause.resume_param.is_none() {
        return Some(ResidualBlocker::Abortive);
    }
    match resume.qualifier {
        ResumeQualifier::Multi => return Some(ResidualBlocker::MultiShot),
        ResumeQualifier::Abortive => return Some(ResidualBlocker::Abortive),
        ResumeQualifier::Affine | ResumeQualifier::Linear => {}
    }
    if !resume.tail_resumptive {
        return Some(ResidualBlocker::NotTailResumptive);
    }
    if clause_captures_environment(program, clause) {
        return Some(ResidualBlocker::CapturesEnvironment);
    }
    (core_stmt_performs_effect(program, clause.body, handler.effect)
        || core_stmt_calls_performing_effect(program, clause.body, handler.effect))
    .then_some(ResidualBlocker::ReentersHandler)
}

fn core_stmt_performs_effect(program: &CoreProgram, root: StmtId, effect: EffectLabelId) -> bool {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if matches!(stmt.kind, StmtKind::Perform { effect: found, .. } if found == effect) {
            return true;
        }
        stack.extend(stmt.child_stmts());
    }
    false
}

/// True when the clause body reads a variable it does not itself bind.
///
/// The binder arms must stay exhaustive over the binding statement forms: a
/// missed binder makes a bound variable look free, which is merely pessimal,
/// but a binder credited to the clause that it does not actually bind would
/// let a capture through.
fn clause_captures_environment(program: &CoreProgram, clause: &HandlerClause) -> bool {
    let mut bound = clause.params.iter().copied().collect::<HashSet<_>>();
    bound.extend(clause.resume_param);
    let mut reachable = Vec::new();
    let mut stack = vec![clause.body];
    let mut seen = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        match &stmt.kind {
            StmtKind::Let { binding, .. } | StmtKind::Val { binding, .. } => {
                bound.insert(*binding);
            }
            StmtKind::Call { result, .. } => {
                bound.insert(*result);
            }
            StmtKind::Perform { result, .. } => bound.extend(*result),
            StmtKind::Resume { result, .. } => {
                bound.insert(*result);
            }
            StmtKind::Match { arms, .. } => {
                bound.extend(arms.iter().flat_map(|arm| arm.binders.iter().copied()));
            }
            _ => {}
        }
        reachable.push(stmt_id);
        stack.extend(stmt.child_stmts());
    }

    reachable.into_iter().any(|stmt_id| {
        let Some(stmt) = program.stmt(stmt_id) else {
            return false;
        };
        stmt.child_exprs()
            .into_iter()
            .any(|expr| reads_var_outside(program, expr, &bound))
            || matches!(stmt.kind, StmtKind::Resume { resume, .. } if !bound.contains(&resume))
    })
}

fn reads_var_outside(program: &CoreProgram, root: ExprId, bound: &HashSet<VarId>) -> bool {
    any_expr(
        program,
        root,
        &mut |_, expr| matches!(&expr.kind, ExprKind::Var(var) if !bound.contains(var)),
    )
}

/// this is like 85-90% of cases
fn is_tail_resumptive_clause(program: &CoreProgram, stmt_id: StmtId, resume_var: VarId) -> bool {
    let mut memo = HashMap::new();
    let mut visiting = HashSet::new();
    is_tail_resumptive_stmt(program, stmt_id, resume_var, &mut memo, &mut visiting)
}

fn is_tail_resumptive_stmt(
    program: &CoreProgram,
    stmt_id: StmtId,
    resume_var: VarId,
    memo: &mut HashMap<StmtId, bool>,
    visiting: &mut HashSet<StmtId>,
) -> bool {
    if let Some(is_tail) = memo.get(&stmt_id).copied() {
        return is_tail;
    }
    if !visiting.insert(stmt_id) {
        // Statement cycles are conservatively non-tail-resumptive.
        return false;
    }

    let Some(stmt) = program.stmt(stmt_id) else {
        return false;
    };

    let is_tail = match &stmt.kind {
        StmtKind::Return(_) => false,
        StmtKind::Let { value, next, .. } => {
            !expr_mentions_var(program, *value, resume_var)
                && is_tail_resumptive_stmt(program, *next, resume_var, memo, visiting)
        }
        StmtKind::Val {
            binding,
            value,
            next,
        } => {
            if is_identity_return_of_var(program, *next, *binding) {
                is_tail_resumptive_stmt(program, *value, resume_var, memo, visiting)
            } else {
                !stmt_mentions_var(program, *value, resume_var)
                    && is_tail_resumptive_stmt(program, *next, resume_var, memo, visiting)
            }
        }
        // Conservatively reject direct-tail classification through intermediate call/effect nodes.
        // This mirrors the reference tail-resumption caveat for effectful/control-sensitive paths.
        StmtKind::Call { .. } | StmtKind::Perform { .. } => false,
        StmtKind::Resume {
            result,
            resume,
            arg,
            next,
        } => {
            *resume == resume_var
                && !expr_mentions_var(program, *arg, resume_var)
                && is_identity_return_of_var(program, *next, *result)
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            !expr_mentions_var(program, *cond, resume_var)
                && is_tail_resumptive_stmt(program, *then_branch, resume_var, memo, visiting)
                && is_tail_resumptive_stmt(program, *else_branch, resume_var, memo, visiting)
        }
        StmtKind::Match {
            scrutinee,
            arms,
            default,
        } => {
            !expr_mentions_var(program, *scrutinee, resume_var)
                && arms.iter().all(|arm| {
                    is_tail_resumptive_stmt(program, arm.body, resume_var, memo, visiting)
                })
                && default.is_none_or(|stmt| {
                    is_tail_resumptive_stmt(program, stmt, resume_var, memo, visiting)
                })
        }
        StmtKind::Handle { .. } | StmtKind::Stage { .. } => false,
        StmtKind::Hole { .. } | StmtKind::Error(_) => true,
    };
    visiting.remove(&stmt_id);
    memo.insert(stmt_id, is_tail);
    is_tail
}

fn count_resume_sites(program: &CoreProgram, root: StmtId, resume_var: VarId) -> usize {
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    let mut sites = 0;
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if matches!(stmt.kind, StmtKind::Resume { resume, .. } if resume == resume_var) {
            sites += 1;
        }
        stack.extend(stmt.child_stmts());
    }
    sites
}

/// A join point needs a binder even when the perform result is discarded. Var
/// ids are dense from lowering, so one past the largest is unused.
pub(super) fn fresh_var_base(program: &CoreProgram) -> u32 {
    fn bump(next: &mut u32, var: VarId) {
        if var.is_valid() {
            *next = (*next).max(var.as_u32() + 1);
        }
    }

    let mut next = 0;
    for expr in program.exprs() {
        if let ExprKind::Var(var) = expr.kind {
            bump(&mut next, var);
        }
    }
    for stmt in program.stmts() {
        match &stmt.kind {
            StmtKind::Let { binding, .. } | StmtKind::Val { binding, .. } => {
                bump(&mut next, *binding)
            }
            StmtKind::Call { result, .. } => bump(&mut next, *result),
            StmtKind::Perform {
                result: Some(result),
                ..
            } => bump(&mut next, *result),
            StmtKind::Resume { result, resume, .. } => {
                bump(&mut next, *result);
                bump(&mut next, *resume);
            }
            StmtKind::Match { arms, .. } => {
                for binder in arms.iter().flat_map(|arm| arm.binders.iter()) {
                    bump(&mut next, *binder);
                }
            }
            _ => {}
        }
    }
    for param in program
        .functions()
        .iter()
        .flat_map(|func| func.params.iter())
    {
        bump(&mut next, *param);
    }
    for handler in program.handlers() {
        bump(&mut next, handler.return_param);
        for clause in &handler.clauses {
            for param in &clause.params {
                bump(&mut next, *param);
            }
            if let Some(resume) = clause.resume_param {
                bump(&mut next, resume);
            }
        }
    }
    next
}

fn stmt_mentions_var(program: &CoreProgram, root: StmtId, var: VarId) -> bool {
    let mut stack = vec![root];
    let mut seen_stmts = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen_stmts.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        match &stmt.kind {
            StmtKind::Return(expr) => {
                if expr_mentions_var(program, *expr, var) {
                    return true;
                }
            }
            StmtKind::Let { value, next, .. } => {
                if expr_mentions_var(program, *value, var) {
                    return true;
                }
                stack.push(*next);
            }
            StmtKind::Val { value, next, .. } => {
                stack.push(*next);
                stack.push(*value);
            }
            StmtKind::Call { args, next, .. } | StmtKind::Perform { args, next, .. } => {
                if args
                    .iter()
                    .copied()
                    .any(|arg| expr_mentions_var(program, arg, var))
                {
                    return true;
                }
                stack.push(*next);
            }
            StmtKind::Resume {
                resume, arg, next, ..
            } => {
                if *resume == var || expr_mentions_var(program, *arg, var) {
                    return true;
                }
                stack.push(*next);
            }
            StmtKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
                if expr_mentions_var(program, *cond, var) {
                    return true;
                }
                stack.push(*else_branch);
                stack.push(*then_branch);
            }
            StmtKind::Match {
                scrutinee,
                arms,
                default,
            } => {
                if expr_mentions_var(program, *scrutinee, var) {
                    return true;
                }
                if let Some(default_stmt) = default {
                    stack.push(*default_stmt);
                }
                for arm in arms {
                    stack.push(arm.body);
                }
            }
            StmtKind::Handle { body, next, .. } | StmtKind::Stage { body, next, .. } => {
                stack.push(*body);
                if let Some(next_stmt) = next {
                    stack.push(*next_stmt);
                }
            }
            StmtKind::Hole { .. } | StmtKind::Error(_) => {}
        }
    }
    false
}

fn clause_resume_use_range(
    program: &CoreProgram,
    root: StmtId,
    resume_var: VarId,
) -> ResumeUseRange {
    let mut memo = HashMap::new();
    let mut visiting = HashSet::new();
    clause_resume_use_range_stmt(program, root, resume_var, &mut memo, &mut visiting)
}

fn clause_resume_use_range_stmt(
    program: &CoreProgram,
    stmt_id: StmtId,
    resume_var: VarId,
    memo: &mut HashMap<StmtId, ResumeUseRange>,
    visiting: &mut HashSet<StmtId>,
) -> ResumeUseRange {
    if let Some(bound) = memo.get(&stmt_id).copied() {
        return bound;
    }
    if !visiting.insert(stmt_id) {
        // Cycles may resume repeatedly; preserve a safe upper bound.
        return ResumeUseRange::many();
    }

    let Some(stmt) = program.stmt(stmt_id) else {
        return ResumeUseRange::many();
    };

    let bound = match &stmt.kind {
        StmtKind::Return(_) | StmtKind::Hole { .. } | StmtKind::Error(_) => ResumeUseRange::zero(),
        StmtKind::Let { next, .. }
        | StmtKind::Call { next, .. }
        | StmtKind::Perform { next, .. } => {
            clause_resume_use_range_stmt(program, *next, resume_var, memo, visiting)
        }
        StmtKind::Val { value, next, .. } => {
            clause_resume_use_range_stmt(program, *value, resume_var, memo, visiting).plus(
                clause_resume_use_range_stmt(program, *next, resume_var, memo, visiting),
            )
        }
        StmtKind::Resume { resume, next, .. } => {
            let this_resume = if *resume == resume_var {
                ResumeUseRange {
                    min: ResumeUseBound::One,
                    max: ResumeUseBound::One,
                }
            } else {
                ResumeUseRange::zero()
            };
            this_resume.plus(clause_resume_use_range_stmt(
                program, *next, resume_var, memo, visiting,
            ))
        }
        StmtKind::If {
            then_branch,
            else_branch,
            ..
        } => clause_resume_use_range_stmt(program, *then_branch, resume_var, memo, visiting).join(
            clause_resume_use_range_stmt(program, *else_branch, resume_var, memo, visiting),
        ),
        StmtKind::Match { arms, default, .. } => {
            let arms_bound = arms.iter().fold(ResumeUseRange::zero(), |acc, arm| {
                acc.join(clause_resume_use_range_stmt(
                    program, arm.body, resume_var, memo, visiting,
                ))
            });
            if let Some(default_stmt) = default {
                arms_bound.join(clause_resume_use_range_stmt(
                    program,
                    *default_stmt,
                    resume_var,
                    memo,
                    visiting,
                ))
            } else {
                arms_bound
            }
        }
        StmtKind::Handle { body, next, .. } | StmtKind::Stage { body, next, .. } => {
            let body_bound =
                clause_resume_use_range_stmt(program, *body, resume_var, memo, visiting);
            if let Some(next_stmt) = next {
                body_bound.plus(clause_resume_use_range_stmt(
                    program, *next_stmt, resume_var, memo, visiting,
                ))
            } else {
                body_bound
            }
        }
    };

    visiting.remove(&stmt_id);
    memo.insert(stmt_id, bound);
    bound
}

fn expr_mentions_var(program: &CoreProgram, root: ExprId, var: VarId) -> bool {
    any_expr(
        program,
        root,
        &mut |_, expr| matches!(&expr.kind, ExprKind::Var(found) if *found == var),
    )
}
