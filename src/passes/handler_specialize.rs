// Pass 7/9: handler_specialize (bounded handler-call specialization groundwork)
//
// Inputs:
// - Residualized Core program
//
// Outputs:
// - Residualized Core program with specialized function copies for handle-wrapped calls
//
// Invariants:
// - A pair (callee, handler-shape) specializes at most once
// - Specialized functions preserve signatures and clone the original body graph
// - Recursive calls in specialized copies are retargeted to the specialized function id
//
// Complexity:
// - O(stmt_count + cloned_nodes)

use std::collections::HashMap;

use crate::common::ids::{EffectLabelId, ExprId, FuncId, HandlerId, StmtId, SymbolId, VarId};
use crate::ir::core::{
    CoreProgram, ExprKind, ExprNode, FunctionDecl, HandlerDef, Literal, StmtKind, StmtNode,
};
use crate::pipeline::phases::Residualized;

pub fn run(residual: Residualized) -> Residualized {
    let (mut program, diagnostics, sema, mono, ct, bta, residual_tables) = residual.into_parts();
    specialize_handle_wrapped_calls(&mut program);
    Residualized::new(program, diagnostics, sema, mono, ct, bta, residual_tables)
}

#[derive(Clone)]
struct SpecializeCandidate {
    handler: HandlerId,
    shape: HandlerShapeKey,
    call_stmt: StmtId,
    callee: FuncId,
}

fn specialize_handle_wrapped_calls(program: &mut CoreProgram) {
    let candidates = collect_specialize_candidates(program);
    let mut specialized: HashMap<(FuncId, HandlerShapeKey), FuncId> = HashMap::new();

    for candidate in candidates {
        let specialized_callee = ensure_specialized(program, &candidate, &mut specialized);
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
    let shapes = collect_handler_shapes(program);
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
        let Some(shape) = shapes.get(handler.index()).cloned() else {
            continue;
        };
        out.push(SpecializeCandidate {
            handler: *handler,
            shape,
            call_stmt,
            callee,
        });
    }
    out
}

