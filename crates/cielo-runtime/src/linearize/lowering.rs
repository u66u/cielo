use std::collections::HashMap;

use cielo_base::Span;
use cielo_base::diagnostics::DiagnosticBag;
use cielo_base::ids::{
    EffectLabelId, ExprId, FuncId, LinearExprId, LinearFuncId, LinearStmtId, ResumptionId, StmtId,
    SymbolId, VarId,
};
use cielo_ir::core::{CoreProgram, ExprKind, HandlerClause, HandlerDef, StmtKind};
use cielo_ir::effect::{SortedEffectRow, is_thunkable};
use cielo_ir::function_graph::collect_reachable_functions;
use cielo_ir::linear::{
    CallConvention, HandlerOutcome, HandlerSite, LinearExpr, LinearFunction, LinearHandlerClause,
    LinearMatchArm, LinearProgram, LinearStmt,
};
use cielo_sema::SemanticTables;

use super::analysis::{
    ResidualBlocker, analyze_clause_resume, classify_clause_convention, clause_policy,
    core_stmt_calls_performing_effect, fresh_var_base, is_identity_handler_return_clause,
    is_identity_return_of_var, linear_stmt_contains_perform_effect, resume_convention_reason,
    stmt_effect_row_contains,
};
use super::types::{
    ClauseConvention, ClausePolicy, PerformReach, ResumeContext, ResumeQualifier, ResumeStrategy,
};

struct LoweringInput<'a> {
    program: &'a CoreProgram,
    fn_names: &'a HashMap<FuncId, SymbolId>,
    fn_ids: &'a HashMap<FuncId, LinearFuncId>,
    sema: &'a SemanticTables,
}

impl LoweringInput<'_> {
    /// Specializations share a name symbol, so the dense id is the only thing
    /// that distinguishes them downstream.
    fn callee_fn(&self, callee: &FuncId) -> LinearFuncId {
        self.fn_ids
            .get(callee)
            .copied()
            .unwrap_or(LinearFuncId::INVALID)
    }
}

/// Clauses that cannot be join-merged still re-lower the continuation at every
/// `Resume` rather than sharing it, so they expand as k^n. Without a ceiling a
/// handful of nesting levels emits tens of megabytes of C with no diagnostic.
/// Sized well above any realistic program.
const HANDLER_INLINE_STMT_BUDGET: usize = 200_000;

struct LoweringState<'a> {
    diagnostics: &'a mut DiagnosticBag,
    linear: &'a mut LinearProgram,
    expr_map: &'a mut [Option<LinearExprId>],
    stmt_map: &'a mut [Option<LinearStmtId>],
    inline_budget_exhausted: bool,
    next_var: u32,
    next_resumption: u32,
    /// Sites handed out per resumption so far. The dispatch is an integer
    /// switch, so the labels have to be dense from zero.
    resume_labels: HashMap<ResumptionId, u32>,
}

impl LoweringState<'_> {
    fn fresh_var(&mut self) -> VarId {
        let var = VarId::from_u32(self.next_var);
        self.next_var += 1;
        var
    }

    fn fresh_resumption(&mut self) -> ResumptionId {
        let id = ResumptionId::from_u32(self.next_resumption);
        self.next_resumption += 1;
        id
    }

    fn next_resume_label(&mut self, resumption: ResumptionId) -> u32 {
        let next = self.resume_labels.entry(resumption).or_default();
        let label = *next;
        *next += 1;
        label
    }

    /// True once expansion has been abandoned. Reported once so a deeply
    /// nested program does not bury the log in identical errors.
    fn inline_budget_exceeded(&mut self, span: Span) -> bool {
        if self.linear.stmts().len() <= HANDLER_INLINE_STMT_BUDGET {
            return false;
        }
        if !self.inline_budget_exhausted {
            self.inline_budget_exhausted = true;
            self.diagnostics.error(
                "LINEARIZE_INLINE_BUDGET_EXCEEDED",
                "handler not erased: inlining exceeded its statement budget",
                span,
            );
        }
        true
    }

    fn record_handler(&mut self, effect: EffectLabelId, span: Span, outcome: HandlerOutcome) {
        self.linear.handler_sites.push(HandlerSite {
            effect,
            span,
            outcome,
        });
    }
}

