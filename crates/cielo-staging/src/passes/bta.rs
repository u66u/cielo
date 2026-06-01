// Pass 5/9: bta (binding-time analysis, v0 classifier)
//
// Inputs:
// - Ct-propagated program + ct literal cache
//
// Outputs:
// - Stage table for expressions (`Ct` / `Rt(reason)`)
//
// Invariants:
// - Stage is assigned for every ExprId
// - CT cache membership implies CT stage in v0
//
// Diagnostics:
// - `BTA_CT_ONLY_RUNTIME_ARG` errors when ct-only calls receive runtime args
// - `BTA_COMPTIME_NOT_FOLDABLE` errors when a `@comptime` block stays runtime
//
// Complexity:
// - O(expr_count + stmt_count)

use std::collections::HashSet;

use crate::pipeline::phases::{
    BtaClassified, BtaTables, ClauseDischarge, CtPropagated, HandlerDischarge, Knownness, Reason,
    Stage,
};
use cielo_base::{EffectLabelId, ExprId, HandlerId, StmtId};
use cielo_ir::core::{CoreProgram, ExprKind, StageDirective, StmtKind};
use cielo_ir::effect::{
    EffectFlags, EffectProperties, SortedEffectRow, first_non_thunkable_effect, is_thunkable,
};
use cielo_sema::SemanticTables;
use cielo_sema::ty::Persistability;

pub fn run(ct: CtPropagated) -> BtaClassified {
    let (program, mut diagnostics, sema, mono, ct_tables) = ct.into_parts();
    let mut bta = BtaTables::default();

    for idx in 0..program.exprs().len() {
        let expr_id = ExprId::new(idx);
        if ct_tables.ct_cache.contains_key(&expr_id) {
            bta.stage_of_expr.insert(expr_id, Stage::Ct);
        } else {
            bta.stage_of_expr
                .insert(expr_id, Stage::Rt(Reason::UnclassifiedRuntime));
        }
    }

    let mut visited = HashSet::new();
    for function in program.functions() {
        apply_stage_directives(&program, function.body, None, &mut bta, &mut visited);
    }
    enforce_persistability_boundaries(&program, &sema, &mut bta, &mut diagnostics);
    propagate_runtime_reasons(&program, &mut bta);
    classify_non_thunkable_effects(&program, &sema, &mut bta);
    classify_handler_discharge(&program, &sema, &mut bta);
    enforce_ct_only_calls(&program, &sema, &mut bta, &mut diagnostics);
    // Last of the enforcement passes: it reports the stage a `@comptime` block
    // actually ended up with, so every demotion above has to be recorded first.
    enforce_comptime_blocks(&program, &sema, &bta, &mut diagnostics);
    classify_knownness(&sema, &ct_tables, &mut bta);

    BtaClassified::new(program, diagnostics, sema, mono, ct_tables, bta)
}

fn enforce_persistability_boundaries(
    program: &CoreProgram,
    sema: &SemanticTables,
    bta: &mut BtaTables,
    diagnostics: &mut cielo_base::diagnostics::DiagnosticBag,
) {
    let uses = ExprUseIndex::build(program, BoundaryPolicy::SkipForcedComptime);
    for (idx, expr) in program.exprs().iter().enumerate() {
        let expr_id = ExprId::new(idx);
        if !matches!(bta.stage_of_expr.get(&expr_id), Some(Stage::Ct)) {
            continue;
        }
        if matches!(expr.kind, ExprKind::Literal(_) | ExprKind::Error(_)) {
            continue;
        }
        let Some(type_id) = sema.type_of_expr.get(idx).and_then(|slot| *slot) else {
            continue;
        };
        let is_non_persistable = sema
            .persistability_of_type
            .get(type_id.index())
            .is_some_and(|persistability| *persistability == Persistability::NonPersistable);
        if !is_non_persistable {
            continue;
        }

        let Some(boundary_use) = uses.first_boundary_use(expr_id) else {
            continue;
        };
        let _ = bta
            .stage_of_expr
            .insert(expr_id, Stage::Rt(Reason::NotPersistable(type_id)));
        let boundary_span = program
            .stmt(boundary_use.stmt_id)
            .map(|stmt| stmt.span)
            .unwrap_or(expr.span);
        let boundary_hint = format!(
            " at statement s{} via {}",
            boundary_use.stmt_id.as_u32(),
            boundary_use.kind.describe()
        );
        diagnostics.error(
            "BTA_NOT_PERSISTABLE_BOUNDARY",
            format!(
                "expression e{} has non-persistable type t{} and cannot cross the CT/RT boundary{}",
                expr_id.as_u32(),
                type_id.as_u32(),
                boundary_hint
            ),
            boundary_span,
        );
    }
}