fn collect_handler_shapes(program: &CoreProgram) -> Vec<HandlerShapeKey> {
    program
        .handlers()
        .iter()
        .map(|handler| HandlerShapeKey::build(program, handler))
        .collect()
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
    candidate: &SpecializeCandidate,
    specialized: &mut HashMap<(FuncId, HandlerShapeKey), FuncId>,
) -> FuncId {
    let key = (candidate.callee, candidate.shape.clone());
    if let Some(existing) = specialized.get(&key).copied() {
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

    specialized.insert(key, specialized_id);
    specialized_id
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct HandlerShapeKey {
    effect: EffectLabelId,
    return_body: String,
    clauses: Vec<ClauseShapeKey>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ClauseShapeKey {
    operation: SymbolId,
    param_count: usize,
    has_resume: bool,
    body: String,
}

impl HandlerShapeKey {
    fn build(program: &CoreProgram, handler: &HandlerDef) -> Self {
        let mut ret_scope = ScopeCanon::default();
        let _ = ret_scope.bind(handler.return_param);
        let return_body = stmt_shape_text(program, handler.return_body, &mut ret_scope);

        let clauses = handler
            .clauses
            .iter()
            .map(|clause| {
                let mut clause_scope = ScopeCanon::default();
                for param in &clause.params {
                    let _ = clause_scope.bind(*param);
                }
                if let Some(resume) = clause.resume_param {
                    let _ = clause_scope.bind(resume);
                }
                ClauseShapeKey {
                    operation: clause.operation,
                    param_count: clause.params.len(),
                    has_resume: clause.resume_param.is_some(),
                    body: stmt_shape_text(program, clause.body, &mut clause_scope),
                }
            })
            .collect();

        Self {
            effect: handler.effect,
            return_body,
            clauses,
        }
    }
}

#[derive(Default)]
struct ScopeCanon {
    map: HashMap<VarId, u32>,
    next: u32,
}

impl ScopeCanon {
    fn bind(&mut self, var: VarId) -> u32 {
        if let Some(existing) = self.map.get(&var).copied() {
            return existing;
        }
        let id = self.next;
        self.next += 1;
        self.map.insert(var, id);
        id
    }

    fn render_var(&self, var: VarId) -> String {
        if let Some(bound) = self.map.get(&var).copied() {
            return format!("b{bound}");
        }
        format!("f{}", var.as_u32())
    }
}

fn stmt_shape_text(program: &CoreProgram, stmt_id: StmtId, scope: &mut ScopeCanon) -> String {
    let Some(stmt) = program.stmt(stmt_id) else {
        return "stmt:missing".to_owned();
    };

    match &stmt.kind {
        StmtKind::Return(expr) => format!("ret({})", expr_shape_text(program, *expr, scope)),
        StmtKind::Let {
            binding,
            value,
            next,
        } => {
            let value_repr = expr_shape_text(program, *value, scope);
            let bind = scope.bind(*binding);
            let next_repr = stmt_shape_text(program, *next, scope);
            format!("let(b{bind},{value_repr},{next_repr})")
        }
        StmtKind::Val {
            binding,
            value,
            next,
        } => {
            let value_repr = stmt_shape_text(program, *value, scope);
            let bind = scope.bind(*binding);
            let next_repr = stmt_shape_text(program, *next, scope);
            format!("val(b{bind},{value_repr},{next_repr})")
        }
        StmtKind::Call {
            result,
            callee,
            args,
            effects,
            next,
        } => {
            let args_repr = args
                .iter()
                .map(|arg| expr_shape_text(program, *arg, scope))
                .collect::<Vec<_>>()
                .join(",");
            let effects_repr = effects
                .iter()
                .map(|effect| effect.as_u32().to_string())
                .collect::<Vec<_>>()
                .join(",");
            let result_id = scope.bind(*result);
            let next_repr = stmt_shape_text(program, *next, scope);
            format!(
                "call(f{},b{result_id},[{args_repr}],[{effects_repr}],{next_repr})",
                callee.as_u32()
            )
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            let cond_repr = expr_shape_text(program, *cond, scope);
            let then_repr = stmt_shape_text(program, *then_branch, scope);
            let else_repr = stmt_shape_text(program, *else_branch, scope);
            format!("if({cond_repr},{then_repr},{else_repr})")
        }
        StmtKind::Match {
            scrutinee,
            arms,
            default,
        } => {
            let scrutinee_repr = expr_shape_text(program, *scrutinee, scope);
            let arms_repr = arms
                .iter()
                .map(|arm| {
                    let mut arm_scope = ScopeCanon {
                        map: scope.map.clone(),
                        next: scope.next,
                    };
                    let binders = arm
                        .binders
                        .iter()
                        .map(|var| {
                            let id = arm_scope.bind(*var);
                            format!("b{id}")
                        })
                        .collect::<Vec<_>>()
                        .join(",");
                    let body = stmt_shape_text(program, arm.body, &mut arm_scope);
                    format!("arm(t{},[{binders}],{body})", arm.tag.as_u32())
                })
                .collect::<Vec<_>>()
                .join(",");
            let default_repr = default
                .map(|stmt| stmt_shape_text(program, stmt, scope))
                .unwrap_or_else(|| "none".to_owned());
            format!("match({scrutinee_repr},[{arms_repr}],{default_repr})")
        }
        StmtKind::Perform {
            result,
            effect,
            operation,
            args,
            next,
        } => {
            let result_repr = result
                .map(|var| format!("b{}", scope.bind(var)))
                .unwrap_or_else(|| "none".to_owned());
            let args_repr = args
                .iter()
                .map(|arg| expr_shape_text(program, *arg, scope))
                .collect::<Vec<_>>()
                .join(",");
            let next_repr = stmt_shape_text(program, *next, scope);
            format!(
                "perform(e{},op{}, {result_repr},[{args_repr}],{next_repr})",
                effect.as_u32(),
                operation.as_u32()
            )
        }
        StmtKind::Resume {
            result,
            resume,
            arg,
            next,
        } => {
            let result_bind = scope.bind(*result);
            let arg_repr = expr_shape_text(program, *arg, scope);
            let next_repr = stmt_shape_text(program, *next, scope);
            format!(
                "resume(v{},b{result_bind},{arg_repr},{next_repr})",
                resume.as_u32()
            )
        }
        StmtKind::Handle {
            handler,
            body,
            next,
        } => {
            let body_repr = stmt_shape_text(program, *body, scope);
            let next_repr = next
                .map(|stmt| stmt_shape_text(program, stmt, scope))
                .unwrap_or_else(|| "none".to_owned());
            format!("handle(h{}, {body_repr}, {next_repr})", handler.as_u32())
        }
        StmtKind::Stage { stage, body, next } => {
            let body_repr = stmt_shape_text(program, *body, scope);
            let next_repr = next
                .map(|stmt| stmt_shape_text(program, stmt, scope))
                .unwrap_or_else(|| "none".to_owned());
            format!("stage({stage:?}, {body_repr}, {next_repr})")
        }
        StmtKind::Hole { ty } => format!("hole(t{})", ty.as_u32()),
        StmtKind::Error(_) => "stmt:error".to_owned(),
    }
}

fn expr_shape_text(program: &CoreProgram, expr_id: ExprId, scope: &ScopeCanon) -> String {
    let Some(expr) = program.expr(expr_id) else {
        return "expr:missing".to_owned();
    };

    match &expr.kind {
        ExprKind::Var(var) => format!("var({})", scope.render_var(*var)),
        ExprKind::Literal(literal) => format!("lit({})", literal_shape_text(literal)),
        ExprKind::Unary { op, expr } => {
            let nested = expr_shape_text(program, *expr, scope);
            format!("un({op:?},{nested})")
        }
        ExprKind::Binary { op, lhs, rhs } => {
            let lhs_repr = expr_shape_text(program, *lhs, scope);
            let rhs_repr = expr_shape_text(program, *rhs, scope);
            format!("bin({op:?},{lhs_repr},{rhs_repr})")
        }
        ExprKind::PureCall { callee, args } => {
            let args_repr = args
                .iter()
                .map(|arg| expr_shape_text(program, *arg, scope))
                .collect::<Vec<_>>()
                .join(",");
            format!("pcall(f{},[{args_repr}])", callee.as_u32())
        }
        ExprKind::MakeStruct { ty, fields } => {
            let fields_repr = fields
                .iter()
                .map(|field| expr_shape_text(program, *field, scope))
                .collect::<Vec<_>>()
                .join(",");
            format!("mkstruct(t{},[{fields_repr}])", ty.as_u32())
        }
        ExprKind::MakeEnum {
            ty,
            variant,
            fields,
        } => {
            let fields_repr = fields
                .iter()
                .map(|field| expr_shape_text(program, *field, scope))
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "mkenum(t{},v{},[{fields_repr}])",
                ty.as_u32(),
                variant.as_u32()
            )
        }
        ExprKind::Error(_) => "expr:error".to_owned(),
    }
}

fn literal_shape_text(literal: &Literal) -> String {
    match literal {
        Literal::Unit => "unit".to_owned(),
        Literal::Bool(value) => format!("bool({value})"),
        Literal::Int(value) => format!("int({value})"),
        Literal::Float(value) => format!("float({value})"),
        Literal::Char(value) => format!("char({value})"),
        Literal::String(value) => format!("str({value})"),
    }
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
        StmtKind::Resume {
            result,
            resume,
            arg,
            next,
        } => StmtKind::Resume {
            result,
            resume,
            arg: clone_expr_graph(program, arg, source_func, specialized_func, expr_map),
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