pub(super) fn lower_program(
    program: &CoreProgram,
    sema: &SemanticTables,
    diagnostics: &mut DiagnosticBag,
) -> LinearProgram {
    let fn_names: HashMap<FuncId, SymbolId> = program
        .functions()
        .iter()
        .enumerate()
        .map(|(idx, func)| (FuncId::new(idx), func.name))
        .collect();

    let mut linear = LinearProgram::default();
    let mut expr_map: Vec<Option<LinearExprId>> = vec![None; program.exprs().len()];
    let mut stmt_map: Vec<Option<LinearStmtId>> = vec![None; program.stmts().len()];
    let reachable = collect_reachable_functions(program);
    let dense_id_by_source: HashMap<FuncId, LinearFuncId> = reachable
        .iter()
        .copied()
        .enumerate()
        .map(|(dense_idx, source_id)| (source_id, LinearFuncId::new(dense_idx)))
        .collect();
    let input = LoweringInput {
        program,
        fn_names: &fn_names,
        fn_ids: &dense_id_by_source,
        sema,
    };
    {
        let mut state = LoweringState {
            diagnostics,
            linear: &mut linear,
            expr_map: &mut expr_map,
            stmt_map: &mut stmt_map,
            inline_budget_exhausted: false,
            next_var: fresh_var_base(program),
            next_resumption: 0,
            resume_labels: HashMap::new(),
        };

        for source_id in reachable {
            let Some(function) = program.function(source_id) else {
                continue;
            };
            let body = lower_stmt(&input, function.body, &mut state);
            let dense_id = dense_id_by_source
                .get(&source_id)
                .copied()
                .unwrap_or(LinearFuncId::INVALID);
            state.linear.functions.push(LinearFunction {
                id: dense_id,
                name: function.name,
                params: function.params.clone(),
                body,
            });
        }

        state.linear.entrypoints = program
            .entrypoints()
            .iter()
            .filter_map(|source| dense_id_by_source.get(source).copied())
            .collect();
    }
    linear
}