#[derive(Clone, Debug)]
struct ExprUseIndex {
    expr_parents: Vec<Vec<ExprId>>,
    stmt_uses: Vec<Vec<BoundaryUse>>,
}

impl ExprUseIndex {
    fn build(program: &CoreProgram, policy: BoundaryPolicy) -> Self {
        let expr_count = program.exprs().len();
        let mut expr_parents = vec![Vec::new(); expr_count];
        for (parent_idx, expr) in program.exprs().iter().enumerate() {
            let parent_id = ExprId::new(parent_idx);
            for operand in expr.kind.child_exprs() {
                if operand.index() < expr_count {
                    expr_parents[operand.index()].push(parent_id);
                }
            }
        }

        let mut collector = UseCollector {
            program,
            policy,
            visited: HashSet::new(),
            stmt_uses: vec![Vec::new(); expr_count],
            expr_count,
        };
        for function in program.functions() {
            collector.collect(function.body, UseContext::Unknown);
        }
        for handler in program.handlers() {
            collector.collect(handler.return_body, UseContext::Unknown);
            for clause in &handler.clauses {
                collector.collect(clause.body, UseContext::Unknown);
            }
        }

        Self {
            expr_parents,
            stmt_uses: collector.stmt_uses,
        }
    }

    fn first_boundary_use(&self, expr_id: ExprId) -> Option<BoundaryUse> {
        let mut stack = vec![expr_id];
        let mut seen = HashSet::new();
        while let Some(current) = stack.pop() {
            if !seen.insert(current) {
                continue;
            }
            if let Some(boundary_use) = self
                .stmt_uses
                .get(current.index())
                .and_then(|uses| uses.first())
                .copied()
            {
                return Some(boundary_use);
            }
            if let Some(parents) = self.expr_parents.get(current.index()) {
                stack.extend(parents.iter().copied());
            }
        }
        None
    }
}

#[derive(Clone, Copy, Debug)]
struct BoundaryUse {
    stmt_id: StmtId,
    kind: BoundaryUseKind,
}

/// Whether uses inside a `@comptime` block count as CT/RT boundary crossings.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BoundaryPolicy {
    /// A `@comptime` block never reaches runtime -- `enforce_comptime_blocks`
    /// rejects it otherwise -- so its uses cross no boundary.
    SkipForcedComptime,
    /// Every use, including the ones above. `enforce_comptime_blocks` needs to
    /// point at exactly the uses the persistability check is allowed to ignore.
    IncludeForcedComptime,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum UseContext {
    Unknown,
    ForcedComptime,
    ForcedRuntime,
}

