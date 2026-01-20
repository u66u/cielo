// Pass 8/9: linearize (Residual Core -> linear runtime IR)
//
// Inputs:
// - Residualized Core program
//
// Outputs:
// - Arena-backed LinearProgram with explicit expression/statement ids
//
// Invariants:
// - Variable identities are preserved
// - Function ids are remapped densely after reachability pruning
// - Core node sharing is preserved via memoized ID mapping
//
// Diagnostics:
// - `LINEARIZE_UNKNOWN_CALLEE` when a call target cannot be resolved
// - `LINEARIZE_UNKNOWN_HANDLER` when a handler id cannot be resolved
//
// Complexity:
// - O(expr_count + stmt_count)

use crate::analysis::function_graph::collect_reachable_functions;
use std::collections::{HashMap, HashSet};

use crate::common::diagnostics::DiagnosticBag;
use crate::common::ids::{
    ExprId, FuncId, LinearExprId, LinearFuncId, LinearStmtId, StmtId, SymbolId, VarId,
};
use crate::ir::core::{CoreProgram, ExprKind, HandlerClause, HandlerDef, StmtKind};
use crate::ir::linear::{
    CallConvention, LinearExpr, LinearFunction, LinearMatchArm, LinearProgram, LinearStmt,
};
use crate::pipeline::phases::{Residualized, SemanticTables};
use crate::sema::effect::is_thunkable;

pub fn run(mut residual: Residualized) -> Linearized {
    let sema = residual.sema().clone();
    let linear = {
        let (program, diagnostics) = residual.program_and_diagnostics_mut();
        lower_program(program, &sema, diagnostics)
    };
    Linearized { residual, linear }
}