fn lower_stmt(
    input: &LoweringInput<'_>,
    stmt_id: StmtId,
    state: &mut LoweringState<'_>,
) -> LinearStmtId {
    if let Some(id) = state.stmt_map.get(stmt_id.index()).copied().flatten() {
        return id;
    }

    let Some(stmt) = input.program.stmt(stmt_id) else {
        let id = state.linear.push_stmt(LinearStmt::Error);
        state.stmt_map[stmt_id.index()] = Some(id);
        return id;
    };

    let kind = match &stmt.kind {
        StmtKind::Return(expr) => LinearStmt::Return(lower_expr(input, *expr, state)),
        StmtKind::Let {
            binding,
            value,
            next,
        } => LinearStmt::Let {
            binding: *binding,
            value: lower_expr(input, *value, state),
            next: lower_stmt(input, *next, state),
        },
        StmtKind::Val {
            binding,
            value,
            next,
        } => LinearStmt::Val {
            binding: *binding,
            value: lower_stmt(input, *value, state),
            next: lower_stmt(input, *next, state),
        },
        StmtKind::Call {
            result,
            callee,
            args,
            effects,
            next,
            ..
        } => {
            let callee_name = input.fn_names.get(callee).copied().unwrap_or_else(|| {
                state.diagnostics.error(
                    "LINEARIZE_UNKNOWN_CALLEE",
                    "Could not resolve function id while lowering call to linear IR",
                    stmt.span,
                );
                SymbolId::INVALID
            });
            let lowered_args = args
                .iter()
                .copied()
                .map(|arg| lower_expr(input, arg, state))
                .collect();
            let lowered_next = lower_stmt(input, *next, state);
            linear_call_stmt(
                *result,
                callee_name,
                input.callee_fn(callee),
                lowered_args,
                lowered_next,
                classify_call_convention(effects, input.sema),
            )
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => LinearStmt::If {
            cond: lower_expr(input, *cond, state),
            then_branch: lower_stmt(input, *then_branch, state),
            else_branch: lower_stmt(input, *else_branch, state),
        },
        StmtKind::Match {
            scrutinee,
            arms,
            default,
        } => LinearStmt::Match {
            scrutinee: lower_expr(input, *scrutinee, state),
            arms: arms
                .iter()
                .map(|arm| LinearMatchArm {
                    tag: arm.tag,
                    binders: arm.binders.clone(),
                    body: lower_stmt(input, arm.body, state),
                })
                .collect(),
            default: default.map(|default_stmt| lower_stmt(input, default_stmt, state)),
        },
        StmtKind::Perform {
            result,
            effect,
            operation,
            args,
            next,
        } => LinearStmt::Perform {
            result: *result,
            effect: *effect,
            operation: *operation,
            args: args
                .iter()
                .copied()
                .map(|arg| lower_expr(input, arg, state))
                .collect(),
            next: lower_stmt(input, *next, state),
        },
        StmtKind::Resume { next, .. } => {
            state.diagnostics.error(
                "LINEARIZE_RESUME_OUTSIDE_HANDLER",
                "Encountered `resume` outside a handler-lowering context",
                stmt.span,
            );
            return lower_stmt(input, *next, state);
        }
        StmtKind::Handle {
            handler,
            body,
            next,
        } => {
            if let Some(handler_def) = input.program.handlers().get(handler.index()) {
                let is_dead_handler =
                    !stmt_effect_row_contains(input.sema, *body, handler_def.effect);
                if is_dead_handler && is_identity_handler_return_clause(input.program, handler_def)
                {
                    state.diagnostics.note(
                        "LINEARIZE_DEAD_HANDLER_ELIMINATED",
                        "Elided handler with empty handled-effect intersection and identity return clause",
                        stmt.span,
                    );
                    state.record_handler(handler_def.effect, stmt.span, HandlerOutcome::Dead);
                    let lowered_body = lower_stmt(input, *body, state);
                    if let Some(next_stmt) = next {
                        let lowered_next = lower_stmt(input, *next_stmt, state);
                        let lowered = state.linear.push_stmt(LinearStmt::Val {
                            binding: handler_def.return_param,
                            value: lowered_body,
                            next: lowered_next,
                        });
                        state.stmt_map[stmt_id.index()] = Some(lowered);
                        return lowered;
                    }
                    state.stmt_map[stmt_id.index()] = Some(lowered_body);
                    return lowered_body;
                }

                let escaped =
                    core_stmt_calls_performing_effect(input.program, *body, handler_def.effect);
                let mut lowered_body =
                    lower_stmt_under_handlers(input, *body, &[handler_def], state, None);
                let leaked = linear_stmt_contains_perform_effect(
                    state.linear,
                    lowered_body,
                    handler_def.effect,
                );

                let mut outcome = HandlerOutcome::Inlined;
                if escaped || leaked {
                    outcome = if escaped {
                        HandlerOutcome::EscapedThroughCall
                    } else {
                        HandlerOutcome::LeakedAfterInlining
                    };
                    // Erasure is the optimization; the clause table is the
                    // fallback that keeps the program compiling when it misses.
                    match residual_clauses(input, handler_def, state) {
                        Ok(clauses) => {
                            outcome = HandlerOutcome::Residual;
                            lowered_body = state.linear.push_stmt_at(
                                LinearStmt::Handle {
                                    effect: handler_def.effect,
                                    clauses,
                                    body: lowered_body,
                                    next: None,
                                },
                                stmt.span,
                            );
                        }
                        Err(blocked) => {
                            state.diagnostics.error(
                                "LINEARIZE_HANDLED_EFFECT_LEAK",
                                format!(
                                    "Handled effect is performed where inlining cannot discharge it, and no runtime clause can stand in: {}",
                                    blocked.as_str()
                                ),
                                stmt.span,
                            );
                        }
                    }
                }
                state.record_handler(handler_def.effect, stmt.span, outcome);
                if let Some(next_stmt) = next {
                    let lowered_next = lower_stmt(input, *next_stmt, state);
                    let lowered = state.linear.push_stmt(LinearStmt::Val {
                        binding: handler_def.return_param,
                        value: lowered_body,
                        next: lowered_next,
                    });
                    state.stmt_map[stmt_id.index()] = Some(lowered);
                    return lowered;
                }
                state.stmt_map[stmt_id.index()] = Some(lowered_body);
                return lowered_body;
            } else {
                state.diagnostics.error(
                    "LINEARIZE_UNKNOWN_HANDLER",
                    "Could not resolve handler id while lowering to linear IR",
                    stmt.span,
                );
                let effect = cielo_base::ids::EffectLabelId::INVALID;
                state.record_handler(effect, stmt.span, HandlerOutcome::UnresolvedHandler);
                LinearStmt::Handle {
                    effect,
                    clauses: Vec::new(),
                    body: lower_stmt(input, *body, state),
                    next: next.map(|next_stmt| lower_stmt(input, next_stmt, state)),
                }
            }
        }
        StmtKind::Stage { stage, body, next } => LinearStmt::Stage {
            stage: *stage,
            body: lower_stmt(input, *body, state),
            next: next.map(|next_stmt| lower_stmt(input, next_stmt, state)),
        },
        StmtKind::Hole { .. } => LinearStmt::Hole,
        StmtKind::Error(_) => LinearStmt::Error,
    };

    let id = state.linear.push_stmt_at(kind, stmt.span);
    state.stmt_map[stmt_id.index()] = Some(id);
    id
}

/// Inlines a stack of enclosing handlers, innermost last.
///
/// A nested handler shadows the enclosing ones only for its own effect, so a
/// perform resolves against the innermost frame that handles it. Carrying a
/// single handler here silently dropped the outer ones, letting a perform of a
/// different effect escape unrewritten.
fn lower_stmt_under_handlers(
    input: &LoweringInput<'_>,
    stmt_id: StmtId,
    handlers: &[&HandlerDef],
    state: &mut LoweringState<'_>,
    resume_ctx: Option<&ResumeContext<'_>>,
) -> LinearStmtId {
    let Some(handler) = handlers.last().copied() else {
        return lower_stmt(input, stmt_id, state);
    };
    let Some(stmt) = input.program.stmt(stmt_id) else {
        return state.linear.push_stmt(LinearStmt::Error);
    };
    if state.inline_budget_exceeded(stmt.span) {
        return state.linear.push_stmt(LinearStmt::Error);
    }

    match &stmt.kind {
        StmtKind::Return(expr) => {
            let ret_value = lower_expr(input, *expr, state);
            let lowered_return = lower_stmt(input, handler.return_body, state);
            state.linear.push_stmt(LinearStmt::Let {
                binding: handler.return_param,
                value: ret_value,
                next: lowered_return,
            })
        }
        StmtKind::Perform {
            result,
            effect,
            operation,
            args,
            next,
            ..
        } if handlers.iter().any(|frame| frame.effect == *effect) => {
            let active = handlers
                .iter()
                .rev()
                .copied()
                .find(|frame| frame.effect == *effect)
                .expect("guard matched a frame");
            if let Some(clause) = active
                .clauses
                .iter()
                .find(|candidate| candidate.operation == *operation)
            {
                let resume_analysis = analyze_clause_resume(input.program, clause);
                if clause.resume_param.is_some() {
                    state.diagnostics.note(
                        "LINEARIZE_RESUME_QUALIFIER",
                        format!(
                            "handler clause resume qualifier: {} (min={}, max={}, tail={})",
                            resume_analysis.qualifier.as_str(),
                            resume_analysis.min_uses.as_str(),
                            resume_analysis.max_uses.as_str(),
                            resume_analysis.tail_resumptive
                        ),
                        clause.span,
                    );
                }
                if matches!(resume_analysis.qualifier, ResumeQualifier::Multi) {
                    state.diagnostics.error(
                        "LINEARIZE_MULTI_SHOT_RESUME",
                        "Handler clause resumes the continuation more than once; v1 supports single-shot resumptions only",
                        clause.span,
                    );
                }
                let clause_convention = classify_clause_convention(clause, resume_analysis);
                if clause.resume_param.is_some() {
                    state.diagnostics.note(
                        "LINEARIZE_RESUME_LOWERING_GATE",
                        format!(
                            "handler clause lowering gate: {} path selected (reason: {})",
                            clause_convention.as_str(),
                            resume_convention_reason(resume_analysis, clause_convention),
                        ),
                        clause.span,
                    );
                }
                // A lexical perform is discharged by splicing, so the policy is
                // always some form of erasure and can never be `Err` here.
                let policy = clause_policy(
                    input.program,
                    active,
                    clause,
                    resume_analysis,
                    PerformReach::Lexical,
                )
                .expect("a lexically visible perform is always erasable");
                if clause.resume_param.is_some() {
                    state.diagnostics.note(
                        "LINEARIZE_CLAUSE_POLICY",
                        format!("handler clause policy: {}", policy.as_str()),
                        clause.span,
                    );
                }
                let resumption = matches!(policy, ClausePolicy::Defunctionalise)
                    .then(|| state.fresh_resumption());
                let clause_resume_ctx = clause.resume_param.map(|resume_var| ResumeContext {
                    resume_var,
                    perform_result: *result,
                    continuation: *next,
                    clause_convention,
                    policy,
                    resumption,
                    outer: resume_ctx,
                });
                let lowered_clause = lower_matching_clause(
                    input,
                    clause,
                    args,
                    handlers,
                    clause_resume_ctx.as_ref(),
                    state,
                );
                match policy {
                    // `Dispatch` cannot be chosen here, and splicing is the
                    // right answer for it if it ever is: a lexical perform the
                    // clause is spliced into is discharged either way.
                    ClausePolicy::Erase(ResumeStrategy::Inline) | ClausePolicy::Dispatch => {
                        lowered_clause
                    }
                    ClausePolicy::Erase(ResumeStrategy::Join) => {
                        let continuation =
                            lower_stmt_under_handlers(input, *next, handlers, state, None);
                        let binding = result.unwrap_or_else(|| state.fresh_var());
                        state.linear.push_stmt(LinearStmt::Val {
                            binding,
                            value: lowered_clause,
                            next: continuation,
                        })
                    }
                    ClausePolicy::Defunctionalise => {
                        // Lowered under the context in force at *this* perform,
                        // exactly as each inlined copy would be: the clause's
                        // own frame is popped, everything enclosing it is live.
                        let continuation =
                            lower_stmt_under_handlers(input, *next, handlers, state, resume_ctx);
                        let param = result.unwrap_or_else(|| state.fresh_var());
                        state.linear.push_stmt(LinearStmt::Resumption {
                            id: resumption.expect("a defunctionalised clause reserves one"),
                            clause: lowered_clause,
                            param,
                            continuation,
                        })
                    }
                }
            } else {
                state.diagnostics.error(
                    "LINEARIZE_MISSING_HANDLER_CLAUSE",
                    "Missing handler clause for performed operation",
                    stmt.span,
                );
                lower_stmt_under_handlers(input, *next, handlers, state, resume_ctx)
            }
        }
        StmtKind::Resume {
            result,
            resume,
            arg,
            next,
        } => {
            let Some(active_ctx) = resume_ctx.filter(|ctx| *resume == ctx.resume_var) else {
                state.diagnostics.error(
                    "LINEARIZE_RESUME_OUTSIDE_CLAUSE",
                    "`resume` used outside the active handler clause context",
                    stmt.span,
                );
                return lower_stmt_under_handlers(input, *next, handlers, state, resume_ctx);
            };

            if matches!(active_ctx.policy, ClausePolicy::Erase(ResumeStrategy::Join)) {
                // Tail-resumptive by construction, so `next` is `return result`
                // and the enclosing join already carries the continuation.
                let arg_expr = lower_expr(input, *arg, state);
                return state.linear.push_stmt(LinearStmt::Return(arg_expr));
            }

            if let Some(resumption) = active_ctx.resumption {
                // Labels are handed out in lowering order, which is what makes
                // them dense; the dispatch reads them back as case indices.
                let label = state.next_resume_label(resumption);
                let arg_expr = lower_expr(input, *arg, state);
                let lowered_next = lower_stmt_under_handlers(input, *next, handlers, state, None);
                return state.linear.push_stmt(LinearStmt::ResumeJump {
                    resumption,
                    label,
                    arg: arg_expr,
                    result: *result,
                    next: lowered_next,
                });
            }

            // The continuation belongs to the perform site, so it is lowered
            // under that site's clause context -- this clause's own frame
            // popped, everything enclosing it still live.
            let continuation = lower_stmt_under_handlers(
                input,
                active_ctx.continuation,
                handlers,
                state,
                active_ctx.outer,
            );
            let continuation = if let Some(perform_var) = active_ctx.perform_result {
                let arg_expr = lower_expr(input, *arg, state);
                state.linear.push_stmt(LinearStmt::Let {
                    binding: perform_var,
                    value: arg_expr,
                    next: continuation,
                })
            } else {
                continuation
            };

            if is_identity_return_of_var(input.program, *next, *result)
                && matches!(active_ctx.clause_convention, ClauseConvention::Direct)
            {
                return continuation;
            }

            if matches!(active_ctx.clause_convention, ClauseConvention::Direct) {
                // Direct clauses must resume in tail position. Preserve a conservative fallback
                // if earlier rewrites invalidate the syntactic guarantee.
                state.diagnostics.error(
                    "LINEARIZE_DIRECT_RESUME_NON_TAIL",
                    "Direct handler clause resume is not in tail position; lowering as control path",
                    stmt.span,
                );
            }

            let lowered_next = lower_stmt_under_handlers(input, *next, handlers, state, None);
            state.linear.push_stmt(LinearStmt::Val {
                binding: *result,
                value: continuation,
                next: lowered_next,
            })
        }
        StmtKind::Let {
            binding,
            value,
            next,
        } => {
            let lowered_next = lower_stmt_under_handlers(input, *next, handlers, state, resume_ctx);
            let lowered_value = lower_expr(input, *value, state);
            state.linear.push_stmt(LinearStmt::Let {
                binding: *binding,
                value: lowered_value,
                next: lowered_next,
            })
        }
        StmtKind::Val {
            binding,
            value,
            next,
        } => {
            let lowered_value =
                lower_stmt_under_handlers(input, *value, handlers, state, resume_ctx);
            let lowered_next = lower_stmt_under_handlers(input, *next, handlers, state, resume_ctx);
            state.linear.push_stmt(LinearStmt::Val {
                binding: *binding,
                value: lowered_value,
                next: lowered_next,
            })
        }
        StmtKind::Call {
            result,
            callee,
            args,
            effects,
            next,
            ..
        } => {
            let callee_name = input.fn_names.get(callee).copied().unwrap_or_else(|| {
                state.diagnostics.error(
                    "LINEARIZE_UNKNOWN_CALLEE",
                    "Could not resolve function id while lowering call to linear IR",
                    stmt.span,
                );
                SymbolId::INVALID
            });
            let lowered_next = lower_stmt_under_handlers(input, *next, handlers, state, resume_ctx);
            let lowered_args = args
                .iter()
                .copied()
                .map(|arg| lower_expr(input, arg, state))
                .collect();
            state.linear.push_stmt(linear_call_stmt(
                *result,
                callee_name,
                input.callee_fn(callee),
                lowered_args,
                lowered_next,
                classify_call_convention(effects, input.sema),
            ))
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            let then_lowered =
                lower_stmt_under_handlers(input, *then_branch, handlers, state, resume_ctx);
            let else_lowered =
                lower_stmt_under_handlers(input, *else_branch, handlers, state, resume_ctx);
            let lowered_cond = lower_expr(input, *cond, state);
            state.linear.push_stmt(LinearStmt::If {
                cond: lowered_cond,
                then_branch: then_lowered,
                else_branch: else_lowered,
            })
        }
        StmtKind::Match {
            scrutinee,
            arms,
            default,
        } => {
            let lowered_arms = arms
                .iter()
                .map(|arm| LinearMatchArm {
                    tag: arm.tag,
                    binders: arm.binders.clone(),
                    body: lower_stmt_under_handlers(input, arm.body, handlers, state, resume_ctx),
                })
                .collect();
            let lowered_default = default.map(|default_stmt| {
                lower_stmt_under_handlers(input, default_stmt, handlers, state, resume_ctx)
            });
            let lowered_scrutinee = lower_expr(input, *scrutinee, state);
            state.linear.push_stmt(LinearStmt::Match {
                scrutinee: lowered_scrutinee,
                arms: lowered_arms,
                default: lowered_default,
            })
        }
        StmtKind::Perform {
            result,
            effect,
            operation,
            args,
            next,
        } => {
            let lowered_next = lower_stmt_under_handlers(input, *next, handlers, state, resume_ctx);
            let lowered_args = args
                .iter()
                .copied()
                .map(|arg| lower_expr(input, arg, state))
                .collect();
            state.linear.push_stmt(LinearStmt::Perform {
                result: *result,
                effect: *effect,
                operation: *operation,
                args: lowered_args,
                next: lowered_next,
            })
        }
        StmtKind::Handle {
            handler: inner_id,
            body,
            next,
        } => {
            let Some(inner) = input.program.handlers().get(inner_id.index()) else {
                return lower_stmt(input, stmt_id, state);
            };
            let mut nested = handlers.to_vec();
            nested.push(inner);
            let lowered_body = lower_stmt_under_handlers(input, *body, &nested, state, resume_ctx);
            state.record_handler(inner.effect, stmt.span, HandlerOutcome::Inlined);
            match next {
                Some(next_stmt) => {
                    let lowered_next =
                        lower_stmt_under_handlers(input, *next_stmt, handlers, state, resume_ctx);
                    state.linear.push_stmt(LinearStmt::Val {
                        binding: inner.return_param,
                        value: lowered_body,
                        next: lowered_next,
                    })
                }
                None => lowered_body,
            }
        }
        StmtKind::Stage { stage, body, next } => {
            let lowered_body = lower_stmt_under_handlers(input, *body, handlers, state, resume_ctx);
            let lowered_next = next.map(|next_stmt| {
                lower_stmt_under_handlers(input, next_stmt, handlers, state, resume_ctx)
            });
            state.linear.push_stmt(LinearStmt::Stage {
                stage: *stage,
                body: lowered_body,
                next: lowered_next,
            })
        }
        StmtKind::Hole { .. } => state.linear.push_stmt(LinearStmt::Hole),
        StmtKind::Error(_) => state.linear.push_stmt(LinearStmt::Error),
    }
}

/// The clause table for a handle site erasure could not fully discharge.
///
/// All or nothing: `cielo_perform` traps when the innermost frame for an
/// effect has no clause for the operation, so a partial table would trade a
/// compile error for a runtime one.
fn residual_clauses(
    input: &LoweringInput<'_>,
    handler: &HandlerDef,
    state: &mut LoweringState<'_>,
) -> Result<Vec<LinearHandlerClause>, ResidualBlocker> {
    let mut lowered = Vec::with_capacity(handler.clauses.len());
    for clause in &handler.clauses {
        // Recomputed from Core rather than carried over from the erasure pass:
        // the clause the dispatcher gets is lowered separately, and the two
        // lowerings are not interchangeable.
        let resume = analyze_clause_resume(input.program, clause);
        clause_policy(
            input.program,
            handler,
            clause,
            resume,
            PerformReach::Runtime,
        )?;
        let resume_var = clause
            .resume_param
            .expect("a clause with no resume parameter is blocked as abortive");
        let body = lower_tail_resume_body(input, clause.body, resume_var, state);
        lowered.push(LinearHandlerClause {
            operation: clause.operation,
            params: clause.params.clone(),
            body,
        });
    }
    Ok(lowered)
}

/// Lowers a tail-resumptive clause body into one that *returns* the resumption
/// argument. The dispatcher hands that value back to the perform site, so the
/// clause needs no continuation and its own value is never materialised.
///
/// Unmemoized on purpose: the same core clause body can also be inlined at a
/// lexical perform, and the two lowerings are not interchangeable.
fn lower_tail_resume_body(
    input: &LoweringInput<'_>,
    stmt_id: StmtId,
    resume_var: VarId,
    state: &mut LoweringState<'_>,
) -> LinearStmtId {
    let Some(stmt) = input.program.stmt(stmt_id) else {
        return state.linear.push_stmt(LinearStmt::Error);
    };
    let kind = match &stmt.kind {
        StmtKind::Resume { resume, arg, .. } if *resume == resume_var => {
            LinearStmt::Return(lower_expr(input, *arg, state))
        }
        StmtKind::Let {
            binding,
            value,
            next,
        } => LinearStmt::Let {
            binding: *binding,
            value: lower_expr(input, *value, state),
            next: lower_tail_resume_body(input, *next, resume_var, state),
        },
        StmtKind::Val {
            binding,
            value,
            next,
        } => {
            if is_identity_return_of_var(input.program, *next, *binding) {
                return lower_tail_resume_body(input, *value, resume_var, state);
            }
            LinearStmt::Val {
                binding: *binding,
                value: lower_stmt(input, *value, state),
                next: lower_tail_resume_body(input, *next, resume_var, state),
            }
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => LinearStmt::If {
            cond: lower_expr(input, *cond, state),
            then_branch: lower_tail_resume_body(input, *then_branch, resume_var, state),
            else_branch: lower_tail_resume_body(input, *else_branch, resume_var, state),
        },
        StmtKind::Match {
            scrutinee,
            arms,
            default,
        } => LinearStmt::Match {
            scrutinee: lower_expr(input, *scrutinee, state),
            arms: arms
                .iter()
                .map(|arm| LinearMatchArm {
                    tag: arm.tag,
                    binders: arm.binders.clone(),
                    body: lower_tail_resume_body(input, arm.body, resume_var, state),
                })
                .collect(),
            default: default
                .map(|default| lower_tail_resume_body(input, default, resume_var, state)),
        },
        // Tail-resumption analysis admits nothing else on a path to `resume`,
        // so anything here is below one and lowers as ordinary code.
        _ => return lower_stmt(input, stmt_id, state),
    };
    state.linear.push_stmt_at(kind, stmt.span)
}

fn lower_matching_clause(
    input: &LoweringInput<'_>,
    clause: &HandlerClause,
    args: &[ExprId],
    handlers: &[&HandlerDef],
    resume_ctx: Option<&ResumeContext<'_>>,
    state: &mut LoweringState<'_>,
) -> LinearStmtId {
    let clause_body = lower_stmt_under_handlers(input, clause.body, handlers, state, resume_ctx);
    let mut current = clause_body;

    for (param, arg) in clause
        .params
        .iter()
        .copied()
        .zip(args.iter().copied())
        .rev()
    {
        let arg_expr = lower_expr(input, arg, state);
        current = state.linear.push_stmt(LinearStmt::Let {
            binding: param,
            value: arg_expr,
            next: current,
        });
    }

    current
}

fn lower_expr(
    input: &LoweringInput<'_>,
    expr_id: ExprId,
    state: &mut LoweringState<'_>,
) -> LinearExprId {
    if let Some(id) = state.expr_map.get(expr_id.index()).copied().flatten() {
        return id;
    }

    let Some(expr) = input.program.expr(expr_id) else {
        let id = state.linear.push_expr(LinearExpr::Error);
        state.expr_map[expr_id.index()] = Some(id);
        return id;
    };

    let kind = match &expr.kind {
        ExprKind::Field { base, .. } => LinearExpr::Field {
            base: lower_expr(input, *base, state),
            index: input
                .sema
                .field_index_of_expr
                .get(&expr_id)
                .copied()
                .unwrap_or_default(),
        },
        ExprKind::Var(var) => LinearExpr::Var(*var),
        ExprKind::Literal(lit) => LinearExpr::Literal(lit.clone()),
        ExprKind::Unary { op, expr } => LinearExpr::Unary {
            op: *op,
            expr: lower_expr(input, *expr, state),
        },
        ExprKind::Binary { op, lhs, rhs } => LinearExpr::Binary {
            op: *op,
            lhs: lower_expr(input, *lhs, state),
            rhs: lower_expr(input, *rhs, state),
        },
        ExprKind::PureCall { callee, args } => LinearExpr::PureCall {
            callee: input
                .fn_names
                .get(callee)
                .copied()
                .unwrap_or(SymbolId::INVALID),
            callee_fn: input.callee_fn(callee),
            args: args
                .iter()
                .copied()
                .map(|arg| lower_expr(input, arg, state))
                .collect(),
        },
        ExprKind::BuiltinCall { builtin, args } => LinearExpr::BuiltinCall {
            builtin: *builtin,
            args: args
                .iter()
                .copied()
                .map(|arg| lower_expr(input, arg, state))
                .collect(),
        },
        ExprKind::MakeStruct { ty, fields } => LinearExpr::MakeStruct {
            ty: *ty,
            fields: fields
                .iter()
                .copied()
                .map(|field| lower_expr(input, field, state))
                .collect(),
        },
        ExprKind::MakeEnum {
            ty,
            variant,
            fields,
        } => LinearExpr::MakeEnum {
            ty: *ty,
            variant: *variant,
            fields: fields
                .iter()
                .copied()
                .map(|field| lower_expr(input, field, state))
                .collect(),
        },
        ExprKind::MakeClosure { func, captures } => LinearExpr::MakeClosure {
            callee: input.fn_names.get(func).copied().unwrap_or_else(|| {
                state.diagnostics.error(
                    "LINEARIZE_UNKNOWN_CLOSURE_BODY",
                    "Could not resolve the function id of a closure body",
                    expr.span,
                );
                SymbolId::INVALID
            }),
            callee_fn: input.callee_fn(func),
            captures: captures
                .iter()
                .copied()
                .map(|capture| lower_expr(input, capture, state))
                .collect(),
        },
        ExprKind::CallClosure { callee, args } => LinearExpr::CallClosure {
            callee: lower_expr(input, *callee, state),
            args: args
                .iter()
                .copied()
                .map(|arg| lower_expr(input, arg, state))
                .collect(),
        },
        ExprKind::Error(_) => LinearExpr::Error,
    };

    let id = state.linear.push_expr(kind);
    state.expr_map[expr_id.index()] = Some(id);
    id
}

fn classify_call_convention(effects: &SortedEffectRow, sema: &SemanticTables) -> CallConvention {
    if effects.is_empty() {
        return CallConvention::Pure;
    }
    if is_thunkable(effects, &sema.effect_properties) {
        CallConvention::Direct
    } else {
        CallConvention::Control
    }
}

fn linear_call_stmt(
    result: VarId,
    callee: SymbolId,
    callee_fn: LinearFuncId,
    args: Vec<LinearExprId>,
    next: LinearStmtId,
    convention: CallConvention,
) -> LinearStmt {
    match convention {
        CallConvention::Pure => LinearStmt::PureCall {
            result,
            callee,
            callee_fn,
            args,
            next,
        },
        CallConvention::Direct => LinearStmt::DirectCall {
            result,
            callee,
            callee_fn,
            args,
            next,
        },
        CallConvention::Control => LinearStmt::ControlCall {
            result,
            callee,
            callee_fn,
            args,
            next,
        },
    }
}