impl UseContext {
    fn encode(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::ForcedComptime => 1,
            Self::ForcedRuntime => 2,
        }
    }

    fn boundary_enabled(self, policy: BoundaryPolicy) -> bool {
        policy == BoundaryPolicy::IncludeForcedComptime || self != Self::ForcedComptime
    }

    fn from_stage(stage: StageDirective) -> Self {
        match stage {
            StageDirective::Comptime => Self::ForcedComptime,
            StageDirective::Runtime => Self::ForcedRuntime,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum BoundaryUseKind {
    ReturnValue,
    LetValue,
    CallArg(usize),
    PerformArg(usize),
    ResumeArg,
    IfCondition,
    MatchScrutinee,
}

impl BoundaryUseKind {
    fn describe(self) -> String {
        match self {
            Self::ReturnValue => "return-value".to_owned(),
            Self::LetValue => "let-value".to_owned(),
            Self::CallArg(index) => format!("call-arg#{index}"),
            Self::PerformArg(index) => format!("perform-arg#{index}"),
            Self::ResumeArg => "resume-arg".to_owned(),
            Self::IfCondition => "if-condition".to_owned(),
            Self::MatchScrutinee => "match-scrutinee".to_owned(),
        }
    }
}

struct UseCollector<'a> {
    program: &'a CoreProgram,
    policy: BoundaryPolicy,
    visited: HashSet<(StmtId, u8)>,
    stmt_uses: Vec<Vec<BoundaryUse>>,
    expr_count: usize,
}

impl UseCollector<'_> {
    fn push(&mut self, expr: ExprId, stmt_id: StmtId, context: UseContext, kind: BoundaryUseKind) {
        if !context.boundary_enabled(self.policy) || expr.index() >= self.expr_count {
            return;
        }
        self.stmt_uses[expr.index()].push(BoundaryUse { stmt_id, kind });
    }

    fn collect(&mut self, stmt_id: StmtId, context: UseContext) {
        if !self.visited.insert((stmt_id, context.encode())) {
            return;
        }
        // Copied out so the borrow of the statement below is on the program,
        // not on `self`, leaving the recursive calls free to take `&mut self`.
        let program = self.program;
        let Some(stmt) = program.stmt(stmt_id) else {
            return;
        };

        match &stmt.kind {
            StmtKind::Return(expr) => {
                self.push(*expr, stmt_id, context, BoundaryUseKind::ReturnValue);
            }
            StmtKind::Let { value, next, .. } => {
                self.push(*value, stmt_id, context, BoundaryUseKind::LetValue);
                self.collect(*next, context);
            }
            StmtKind::Val { value, next, .. } => {
                self.collect(*value, context);
                self.collect(*next, context);
            }
            StmtKind::Call { args, next, .. } => {
                for (arg_idx, arg) in args.iter().copied().enumerate() {
                    self.push(arg, stmt_id, context, BoundaryUseKind::CallArg(arg_idx));
                }
                self.collect(*next, context);
            }
            StmtKind::Resume { arg, next, .. } => {
                self.push(*arg, stmt_id, context, BoundaryUseKind::ResumeArg);
                self.collect(*next, context);
            }
            StmtKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
                self.push(*cond, stmt_id, context, BoundaryUseKind::IfCondition);
                self.collect(*then_branch, context);
                self.collect(*else_branch, context);
            }
            StmtKind::Match {
                scrutinee,
                arms,
                default,
            } => {
                self.push(
                    *scrutinee,
                    stmt_id,
                    context,
                    BoundaryUseKind::MatchScrutinee,
                );
                for arm in arms {
                    self.collect(arm.body, context);
                }
                if let Some(default_stmt) = default {
                    self.collect(*default_stmt, context);
                }
            }
            StmtKind::Perform { args, next, .. } => {
                for (arg_idx, arg) in args.iter().copied().enumerate() {
                    self.push(arg, stmt_id, context, BoundaryUseKind::PerformArg(arg_idx));
                }
                self.collect(*next, context);
            }
            StmtKind::Handle { body, next, .. } => {
                self.collect(*body, context);
                if let Some(next_stmt) = next {
                    self.collect(*next_stmt, context);
                }
            }
            StmtKind::Stage { stage, body, next } => {
                self.collect(*body, UseContext::from_stage(*stage));
                if let Some(next_stmt) = next {
                    self.collect(*next_stmt, context);
                }
            }
            StmtKind::Hole { .. } | StmtKind::Error(_) => {}
        }
    }
}

#[derive(Clone, Copy)]
enum ForcedStage {
    Ct,
    Rt,
}

fn apply_stage_directives(
    program: &CoreProgram,
    stmt_id: StmtId,
    forced: Option<ForcedStage>,
    bta: &mut BtaTables,
    visited: &mut HashSet<(StmtId, u8)>,
) {
    let key = (
        stmt_id,
        match forced {
            None => 0,
            Some(ForcedStage::Ct) => 1,
            Some(ForcedStage::Rt) => 2,
        },
    );
    if !visited.insert(key) {
        return;
    }

    let Some(stmt) = program.stmt(stmt_id) else {
        return;
    };

    match &stmt.kind {
        StmtKind::Return(expr) => apply_forced_expr(*expr, forced, bta),
        StmtKind::Let { value, next, .. } => {
            apply_forced_expr(*value, forced, bta);
            apply_stage_directives(program, *next, forced, bta, visited);
        }
        StmtKind::Val { value, next, .. } => {
            apply_stage_directives(program, *value, forced, bta, visited);
            apply_stage_directives(program, *next, forced, bta, visited);
        }
        StmtKind::Call { args, next, .. } => {
            for arg in args {
                apply_forced_expr(*arg, forced, bta);
            }
            apply_stage_directives(program, *next, forced, bta, visited);
        }
        StmtKind::Resume { arg, next, .. } => {
            apply_forced_expr(*arg, forced, bta);
            apply_stage_directives(program, *next, forced, bta, visited);
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            apply_forced_expr(*cond, forced, bta);
            apply_stage_directives(program, *then_branch, forced, bta, visited);
            apply_stage_directives(program, *else_branch, forced, bta, visited);
        }
        StmtKind::Match {
            scrutinee,
            arms,
            default,
        } => {
            apply_forced_expr(*scrutinee, forced, bta);
            for arm in arms {
                apply_stage_directives(program, arm.body, forced, bta, visited);
            }
            if let Some(default_stmt) = default {
                apply_stage_directives(program, *default_stmt, forced, bta, visited);
            }
        }
        StmtKind::Perform { args, next, .. } => {
            for arg in args {
                apply_forced_expr(*arg, forced, bta);
            }
            apply_stage_directives(program, *next, forced, bta, visited);
        }
        StmtKind::Handle { body, next, .. } => {
            apply_stage_directives(program, *body, forced, bta, visited);
            if let Some(next_stmt) = next {
                apply_stage_directives(program, *next_stmt, forced, bta, visited);
            }
        }
        StmtKind::Stage { stage, body, next } => {
            let inner = Some(match stage {
                StageDirective::Comptime => ForcedStage::Ct,
                StageDirective::Runtime => ForcedStage::Rt,
            });
            apply_stage_directives(program, *body, inner, bta, visited);
            if let Some(next_stmt) = next {
                apply_stage_directives(program, *next_stmt, forced, bta, visited);
            }
        }
        StmtKind::Hole { .. } | StmtKind::Error(_) => {}
    }
}