#[derive(Clone, Debug)]
pub struct Linearized {
    pub residual: Residualized,
    pub linear: LinearProgram,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ResumeContext {
    resume_var: VarId,
    perform_result: Option<VarId>,
    continuation: StmtId,
    clause_convention: ClauseConvention,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ClauseConvention {
    Pure,
    Direct,
    Control,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ResumeUseBound {
    Zero,
    One,
    Many,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ResumeQualifier {
    Abortive,
    Affine,
    Linear,
    Multi,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ClauseResumeAnalysis {
    qualifier: ResumeQualifier,
    min_uses: ResumeUseBound,
    max_uses: ResumeUseBound,
    tail_resumptive: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct ResumeUseRange {
    min: ResumeUseBound,
    max: ResumeUseBound,
}

impl ResumeUseBound {
    fn plus(self, other: Self) -> Self {
        use ResumeUseBound::{Many, One, Zero};
        match (self, other) {
            (Many, _) | (_, Many) => Many,
            (One, One) => Many,
            (One, Zero) | (Zero, One) => One,
            (Zero, Zero) => Zero,
        }
    }

    fn max(self, other: Self) -> Self {
        use ResumeUseBound::{Many, One, Zero};
        match (self, other) {
            (Many, _) | (_, Many) => Many,
            (One, _) | (_, One) => One,
            (Zero, Zero) => Zero,
        }
    }

    fn min(self, other: Self) -> Self {
        use ResumeUseBound::{Many, One, Zero};
        match (self, other) {
            (Zero, _) | (_, Zero) => Zero,
            (One, _) | (_, One) => One,
            (Many, Many) => Many,
        }
    }

    fn is_many(self) -> bool {
        matches!(self, Self::Many)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Zero => "zero",
            Self::One => "one",
            Self::Many => "many",
        }
    }
}

impl ResumeQualifier {
    fn as_str(self) -> &'static str {
        match self {
            Self::Abortive => "Abortive",
            Self::Affine => "Affine",
            Self::Linear => "Linear",
            Self::Multi => "Multi",
        }
    }
}

impl ResumeUseRange {
    fn zero() -> Self {
        Self {
            min: ResumeUseBound::Zero,
            max: ResumeUseBound::Zero,
        }
    }

    fn many() -> Self {
        Self {
            min: ResumeUseBound::Zero,
            max: ResumeUseBound::Many,
        }
    }

    fn plus(self, other: Self) -> Self {
        Self {
            min: self.min.plus(other.min),
            max: self.max.plus(other.max),
        }
    }

    fn join(self, other: Self) -> Self {
        Self {
            min: self.min.min(other.min),
            max: self.max.max(other.max),
        }
    }
}

fn lower_program(
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

    for source_id in reachable {
        let Some(function) = program.function(source_id) else {
            continue;
        };
        let body = lower_stmt(
            program,
            function.body,
            &fn_names,
            sema,
            diagnostics,
            &mut linear,
            &mut expr_map,
            &mut stmt_map,
        );
        let dense_id = dense_id_by_source
            .get(&source_id)
            .copied()
            .unwrap_or(LinearFuncId::INVALID);
        linear.functions.push(LinearFunction {
            id: dense_id,
            name: function.name,
            params: function.params.clone(),
            body,
        });
    }

    linear.entrypoints = program
        .entrypoints()
        .iter()
        .filter_map(|source| dense_id_by_source.get(source).copied())
        .collect();
    linear
}

fn lower_stmt(
    program: &CoreProgram,
    stmt_id: StmtId,
    fn_names: &HashMap<FuncId, SymbolId>,
    sema: &SemanticTables,
    diagnostics: &mut DiagnosticBag,
    linear: &mut LinearProgram,
    expr_map: &mut [Option<LinearExprId>],
    stmt_map: &mut [Option<LinearStmtId>],
) -> LinearStmtId {
    if let Some(id) = stmt_map.get(stmt_id.index()).copied().flatten() {
        return id;
    }

    let Some(stmt) = program.stmt(stmt_id) else {
        let id = linear.push_stmt(LinearStmt::Error);
        stmt_map[stmt_id.index()] = Some(id);
        return id;
    };

    let kind = match &stmt.kind {
        StmtKind::Return(expr) => {
            LinearStmt::Return(lower_expr(program, *expr, fn_names, linear, expr_map))
        }
        StmtKind::Let {
            binding,
            value,
            next,
        } => LinearStmt::Let {
            binding: *binding,
            value: lower_expr(program, *value, fn_names, linear, expr_map),
            next: lower_stmt(
                program,
                *next,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
            ),
        },
        StmtKind::Val {
            binding,
            value,
            next,
        } => LinearStmt::Val {
            binding: *binding,
            value: lower_stmt(
                program,
                *value,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
            ),
            next: lower_stmt(
                program,
                *next,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
            ),
        },
        StmtKind::Call {
            result,
            callee,
            args,
            effects,
            next,
            ..
        } => {
            let callee_name = fn_names.get(callee).copied().unwrap_or_else(|| {
                diagnostics.error(
                    "LINEARIZE_UNKNOWN_CALLEE",
                    "Could not resolve function id while lowering call to linear IR",
                    stmt.span,
                );
                SymbolId::INVALID
            });
            let lowered_args = args
                .iter()
                .copied()
                .map(|arg| lower_expr(program, arg, fn_names, linear, expr_map))
                .collect();
            let lowered_next = lower_stmt(
                program,
                *next,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
            );
            linear_call_stmt(
                *result,
                callee_name,
                lowered_args,
                lowered_next,
                classify_call_convention(effects, sema),
            )
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => LinearStmt::If {
            cond: lower_expr(program, *cond, fn_names, linear, expr_map),
            then_branch: lower_stmt(
                program,
                *then_branch,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
            ),
            else_branch: lower_stmt(
                program,
                *else_branch,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
            ),
        },
        StmtKind::Match {
            scrutinee,
            arms,
            default,
        } => LinearStmt::Match {
            scrutinee: lower_expr(program, *scrutinee, fn_names, linear, expr_map),
            arms: arms
                .iter()
                .map(|arm| LinearMatchArm {
                    tag: arm.tag,
                    binders: arm.binders.clone(),
                    body: lower_stmt(
                        program,
                        arm.body,
                        fn_names,
                        sema,
                        diagnostics,
                        linear,
                        expr_map,
                        stmt_map,
                    ),
                })
                .collect(),
            default: default.map(|default_stmt| {
                lower_stmt(
                    program,
                    default_stmt,
                    fn_names,
                    sema,
                    diagnostics,
                    linear,
                    expr_map,
                    stmt_map,
                )
            }),
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
                .map(|arg| lower_expr(program, arg, fn_names, linear, expr_map))
                .collect(),
            next: lower_stmt(
                program,
                *next,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
            ),
        },
        StmtKind::Resume { next, .. } => {
            diagnostics.error(
                "LINEARIZE_RESUME_OUTSIDE_HANDLER",
                "Encountered `resume` outside a handler-lowering context",
                stmt.span,
            );
            return lower_stmt(
                program,
                *next,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
            );
        }
        StmtKind::Handle {
            handler,
            body,
            next,
        } => {
            if let Some(handler_def) = program.handlers().get(handler.index()) {
                if next.is_none() {
                    let lowered = lower_stmt_under_handler(
                        program,
                        *body,
                        handler_def,
                        fn_names,
                        sema,
                        diagnostics,
                        linear,
                        expr_map,
                        stmt_map,
                        None,
                    );
                    stmt_map[stmt_id.index()] = Some(lowered);
                    return lowered;
                }

                let effect = handler_def.effect;
                LinearStmt::Handle {
                    effect,
                    body: lower_stmt(
                        program,
                        *body,
                        fn_names,
                        sema,
                        diagnostics,
                        linear,
                        expr_map,
                        stmt_map,
                    ),
                    next: next.map(|next_stmt| {
                        lower_stmt(
                            program,
                            next_stmt,
                            fn_names,
                            sema,
                            diagnostics,
                            linear,
                            expr_map,
                            stmt_map,
                        )
                    }),
                }
            } else {
                diagnostics.error(
                    "LINEARIZE_UNKNOWN_HANDLER",
                    "Could not resolve handler id while lowering to linear IR",
                    stmt.span,
                );
                let effect = crate::common::ids::EffectLabelId::INVALID;
                LinearStmt::Handle {
                    effect,
                    body: lower_stmt(
                        program,
                        *body,
                        fn_names,
                        sema,
                        diagnostics,
                        linear,
                        expr_map,
                        stmt_map,
                    ),
                    next: next.map(|next_stmt| {
                        lower_stmt(
                            program,
                            next_stmt,
                            fn_names,
                            sema,
                            diagnostics,
                            linear,
                            expr_map,
                            stmt_map,
                        )
                    }),
                }
            }
        }
        StmtKind::Stage { stage, body, next } => LinearStmt::Stage {
            stage: *stage,
            body: lower_stmt(
                program,
                *body,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
            ),
            next: next.map(|next_stmt| {
                lower_stmt(
                    program,
                    next_stmt,
                    fn_names,
                    sema,
                    diagnostics,
                    linear,
                    expr_map,
                    stmt_map,
                )
            }),
        },
        StmtKind::Hole { .. } => LinearStmt::Hole,
        StmtKind::Error(_) => LinearStmt::Error,
    };

    let id = linear.push_stmt(kind);
    stmt_map[stmt_id.index()] = Some(id);
    id
}

#[allow(clippy::too_many_arguments)]
fn lower_stmt_under_handler(
    program: &CoreProgram,
    stmt_id: StmtId,
    handler: &HandlerDef,
    fn_names: &HashMap<FuncId, SymbolId>,
    sema: &SemanticTables,
    diagnostics: &mut DiagnosticBag,
    linear: &mut LinearProgram,
    expr_map: &mut [Option<LinearExprId>],
    stmt_map: &mut [Option<LinearStmtId>],
    resume_ctx: Option<ResumeContext>,
) -> LinearStmtId {
    let Some(stmt) = program.stmt(stmt_id) else {
        return linear.push_stmt(LinearStmt::Error);
    };

    match &stmt.kind {
        StmtKind::Return(expr) => {
            let ret_value = lower_expr(program, *expr, fn_names, linear, expr_map);
            let lowered_return = lower_stmt(
                program,
                handler.return_body,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
            );
            linear.push_stmt(LinearStmt::Let {
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
        } if *effect == handler.effect => {
            if let Some(clause) = handler
                .clauses
                .iter()
                .find(|candidate| candidate.operation == *operation)
            {
                let resume_analysis = analyze_clause_resume(program, clause);
                if clause.resume_param.is_some() {
                    diagnostics.note(
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
                    diagnostics.error(
                        "LINEARIZE_MULTI_SHOT_RESUME",
                        "Handler clause resumes the continuation more than once; v1 supports single-shot resumptions only",
                        clause.span,
                    );
                }
                let clause_convention = classify_clause_convention(clause, resume_analysis);
                let clause_resume_ctx = clause.resume_param.map(|resume_var| ResumeContext {
                    resume_var,
                    perform_result: *result,
                    continuation: *next,
                    clause_convention,
                });
                lower_matching_clause(
                    program,
                    clause,
                    args,
                    handler,
                    clause_resume_ctx,
                    fn_names,
                    sema,
                    diagnostics,
                    linear,
                    expr_map,
                    stmt_map,
                )
            } else {
                diagnostics.error(
                    "LINEARIZE_MISSING_HANDLER_CLAUSE",
                    "Missing handler clause for performed operation",
                    stmt.span,
                );
                lower_stmt_under_handler(
                    program,
                    *next,
                    handler,
                    fn_names,
                    sema,
                    diagnostics,
                    linear,
                    expr_map,
                    stmt_map,
                    resume_ctx,
                )
            }
        }
        StmtKind::Resume {
            result,
            resume,
            arg,
            next,
        } => {
            let Some(active_ctx) = resume_ctx.filter(|ctx| *resume == ctx.resume_var) else {
                diagnostics.error(
                    "LINEARIZE_RESUME_OUTSIDE_CLAUSE",
                    "`resume` used outside the active handler clause context",
                    stmt.span,
                );
                return lower_stmt_under_handler(
                    program,
                    *next,
                    handler,
                    fn_names,
                    sema,
                    diagnostics,
                    linear,
                    expr_map,
                    stmt_map,
                    resume_ctx,
                );
            };

            let continuation = lower_stmt_under_handler(
                program,
                active_ctx.continuation,
                handler,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
                None,
            );
            let continuation = if let Some(perform_var) = active_ctx.perform_result {
                let arg_expr = lower_expr(program, *arg, fn_names, linear, expr_map);
                linear.push_stmt(LinearStmt::Let {
                    binding: perform_var,
                    value: arg_expr,
                    next: continuation,
                })
            } else {
                continuation
            };

            if is_identity_return_of_var(program, *next, *result)
                && matches!(active_ctx.clause_convention, ClauseConvention::Direct)
            {
                return continuation;
            }

            if matches!(active_ctx.clause_convention, ClauseConvention::Direct) {
                // Direct clauses must resume in tail position. Preserve a conservative fallback
                // if earlier rewrites invalidate the syntactic guarantee.
                diagnostics.error(
                    "LINEARIZE_DIRECT_RESUME_NON_TAIL",
                    "Direct handler clause resume is not in tail position; lowering as control path",
                    stmt.span,
                );
            }

            let lowered_next = lower_stmt_under_handler(
                program,
                *next,
                handler,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
                None,
            );
            linear.push_stmt(LinearStmt::Val {
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
            let lowered_next = lower_stmt_under_handler(
                program,
                *next,
                handler,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
                resume_ctx,
            );
            let lowered_value = lower_expr(program, *value, fn_names, linear, expr_map);
            linear.push_stmt(LinearStmt::Let {
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
            let lowered_value = lower_stmt_under_handler(
                program,
                *value,
                handler,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
                resume_ctx,
            );
            let lowered_next = lower_stmt_under_handler(
                program,
                *next,
                handler,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
                resume_ctx,
            );
            linear.push_stmt(LinearStmt::Val {
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
            let callee_name = fn_names.get(callee).copied().unwrap_or_else(|| {
                diagnostics.error(
                    "LINEARIZE_UNKNOWN_CALLEE",
                    "Could not resolve function id while lowering call to linear IR",
                    stmt.span,
                );
                SymbolId::INVALID
            });
            let lowered_next = lower_stmt_under_handler(
                program,
                *next,
                handler,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
                resume_ctx,
            );
            let lowered_args = args
                .iter()
                .copied()
                .map(|arg| lower_expr(program, arg, fn_names, linear, expr_map))
                .collect();
            linear.push_stmt(linear_call_stmt(
                *result,
                callee_name,
                lowered_args,
                lowered_next,
                classify_call_convention(effects, sema),
            ))
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            let then_lowered = lower_stmt_under_handler(
                program,
                *then_branch,
                handler,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
                resume_ctx,
            );
            let else_lowered = lower_stmt_under_handler(
                program,
                *else_branch,
                handler,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
                resume_ctx,
            );
            let lowered_cond = lower_expr(program, *cond, fn_names, linear, expr_map);
            linear.push_stmt(LinearStmt::If {
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
                    body: lower_stmt_under_handler(
                        program,
                        arm.body,
                        handler,
                        fn_names,
                        sema,
                        diagnostics,
                        linear,
                        expr_map,
                        stmt_map,
                        resume_ctx,
                    ),
                })
                .collect();
            let lowered_default = default.map(|default_stmt| {
                lower_stmt_under_handler(
                    program,
                    default_stmt,
                    handler,
                    fn_names,
                    sema,
                    diagnostics,
                    linear,
                    expr_map,
                    stmt_map,
                    resume_ctx,
                )
            });
            let lowered_scrutinee = lower_expr(program, *scrutinee, fn_names, linear, expr_map);
            linear.push_stmt(LinearStmt::Match {
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
            let lowered_next = lower_stmt_under_handler(
                program,
                *next,
                handler,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
                resume_ctx,
            );
            let lowered_args = args
                .iter()
                .copied()
                .map(|arg| lower_expr(program, arg, fn_names, linear, expr_map))
                .collect();
            linear.push_stmt(LinearStmt::Perform {
                result: *result,
                effect: *effect,
                operation: *operation,
                args: lowered_args,
                next: lowered_next,
            })
        }
        StmtKind::Handle { .. } => lower_stmt(
            program,
            stmt_id,
            fn_names,
            sema,
            diagnostics,
            linear,
            expr_map,
            stmt_map,
        ),
        StmtKind::Stage { stage, body, next } => {
            let lowered_body = lower_stmt_under_handler(
                program,
                *body,
                handler,
                fn_names,
                sema,
                diagnostics,
                linear,
                expr_map,
                stmt_map,
                resume_ctx,
            );
            let lowered_next = next.map(|next_stmt| {
                lower_stmt_under_handler(
                    program,
                    next_stmt,
                    handler,
                    fn_names,
                    sema,
                    diagnostics,
                    linear,
                    expr_map,
                    stmt_map,
                    resume_ctx,
                )
            });
            linear.push_stmt(LinearStmt::Stage {
                stage: *stage,
                body: lowered_body,
                next: lowered_next,
            })
        }
        StmtKind::Hole { .. } => linear.push_stmt(LinearStmt::Hole),
        StmtKind::Error(_) => linear.push_stmt(LinearStmt::Error),
    }
}

fn is_identity_return_of_var(program: &CoreProgram, stmt_id: StmtId, var: VarId) -> bool {
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

fn analyze_clause_resume(program: &CoreProgram, clause: &HandlerClause) -> ClauseResumeAnalysis {
    let Some(resume_var) = clause.resume_param else {
        return ClauseResumeAnalysis {
            qualifier: ResumeQualifier::Abortive,
            min_uses: ResumeUseBound::Zero,
            max_uses: ResumeUseBound::Zero,
            tail_resumptive: true,
        };
    };
    let range = clause_resume_use_range(program, clause.body, resume_var);
    let tail_resumptive = is_tail_resumptive_clause(program, clause.body, resume_var);
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
    }
}

fn classify_clause_convention(
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
                && default.map_or(true, |stmt| {
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

fn clause_resume_use_range(program: &CoreProgram, root: StmtId, resume_var: VarId) -> ResumeUseRange {
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
            clause_resume_use_range_stmt(program, *value, resume_var, memo, visiting)
                .plus(clause_resume_use_range_stmt(program, *next, resume_var, memo, visiting))
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
            let body_bound = clause_resume_use_range_stmt(program, *body, resume_var, memo, visiting);
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
    let mut stack = vec![root];
    let mut seen_exprs = HashSet::new();
    while let Some(expr_id) = stack.pop() {
        if !seen_exprs.insert(expr_id) {
            continue;
        }
        let Some(expr) = program.expr(expr_id) else {
            continue;
        };
        match &expr.kind {
            ExprKind::Var(found) => {
                if *found == var {
                    return true;
                }
            }
            ExprKind::Unary { expr, .. } => stack.push(*expr),
            ExprKind::Binary { lhs, rhs, .. } => {
                stack.push(*rhs);
                stack.push(*lhs);
            }
            ExprKind::PureCall { args, .. }
            | ExprKind::MakeStruct { fields: args, .. }
            | ExprKind::MakeEnum { fields: args, .. } => {
                for arg in args {
                    stack.push(*arg);
                }
            }
            ExprKind::Literal(_) | ExprKind::Error(_) => {}
        }
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn lower_matching_clause(
    program: &CoreProgram,
    clause: &HandlerClause,
    args: &[ExprId],
    handler: &HandlerDef,
    resume_ctx: Option<ResumeContext>,
    fn_names: &HashMap<FuncId, SymbolId>,
    sema: &SemanticTables,
    diagnostics: &mut DiagnosticBag,
    linear: &mut LinearProgram,
    expr_map: &mut [Option<LinearExprId>],
    stmt_map: &mut [Option<LinearStmtId>],
) -> LinearStmtId {
    let clause_body = lower_stmt_under_handler(
        program,
        clause.body,
        handler,
        fn_names,
        sema,
        diagnostics,
        linear,
        expr_map,
        stmt_map,
        resume_ctx,
    );
    let mut current = clause_body;

    for (param, arg) in clause
        .params
        .iter()
        .copied()
        .zip(args.iter().copied())
        .rev()
    {
        let arg_expr = lower_expr(program, arg, fn_names, linear, expr_map);
        current = linear.push_stmt(LinearStmt::Let {
            binding: param,
            value: arg_expr,
            next: current,
        });
    }

    current
}

fn lower_expr(
    program: &CoreProgram,
    expr_id: ExprId,
    fn_names: &HashMap<FuncId, SymbolId>,
    linear: &mut LinearProgram,
    expr_map: &mut [Option<LinearExprId>],
) -> LinearExprId {
    if let Some(id) = expr_map.get(expr_id.index()).copied().flatten() {
        return id;
    }

    let Some(expr) = program.expr(expr_id) else {
        let id = linear.push_expr(LinearExpr::Error);
        expr_map[expr_id.index()] = Some(id);
        return id;
    };

    let kind = match &expr.kind {
        ExprKind::Var(var) => LinearExpr::Var(*var),
        ExprKind::Literal(lit) => LinearExpr::Literal(lit.clone()),
        ExprKind::Unary { op, expr } => LinearExpr::Unary {
            op: *op,
            expr: lower_expr(program, *expr, fn_names, linear, expr_map),
        },
        ExprKind::Binary { op, lhs, rhs } => LinearExpr::Binary {
            op: *op,
            lhs: lower_expr(program, *lhs, fn_names, linear, expr_map),
            rhs: lower_expr(program, *rhs, fn_names, linear, expr_map),
        },
        ExprKind::PureCall { callee, args } => LinearExpr::PureCall {
            callee: fn_names.get(callee).copied().unwrap_or(SymbolId::INVALID),
            args: args
                .iter()
                .copied()
                .map(|arg| lower_expr(program, arg, fn_names, linear, expr_map))
                .collect(),
        },
        ExprKind::MakeStruct { ty, fields } => LinearExpr::MakeStruct {
            ty: *ty,
            fields: fields
                .iter()
                .copied()
                .map(|field| lower_expr(program, field, fn_names, linear, expr_map))
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
                .map(|field| lower_expr(program, field, fn_names, linear, expr_map))
                .collect(),
        },
        ExprKind::Error(_) => LinearExpr::Error,
    };

    let id = linear.push_expr(kind);
    expr_map[expr_id.index()] = Some(id);
    id
}

fn classify_call_convention(
    effects: &crate::sema::effect::SortedEffectRow,
    sema: &SemanticTables,
) -> CallConvention {
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
    args: Vec<LinearExprId>,
    next: LinearStmtId,
    convention: CallConvention,
) -> LinearStmt {
    match convention {
        CallConvention::Pure => LinearStmt::PureCall {
            result,
            callee,
            args,
            next,
        },
        CallConvention::Direct => LinearStmt::DirectCall {
            result,
            callee,
            args,
            next,
        },
        CallConvention::Control => LinearStmt::ControlCall {
            result,
            callee,
            args,
            next,
        },
    }
}
