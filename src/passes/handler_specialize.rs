// Pass 7/9: handler_specialize (bounded handler-call specialization groundwork)
//
// Inputs:
// - Residualized Core program
//
// Outputs:
// - Residualized Core program with specialized function copies for handle-wrapped calls
//
// Invariants:
// - A pair (callee, handler) specializes at most once
// - Specialized functions preserve signatures and clone the original body graph
// - Recursive calls in specialized copies are retargeted to the specialized function id
//
// Complexity:
// - O(stmt_count + cloned_nodes)

use std::collections::HashMap;

use crate::common::ids::{ExprId, FuncId, HandlerId, StmtId};
use crate::ir::core::{CoreProgram, ExprKind, ExprNode, FunctionDecl, StmtKind, StmtNode};
use crate::pipeline::phases::Residualized;

pub fn run(residual: Residualized) -> Residualized {
    let (mut program, diagnostics, sema, mono, ct, bta, residual_tables) = residual.into_parts();
    specialize_handle_wrapped_calls(&mut program);
    Residualized::new(program, diagnostics, sema, mono, ct, bta, residual_tables)
}

#[derive(Clone, Copy)]
struct SpecializeCandidate {
    handler: HandlerId,
    call_stmt: StmtId,
    callee: FuncId,
}

fn specialize_handle_wrapped_calls(program: &mut CoreProgram) {
    let candidates = collect_specialize_candidates(program);
    let mut specialized: HashMap<(FuncId, HandlerId), FuncId> = HashMap::new();

    for candidate in candidates {
        let specialized_callee = ensure_specialized(program, candidate, &mut specialized);
        if specialized_callee == candidate.callee {
            continue;
        }
        if let Some(stmt) = program.stmt_mut(candidate.call_stmt) {
            if let StmtKind::Call { callee, .. } = &mut stmt.kind {
                *callee = specialized_callee;
            }
        }
    }
}

fn collect_specialize_candidates(program: &CoreProgram) -> Vec<SpecializeCandidate> {
    let mut out = Vec::new();
    for stmt_idx in 0..program.stmts().len() {
        let stmt_id = StmtId::new(stmt_idx);
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        let StmtKind::Handle { handler, body, .. } = &stmt.kind else {
            continue;
        };
        let Some(call_stmt) = first_call_stmt(program, *body) else {
            continue;
        };
        let Some(body_stmt) = program.stmt(call_stmt) else {
            continue;
        };
        let StmtKind::Call { callee, .. } = body_stmt.kind else {
            continue;
        };
        out.push(SpecializeCandidate {
            handler: *handler,
            call_stmt,
            callee,
        });
    }
    out
}

fn first_call_stmt(program: &CoreProgram, root: StmtId) -> Option<StmtId> {
    let mut stack = vec![root];
    let mut seen = std::collections::HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let stmt = program.stmt(stmt_id)?;
        if matches!(stmt.kind, StmtKind::Call { .. }) {
            return Some(stmt_id);
        }
        for child in stmt.child_stmts() {
            stack.push(child);
        }
    }
    None
}

fn ensure_specialized(
    program: &mut CoreProgram,
    candidate: SpecializeCandidate,
    specialized: &mut HashMap<(FuncId, HandlerId), FuncId>,
) -> FuncId {
    if let Some(existing) = specialized.get(&(candidate.callee, candidate.handler)).copied() {
        return existing;
    }

    let Some(source_decl) = program.function(candidate.callee).cloned() else {
        return candidate.callee;
    };

    // Add a placeholder copy first to obtain the stable specialized FuncId.
    let placeholder = FunctionDecl {
        body: source_decl.body,
        ..source_decl.clone()
    };
    let specialized_id = program.add_function(placeholder);

    let mut expr_map = HashMap::new();
    let mut stmt_map = HashMap::new();
    let cloned_body = clone_stmt_graph(
        program,
        source_decl.body,
        candidate.callee,
        specialized_id,
        &mut expr_map,
        &mut stmt_map,
    );
    let wrapped_body = program.push_stmt(StmtNode {
        span: source_decl.span,
        kind: StmtKind::Handle {
            handler: candidate.handler,
            body: cloned_body,
            next: None,
        },
    });

    if let Some(function) = program.function_mut(specialized_id) {
        function.body = wrapped_body;
    }

    specialized.insert((candidate.callee, candidate.handler), specialized_id);
    specialized_id
}