fn apply_forced_expr(expr_id: ExprId, forced: Option<ForcedStage>, bta: &mut BtaTables) {
    match forced {
        Some(ForcedStage::Ct) => {
            bta.stage_of_expr.insert(expr_id, Stage::Ct);
        }
        Some(ForcedStage::Rt) => {
            bta.stage_of_expr
                .insert(expr_id, Stage::Rt(Reason::UserForcedRuntime));
        }
        None => {}
    }
}

fn propagate_runtime_reasons(program: &CoreProgram, bta: &mut BtaTables) {
    let limit = program
        .exprs()
        .len()
        .saturating_add(program.stmts().len())
        .max(1);
    for _ in 0..limit {
        let mut changed = false;
        changed |= propagate_var_reasons(program, bta);
        changed |= propagate_expr_reasons(program, bta);
        if !changed {
            break;
        }
    }
}

fn propagate_var_reasons(program: &CoreProgram, bta: &mut BtaTables) -> bool {
    let mut changed = false;
    for stmt in program.stmts() {
        match &stmt.kind {
            StmtKind::Let { binding, value, .. } => {
                let Some(Stage::Rt(reason)) = bta.stage_of_expr.get(value).copied() else {
                    continue;
                };
                changed |= refine_var_stage(*binding, reason, bta);
            }
            StmtKind::Val { binding, value, .. } => {
                let Some(reason) = find_stmt_runtime_reason(program, bta, *value) else {
                    continue;
                };
                changed |= refine_var_stage(*binding, reason, bta);
            }
            StmtKind::Call { result, args, .. }
            | StmtKind::Perform {
                result: Some(result),
                args,
                ..
            } => {
                let Some(reason) = args.iter().find_map(|arg| {
                    bta.stage_of_expr.get(arg).and_then(|stage| match stage {
                        Stage::Rt(reason) => Some(*reason),
                        Stage::Ct => None,
                    })
                }) else {
                    continue;
                };
                changed |= refine_var_stage(*result, reason, bta);
            }
            StmtKind::Resume { result, arg, .. } => {
                let Some(Stage::Rt(reason)) = bta.stage_of_expr.get(arg).copied() else {
                    continue;
                };
                changed |= refine_var_stage(*result, reason, bta);
            }
            StmtKind::Return(_)
            | StmtKind::If { .. }
            | StmtKind::Match { .. }
            | StmtKind::Perform { result: None, .. }
            | StmtKind::Handle { .. }
            | StmtKind::Stage { .. }
            | StmtKind::Hole { .. }
            | StmtKind::Error(_) => {}
        }
    }
    changed
}

fn propagate_expr_reasons(program: &CoreProgram, bta: &mut BtaTables) -> bool {
    let mut changed = false;
    for (idx, expr) in program.exprs().iter().enumerate() {
        let expr_id = ExprId::new(idx);
        if matches!(bta.stage_of_expr.get(&expr_id), Some(Stage::Ct)) {
            continue;
        }
        let Some(reason) = infer_expr_runtime_reason(bta, &expr.kind) else {
            continue;
        };
        changed |= refine_expr_stage(expr_id, reason, bta);
    }
    changed
}

/// A `Var` carries its own stage; every other node inherits the reason of its
/// first runtime operand, so operand order decides which reason is reported.
fn infer_expr_runtime_reason(bta: &BtaTables, kind: &ExprKind) -> Option<Reason> {
    match kind {
        ExprKind::Var(var) => {
            return matches!(bta.stage_of_var.get(var), Some(Stage::Rt(_)))
                .then_some(Reason::DependsOnVar(*var));
        }
        // Output is observable, so a builtin stays runtime however static its
        // arguments are. Inheriting from operands would fold the call away.
        // A closure has no compile-time value either: `ct_eval` yields
        // literals, and neither a function value nor a call through one is one.
        ExprKind::BuiltinCall { .. }
        | ExprKind::MakeClosure { .. }
        | ExprKind::CallClosure { .. } => return Some(Reason::UnclassifiedRuntime),
        _ => {}
    }
    kind.child_exprs()
        .into_iter()
        .find_map(|operand| stage_reason_of_expr(bta, operand))
}

fn stage_reason_of_expr(bta: &BtaTables, expr: ExprId) -> Option<Reason> {
    bta.stage_of_expr.get(&expr).and_then(|stage| match stage {
        Stage::Ct => None,
        Stage::Rt(reason) => Some(*reason),
    })
}

fn find_stmt_runtime_reason(
    program: &CoreProgram,
    bta: &BtaTables,
    root: StmtId,
) -> Option<Reason> {
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
            StmtKind::Return(expr) => {
                if let Some(reason) = stage_reason_of_expr(bta, *expr) {
                    return Some(reason);
                }
            }
            StmtKind::Let { value, next, .. } => {
                if let Some(reason) = stage_reason_of_expr(bta, *value) {
                    return Some(reason);
                }
                stack.push(*next);
            }
            StmtKind::Val { value, next, .. } => {
                stack.push(*next);
                stack.push(*value);
            }
            StmtKind::Call { args, next, .. } | StmtKind::Perform { args, next, .. } => {
                if let Some(reason) = args.iter().find_map(|arg| stage_reason_of_expr(bta, *arg)) {
                    return Some(reason);
                }
                stack.push(*next);
            }
            StmtKind::Resume { arg, next, .. } => {
                if let Some(reason) = stage_reason_of_expr(bta, *arg) {
                    return Some(reason);
                }
                stack.push(*next);
            }
            StmtKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
                if stage_reason_of_expr(bta, *cond).is_some() {
                    return Some(Reason::BranchOnRuntime(*cond));
                }
                stack.push(*else_branch);
                stack.push(*then_branch);
            }
            StmtKind::Match {
                scrutinee,
                arms,
                default,
            } => {
                if let Some(reason) = stage_reason_of_expr(bta, *scrutinee) {
                    return Some(reason);
                }
                if let Some(default_stmt) = default {
                    stack.push(*default_stmt);
                }
                for arm in arms {
                    stack.push(arm.body);
                }
            }
            StmtKind::Handle { body, next, .. } | StmtKind::Stage { body, next, .. } => {
                if let Some(next_stmt) = next {
                    stack.push(*next_stmt);
                }
                stack.push(*body);
            }
            StmtKind::Hole { .. } | StmtKind::Error(_) => {}
        }
    }
    None
}

/// Reports a change only when the reason actually moves. Rewriting
/// `UnclassifiedRuntime` with itself is a no-op, and counting it as progress
/// stops the fixpoint from converging.
fn refine_expr_stage(expr_id: ExprId, reason: Reason, bta: &mut BtaTables) -> bool {
    match bta.stage_of_expr.get(&expr_id).copied() {
        Some(Stage::Ct) => false,
        Some(Stage::Rt(Reason::UnclassifiedRuntime)) if reason != Reason::UnclassifiedRuntime => {
            bta.stage_of_expr.insert(expr_id, Stage::Rt(reason));
            true
        }
        Some(Stage::Rt(_)) => false,
        None => {
            bta.stage_of_expr.insert(expr_id, Stage::Rt(reason));
            true
        }
    }
}

fn refine_var_stage(var_id: cielo_base::VarId, reason: Reason, bta: &mut BtaTables) -> bool {
    match bta.stage_of_var.get(&var_id).copied() {
        Some(Stage::Ct) => false,
        Some(Stage::Rt(Reason::UnclassifiedRuntime)) if reason != Reason::UnclassifiedRuntime => {
            bta.stage_of_var.insert(var_id, Stage::Rt(reason));
            true
        }
        Some(Stage::Rt(_)) => false,
        None => {
            bta.stage_of_var.insert(var_id, Stage::Rt(reason));
            true
        }
    }
}

fn is_runtime_expr(expr_id: ExprId, bta: &BtaTables) -> bool {
    !matches!(bta.stage_of_expr.get(&expr_id), Some(Stage::Ct))
}