fn clone_stmt_graph(
    program: &mut CoreProgram,
    source: StmtId,
    source_func: FuncId,
    specialized_func: FuncId,
    expr_map: &mut HashMap<ExprId, ExprId>,
    stmt_map: &mut HashMap<StmtId, StmtId>,
) -> StmtId {
    if let Some(existing) = stmt_map.get(&source).copied() {
        return existing;
    }

    let Some(node) = program.stmt(source).cloned() else {
        return source;
    };

    let kind = match node.kind {
        StmtKind::Return(expr) => StmtKind::Return(clone_expr_graph(
            program,
            expr,
            source_func,
            specialized_func,
            expr_map,
        )),
        StmtKind::Let {
            binding,
            value,
            next,
        } => StmtKind::Let {
            binding,
            value: clone_expr_graph(program, value, source_func, specialized_func, expr_map),
            next: clone_stmt_graph(
                program,
                next,
                source_func,
                specialized_func,
                expr_map,
                stmt_map,
            ),
        },
        StmtKind::Val {
            binding,
            value,
            next,
        } => StmtKind::Val {
            binding,
            value: clone_stmt_graph(
                program,
                value,
                source_func,
                specialized_func,
                expr_map,
                stmt_map,
            ),
            next: clone_stmt_graph(
                program,
                next,
                source_func,
                specialized_func,
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
        } => StmtKind::Call {
            result,
            callee: if callee == source_func {
                specialized_func
            } else {
                callee
            },
            args: args
                .into_iter()
                .map(|arg| clone_expr_graph(program, arg, source_func, specialized_func, expr_map))
                .collect(),
            effects,
            next: clone_stmt_graph(
                program,
                next,
                source_func,
                specialized_func,
                expr_map,
                stmt_map,
            ),
        },
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => StmtKind::If {
            cond: clone_expr_graph(program, cond, source_func, specialized_func, expr_map),
            then_branch: clone_stmt_graph(
                program,
                then_branch,
                source_func,
                specialized_func,
                expr_map,
                stmt_map,
            ),
            else_branch: clone_stmt_graph(
                program,
                else_branch,
                source_func,
                specialized_func,
                expr_map,
                stmt_map,
            ),
        },
        StmtKind::Match {
            scrutinee,
            arms,
            default,
        } => StmtKind::Match {
            scrutinee: clone_expr_graph(program, scrutinee, source_func, specialized_func, expr_map),
            arms: arms
                .into_iter()
                .map(|arm| crate::ir::core::MatchArm {
                    tag: arm.tag,
                    binders: arm.binders,
                    body: clone_stmt_graph(
                        program,
                        arm.body,
                        source_func,
                        specialized_func,
                        expr_map,
                        stmt_map,
                    ),
                    span: arm.span,
                })
                .collect(),
            default: default.map(|stmt| {
                clone_stmt_graph(
                    program,
                    stmt,
                    source_func,
                    specialized_func,
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
        } => StmtKind::Perform {
            result,
            effect,
            operation,
            args: args
                .into_iter()
                .map(|arg| clone_expr_graph(program, arg, source_func, specialized_func, expr_map))
                .collect(),
            next: clone_stmt_graph(
                program,
                next,
                source_func,
                specialized_func,
                expr_map,
                stmt_map,
            ),
        },
        StmtKind::Handle {
            handler,
            body,
            next,
        } => StmtKind::Handle {
            handler,
            body: clone_stmt_graph(
                program,
                body,
                source_func,
                specialized_func,
                expr_map,
                stmt_map,
            ),
            next: next.map(|stmt| {
                clone_stmt_graph(
                    program,
                    stmt,
                    source_func,
                    specialized_func,
                    expr_map,
                    stmt_map,
                )
            }),
        },
        StmtKind::Stage { stage, body, next } => StmtKind::Stage {
            stage,
            body: clone_stmt_graph(
                program,
                body,
                source_func,
                specialized_func,
                expr_map,
                stmt_map,
            ),
            next: next.map(|stmt| {
                clone_stmt_graph(
                    program,
                    stmt,
                    source_func,
                    specialized_func,
                    expr_map,
                    stmt_map,
                )
            }),
        },
        StmtKind::Hole { ty } => StmtKind::Hole { ty },
        StmtKind::Error(error) => StmtKind::Error(error),
    };

    let cloned = program.push_stmt(StmtNode {
        span: node.span,
        kind,
    });
    stmt_map.insert(source, cloned);
    cloned
}

fn clone_expr_graph(
    program: &mut CoreProgram,
    source: ExprId,
    source_func: FuncId,
    specialized_func: FuncId,
    expr_map: &mut HashMap<ExprId, ExprId>,
) -> ExprId {
    if let Some(existing) = expr_map.get(&source).copied() {
        return existing;
    }

    let Some(node) = program.expr(source).cloned() else {
        return source;
    };

    let kind = match node.kind {
        ExprKind::Var(var) => ExprKind::Var(var),
        ExprKind::Literal(literal) => ExprKind::Literal(literal),
        ExprKind::Unary { op, expr } => ExprKind::Unary {
            op,
            expr: clone_expr_graph(program, expr, source_func, specialized_func, expr_map),
        },
        ExprKind::Binary { op, lhs, rhs } => ExprKind::Binary {
            op,
            lhs: clone_expr_graph(program, lhs, source_func, specialized_func, expr_map),
            rhs: clone_expr_graph(program, rhs, source_func, specialized_func, expr_map),
        },
        ExprKind::PureCall { callee, args } => ExprKind::PureCall {
            callee: if callee == source_func {
                specialized_func
            } else {
                callee
            },
            args: args
                .into_iter()
                .map(|arg| clone_expr_graph(program, arg, source_func, specialized_func, expr_map))
                .collect(),
        },
        ExprKind::MakeStruct { ty, fields } => ExprKind::MakeStruct {
            ty,
            fields: fields
                .into_iter()
                .map(|field| clone_expr_graph(program, field, source_func, specialized_func, expr_map))
                .collect(),
        },
        ExprKind::MakeEnum {
            ty,
            variant,
            fields,
        } => ExprKind::MakeEnum {
            ty,
            variant,
            fields: fields
                .into_iter()
                .map(|field| clone_expr_graph(program, field, source_func, specialized_func, expr_map))
                .collect(),
        },
        ExprKind::Error(error) => ExprKind::Error(error),
    };

    let cloned = program.push_expr(ExprNode {
        span: node.span,
        kind,
    });
    expr_map.insert(source, cloned);
    cloned
}