fn enforce_ct_only_calls(
    program: &CoreProgram,
    sema: &SemanticTables,
    bta: &mut BtaTables,
    diagnostics: &mut cielo_base::diagnostics::DiagnosticBag,
) {
    let uses = ExprUseIndex::build(program, BoundaryPolicy::SkipForcedComptime);

    for (idx, expr) in program.exprs().iter().enumerate() {
        let ExprKind::PureCall { callee, args } = &expr.kind else {
            continue;
        };
        let is_ct_only = program
            .function(*callee)
            .is_some_and(|function| is_ct_only_function(function, sema));
        if !is_ct_only || !args.iter().copied().any(|arg| is_runtime_expr(arg, bta)) {
            continue;
        }

        let expr_id = ExprId::new(idx);
        bta.stage_of_expr
            .insert(expr_id, Stage::Rt(Reason::CtOnlyWithRuntimeArgs(*callee)));

        let role_hint = uses
            .first_boundary_use(expr_id)
            .map(|u| {
                format!(
                    " at statement s{} via {}",
                    u.stmt_id.as_u32(),
                    u.kind.describe()
                )
            })
            .unwrap_or_default();

        diagnostics.error(
            "BTA_CT_ONLY_RUNTIME_ARG",
            format!(
                "ct-only function call f{} has runtime arguments{}; this call cannot be residualized",
                callee.as_u32(),
                role_hint
            ),
            expr.span,
        );
    }

    for (stmt_idx, stmt) in program.stmts().iter().enumerate() {
        let StmtKind::Call {
            result,
            callee,
            args,
            ..
        } = &stmt.kind
        else {
            continue;
        };

        let is_ct_only = program
            .function(*callee)
            .is_some_and(|function| is_ct_only_function(function, sema));
        if !is_ct_only || !args.iter().copied().any(|arg| is_runtime_expr(arg, bta)) {
            continue;
        }

        bta.stage_of_var
            .insert(*result, Stage::Rt(Reason::CtOnlyWithRuntimeArgs(*callee)));

        let stmt_id = StmtId::new(stmt_idx);
        let role_hint = format!(" at statement s{}", stmt_id.as_u32());

        diagnostics.error(
            "BTA_CT_ONLY_RUNTIME_ARG",
            format!(
                "ct-only function call f{} has runtime arguments{}; this call cannot be residualized",
                callee.as_u32(),
                role_hint
            ),
            stmt.span,
        );
    }
}

/// Why a `@comptime` block failed to fold. The effect case is checked first
/// and separately: a `perform` whose result is discarded leaves no expression
/// to blame at all, and where it does leave one the expression only says
/// "runtime", not which effect the block is stuck on.
enum ComptimeFailure {
    EscapingEffect(EffectLabelId),
    RuntimeExpr(ExprId, Reason),
}

/// `@comptime` is an assertion, not a preference. A block that cannot be
/// evaluated at compile time is an error, not a silent downgrade to runtime
/// code -- otherwise the annotation is unverifiable and `boundary_enabled`
/// skipping persistability checks inside it has nothing backing it.
fn enforce_comptime_blocks(
    program: &CoreProgram,
    sema: &SemanticTables,
    bta: &BtaTables,
    diagnostics: &mut cielo_base::diagnostics::DiagnosticBag,
) {
    let uses = ExprUseIndex::build(program, BoundaryPolicy::IncludeForcedComptime);
    // Monomorphization and handler specialization clone a block once per
    // instantiation. The user wrote it once, so report it once.
    let mut reported = HashSet::new();

    for stmt in program.stmts() {
        let StmtKind::Stage {
            stage: StageDirective::Comptime,
            body,
            ..
        } = &stmt.kind
        else {
            continue;
        };
        let Some(failure) = comptime_block_failure(program, sema, bta, *body) else {
            continue;
        };
        if !reported.insert(stmt.span) {
            continue;
        }

        let cause = match failure {
            ComptimeFailure::EscapingEffect(effect) => format!(
                "effect e{} escapes the block and needs a runtime handler",
                effect.as_u32()
            ),
            ComptimeFailure::RuntimeExpr(expr_id, reason) => {
                let role_hint = uses
                    .first_boundary_use(expr_id)
                    .map(|u| {
                        format!(
                            " at statement s{} via {}",
                            u.stmt_id.as_u32(),
                            u.kind.describe()
                        )
                    })
                    .unwrap_or_default();
                format!(
                    "expression e{}{} is runtime because {}",
                    expr_id.as_u32(),
                    role_hint,
                    reason.describe_runtime()
                )
            }
        };

        diagnostics.error(
            "BTA_COMPTIME_NOT_FOLDABLE",
            format!("@comptime block cannot be evaluated at compile time: {cause}"),
            stmt.span,
        );
    }
}

fn comptime_block_failure(
    program: &CoreProgram,
    sema: &SemanticTables,
    bta: &BtaTables,
    body: StmtId,
) -> Option<ComptimeFailure> {
    if let Some(effect) = escaping_runtime_effect(sema, body) {
        return Some(ComptimeFailure::EscapingEffect(effect));
    }
    first_runtime_expr(program, bta, body)
        .map(|(expr_id, reason)| ComptimeFailure::RuntimeExpr(expr_id, reason))
}

/// A ct-only effect is discharged during staging, so it is the one kind of
/// effect a `@comptime` block exists to perform. Any other effect still in the
/// block's row escaped every handler written inside it.
fn escaping_runtime_effect(sema: &SemanticTables, body: StmtId) -> Option<EffectLabelId> {
    sema.effects_of_stmt
        .get(body.index())?
        .iter()
        .find(|effect| {
            !sema
                .effect_properties
                .get(effect)
                .is_some_and(|props| props.flags.contains(EffectFlags::CT_ONLY))
        })
}

/// Lowest-numbered expression in the block that did not reach the compile-time
/// stage, subexpressions included.
fn first_runtime_expr(
    program: &CoreProgram,
    bta: &BtaTables,
    body: StmtId,
) -> Option<(ExprId, Reason)> {
    let mut stack = Vec::new();
    collect_region_exprs(program, body, &mut stack, &mut HashSet::new());

    let mut best: Option<(ExprId, Reason)> = None;
    let mut seen = HashSet::new();
    while let Some(expr_id) = stack.pop() {
        if !seen.insert(expr_id) {
            continue;
        }
        if let Some(reason) = comptime_expr_failure(program, bta, expr_id)
            && best.is_none_or(|(current, _)| expr_id.index() < current.index())
        {
            best = Some((expr_id, reason));
        }
        if let Some(expr) = program.expr(expr_id) {
            stack.extend(expr.kind.child_exprs());
        }
    }
    best
}

/// Why an expression failed to stage `Ct`, `None` if it did not fail. Two
/// paths, because `apply_forced_expr` stamps `Ct` over every statement-level
/// slot in the block and so destroys the evidence for the second:
///
/// - a `Rt` stage of its own, which is how a runtime operand shows up: the
///   stamp does not reach subexpressions, so it survives under a `Ct` parent
/// - reading a runtime variable, which is how a stamped slot shows up.
///   `stage_of_var` is never stamped, so it still says what the stage would
///   have been.
fn comptime_expr_failure(
    program: &CoreProgram,
    bta: &BtaTables,
    expr_id: ExprId,
) -> Option<Reason> {
    if let Some(ExprKind::Var(var)) = program.expr(expr_id).map(|node| &node.kind)
        && let Some(Stage::Rt(reason)) = bta.stage_of_var.get(var).copied()
    {
        return Some(root_reason(bta, reason));
    }
    stage_reason_of_expr(bta, expr_id).map(|reason| root_reason(bta, reason))
}

/// Everything lexically inside the block, a nested `@runtime` block included:
/// `@comptime` asserts the whole block folds, so an inner `@runtime` is a
/// contradiction rather than an exemption. Handler clause bodies hang off
/// `program.handlers()` rather than off the `Handle` statement and are staged
/// on their own, so they are not part of any block.
fn collect_region_exprs(
    program: &CoreProgram,
    stmt_id: StmtId,
    out: &mut Vec<ExprId>,
    visited: &mut HashSet<StmtId>,
) {
    if !visited.insert(stmt_id) {
        return;
    }
    let Some(stmt) = program.stmt(stmt_id) else {
        return;
    };

    match &stmt.kind {
        StmtKind::Return(expr) => out.push(*expr),
        StmtKind::Let { value, next, .. } => {
            out.push(*value);
            collect_region_exprs(program, *next, out, visited);
        }
        StmtKind::Val { value, next, .. } => {
            collect_region_exprs(program, *value, out, visited);
            collect_region_exprs(program, *next, out, visited);
        }
        StmtKind::Call { args, next, .. } | StmtKind::Perform { args, next, .. } => {
            out.extend(args.iter().copied());
            collect_region_exprs(program, *next, out, visited);
        }
        StmtKind::Resume { arg, next, .. } => {
            out.push(*arg);
            collect_region_exprs(program, *next, out, visited);
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            out.push(*cond);
            collect_region_exprs(program, *then_branch, out, visited);
            collect_region_exprs(program, *else_branch, out, visited);
        }
        StmtKind::Match {
            scrutinee,
            arms,
            default,
        } => {
            out.push(*scrutinee);
            for arm in arms {
                collect_region_exprs(program, arm.body, out, visited);
            }
            if let Some(default_stmt) = default {
                collect_region_exprs(program, *default_stmt, out, visited);
            }
        }
        StmtKind::Handle { body, next, .. } | StmtKind::Stage { body, next, .. } => {
            collect_region_exprs(program, *body, out, visited);
            if let Some(next_stmt) = next {
                collect_region_exprs(program, *next_stmt, out, visited);
            }
        }
        StmtKind::Hole { .. } | StmtKind::Error(_) => {}
    }
}

/// `DependsOnVar` names a variable, not a cause. Follow the variable's own
/// stage so the message says why the variable is runtime rather than only that
/// it is -- the same move `provenance::find_terminal_reason` makes for the
/// staging report's root-cause table, minus its walk back to variable
/// definitions, which the report needs and a single diagnostic does not.
fn root_reason(bta: &BtaTables, reason: Reason) -> Reason {
    let mut current = reason;
    let mut seen = HashSet::new();
    while let Reason::DependsOnVar(var) = current {
        if !seen.insert(var) {
            break;
        }
        match bta.stage_of_var.get(&var).copied() {
            Some(Stage::Rt(next)) if next != Reason::UnclassifiedRuntime => current = next,
            _ => break,
        }
    }
    current
}

fn is_ct_only_function(function: &cielo_ir::core::FunctionDecl, sema: &SemanticTables) -> bool {
    function.ct_only
        || function.declared_effects.iter().any(|effect| {
            sema.effect_properties
                .get(&effect)
                .is_some_and(|props| props.flags.contains(EffectFlags::CT_ONLY))
        })
}

fn classify_non_thunkable_effects(
    program: &CoreProgram,
    sema: &SemanticTables,
    bta: &mut BtaTables,
) {
    for stmt in program.stmts() {
        match &stmt.kind {
            StmtKind::Call {
                result, effects, ..
            } => {
                if let Some(effect) = blocking_effect(effects, &sema.effect_properties) {
                    bta.stage_of_var
                        .insert(*result, Stage::Rt(Reason::EffectNotDischarged(effect)));
                }
            }
            StmtKind::Perform { result, effect, .. } => {
                let Some(result) = result else {
                    continue;
                };
                let row = SortedEffectRow::singleton(*effect);
                if let Some(blocking) = blocking_effect(&row, &sema.effect_properties) {
                    bta.stage_of_var
                        .insert(*result, Stage::Rt(Reason::EffectNotDischarged(blocking)));
                }
            }
            _ => {}
        }
    }
}

fn classify_handler_discharge(program: &CoreProgram, sema: &SemanticTables, bta: &mut BtaTables) {
    for (idx, handler) in program.handlers().iter().enumerate() {
        let handler_id = HandlerId::new(idx);
        let mut clause_statuses = Vec::with_capacity(handler.clauses.len());
        for clause in &handler.clauses {
            let reason = blocking_stmt_effect_reason(sema, clause.body);
            clause_statuses.push(ClauseDischarge {
                dischargeable: reason.is_none(),
                reason,
            });
        }

        let handler_reason = blocking_stmt_effect_reason(sema, handler.return_body)
            .or_else(|| clause_statuses.iter().find_map(|status| status.reason));
        bta.handler_discharge.insert(
            handler_id,
            HandlerDischarge {
                dischargeable: handler_reason.is_none(),
                reason: handler_reason,
            },
        );
        bta.clause_discharge.insert(handler_id, clause_statuses);
    }
}

fn blocking_stmt_effect_reason(sema: &SemanticTables, stmt_id: StmtId) -> Option<Reason> {
    sema.effects_of_stmt
        .get(stmt_id.index())
        .and_then(|row| blocking_effect(row, &sema.effect_properties))
        .map(Reason::EffectNotDischarged)
}

fn blocking_effect(
    row: &SortedEffectRow,
    effect_props: &std::collections::HashMap<EffectLabelId, EffectProperties>,
) -> Option<EffectLabelId> {
    if is_thunkable(row, effect_props) {
        return None;
    }
    first_non_thunkable_effect(row, effect_props)
}

fn classify_knownness(
    sema: &SemanticTables,
    ct: &crate::pipeline::phases::CtPropagationTables,
    bta: &mut BtaTables,
) {
    for (idx, _expr_ty) in sema.type_of_expr.iter().enumerate() {
        let expr_id = ExprId::new(idx);
        let knownness = if !ct.ct_cache.contains_key(&expr_id) {
            Knownness::Unknown
        } else {
            let persistable_by_value = ct
                .ct_cache
                .get(&expr_id)
                .is_some_and(is_trivially_persistable_literal);
            if persistable_by_value {
                Knownness::KnownPersistable
            } else {
                let persistability = sema
                    .type_of_expr
                    .get(idx)
                    .and_then(|slot| *slot)
                    .and_then(|type_id| sema.persistability_of_type.get(type_id.index()).copied());

                if matches!(persistability, Some(Persistability::NonPersistable)) {
                    Knownness::KnownLocal
                } else {
                    let persistable_by_type = persistability.is_some();
                    if persistable_by_type {
                        Knownness::KnownPersistable
                    } else {
                        Knownness::KnownLocal
                    }
                }
            }
        };
        bta.knownness_of_expr.insert(expr_id, knownness);
    }
}

fn is_trivially_persistable_literal(literal: &cielo_ir::core::Literal) -> bool {
    matches!(
        literal,
        cielo_ir::core::Literal::Unit
            | cielo_ir::core::Literal::Bool(_)
            | cielo_ir::core::Literal::Int(_)
            | cielo_ir::core::Literal::Float(_)
            | cielo_ir::core::Literal::Char(_)
            | cielo_ir::core::Literal::String(_)
    )
}
