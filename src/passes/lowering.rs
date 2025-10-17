use std::collections::HashMap;

use crate::common::diagnostics::DiagnosticBag;
use crate::common::ids::{FuncId, SymbolId, TypeId, VarId};
use crate::common::span::Span;
use crate::frontend::ast::{self, ExprKind as AstExprKind, Item, Stmt as AstStmt};
use crate::ir::core::{
    BinaryOp, CoreProgram, ExprKind, ExprNode, FunctionDecl, Literal, StmtKind, StmtNode, UnaryOp,
};
use crate::sema::effect::SortedEffectRow;

#[derive(Clone, Debug)]
pub struct LowerOutput {
    pub program: CoreProgram,
    pub diagnostics: DiagnosticBag,
}

pub fn lower_program(ast: &ast::Program) -> LowerOutput {
    let mut lowerer = Lowerer::new();
    lowerer.lower(ast);
    LowerOutput {
        program: lowerer.program,
        diagnostics: lowerer.diagnostics,
    }
}

struct Lowerer {
    program: CoreProgram,
    diagnostics: DiagnosticBag,
    next_var: u32,
    functions_by_name: HashMap<SymbolId, FuncId>,
}

impl Lowerer {
    fn new() -> Self {
        Self {
            program: CoreProgram::new(),
            diagnostics: DiagnosticBag::default(),
            next_var: 0,
            functions_by_name: HashMap::new(),
        }
    }

    fn lower(&mut self, ast: &ast::Program) {
        let mut function_work = Vec::new();

        for item in &ast.items {
            if let Item::Function(function) = item {
                let mut param_vars = Vec::with_capacity(function.params.len());
                for _ in &function.params {
                    param_vars.push(self.fresh_var());
                }
                let dummy = self.make_dummy_body(function.span);
                let func_id = self.program.add_function(FunctionDecl {
                    name: function.name,
                    params: param_vars.clone(),
                    param_types: vec![TypeId::INVALID; param_vars.len()],
                    return_type: TypeId::INVALID,
                    declared_effects: SortedEffectRow::empty(),
                    body: dummy,
                    ct_only: false,
                    span: function.span,
                });
                self.functions_by_name.insert(function.name, func_id);
                function_work.push((func_id, function, param_vars));
            }
        }

        for (func_id, function, param_vars) in function_work {
            let mut locals = HashMap::new();
            for (param, var_id) in function.params.iter().zip(param_vars.iter().copied()) {
                locals.insert(param.name, var_id);
            }

            if !function.effects.is_empty() {
                self.diagnostics.note(
                    "LOWER_EFFECTS_TODO",
                    "Function effects are parsed but not yet lowered into effect rows in v0",
                    function.span,
                );
            }

            let body = self.lower_block(&function.body, &mut locals);
            if let Some(core_fn) = self.program.function_mut(func_id) {
                core_fn.body = body;
            }
        }

        let entrypoints = ast
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Function(function) => self
                    .functions_by_name
                    .get(&function.name)
                    .copied()
                    .filter(|_| self.symbol_is_named(function.name, "main")),
                _ => None,
            })
            .collect::<Vec<_>>();
        self.program.set_entrypoints(entrypoints);
    }

    fn lower_block(
        &mut self,
        block: &ast::BlockExpr,
        outer_locals: &mut HashMap<SymbolId, VarId>,
    ) -> crate::common::ids::StmtId {
        let mut locals = outer_locals.clone();
        let mut lowered_seq: Vec<(Span, VarId, crate::common::ids::ExprId)> = Vec::new();

        for stmt in &block.statements {
            match stmt {
                AstStmt::Let {
                    name, value, span, ..
                } => {
                    let binding = self.fresh_var();
                    let value_id = self.lower_expr(value, &locals);
                    locals.insert(*name, binding);
                    lowered_seq.push((*span, binding, value_id));
                }
                AstStmt::Expr { value, span } => {
                    let temp = self.fresh_var();
                    let value_id = self.lower_expr(value, &locals);
                    lowered_seq.push((*span, temp, value_id));
                }
                AstStmt::Error(error) => {
                    let expr_id = self.push_expr(ExprKind::Error(error.clone()), error.span);
                    let temp = self.fresh_var();
                    lowered_seq.push((error.span, temp, expr_id));
                }
            }
        }

        let tail_expr = if let Some(tail) = &block.tail {
            self.lower_expr(tail, &locals)
        } else {
            self.push_expr(ExprKind::Literal(Literal::Unit), block.span)
        };
        let mut next = self.push_stmt(StmtKind::Return(tail_expr), block.span);

        for (span, binding, value) in lowered_seq.into_iter().rev() {
            next = self.push_stmt(
                StmtKind::Let {
                    binding,
                    value,
                    next,
                },
                span,
            );
        }
        next
    }

    fn lower_expr(
        &mut self,
        expr: &ast::Expr,
        locals: &HashMap<SymbolId, VarId>,
    ) -> crate::common::ids::ExprId {
        let kind = match &expr.kind {
            AstExprKind::Int(value) => ExprKind::Literal(Literal::Int(*value)),
            AstExprKind::Bool(value) => ExprKind::Literal(Literal::Bool(*value)),
            AstExprKind::String(value) => ExprKind::Literal(Literal::String(value.clone())),
            AstExprKind::Var(name) => {
                if let Some(var_id) = locals.get(name) {
                    ExprKind::Var(*var_id)
                } else {
                    let error = self.diagnostics.error_node(
                        "LOWER_UNKNOWN_VAR",
                        "Unknown variable during AST->Core lowering",
                        expr.span,
                    );
                    ExprKind::Error(error)
                }
            }
            AstExprKind::Unary { op, expr: inner } => ExprKind::Unary {
                op: map_unary_op(*op),
                expr: self.lower_expr(inner, locals),
            },
            AstExprKind::Binary { op, lhs, rhs } => ExprKind::Binary {
                op: map_binary_op(*op),
                lhs: self.lower_expr(lhs, locals),
                rhs: self.lower_expr(rhs, locals),
            },
            AstExprKind::Call { callee, args } => {
                if let AstExprKind::Var(symbol) = callee.kind {
                    if let Some(func_id) = self.functions_by_name.get(&symbol) {
                        ExprKind::PureCall {
                            callee: *func_id,
                            args: args
                                .iter()
                                .map(|arg| self.lower_expr(arg, locals))
                                .collect(),
                        }
                    } else {
                        let error = self.diagnostics.error_node(
                            "LOWER_UNKNOWN_FUNC",
                            "Unknown function call target during AST->Core lowering",
                            expr.span,
                        );
                        ExprKind::Error(error)
                    }
                } else {
                    let error = self.diagnostics.error_node(
                        "LOWER_COMPLEX_CALLEE",
                        "Only direct calls by function name are supported in v0 lowering",
                        expr.span,
                    );
                    ExprKind::Error(error)
                }
            }
            AstExprKind::If { .. } | AstExprKind::Block(_) | AstExprKind::StageBlock { .. } => {
                let error = self.diagnostics.error_node(
                    "LOWER_EXPR_UNSUPPORTED",
                    "This expression form is parsed but not lowered yet in v0",
                    expr.span,
                );
                ExprKind::Error(error)
            }
            AstExprKind::Error(error) => ExprKind::Error(error.clone()),
        };
        self.push_expr(kind, expr.span)
    }

    fn make_dummy_body(&mut self, span: Span) -> crate::common::ids::StmtId {
        let unit = self.push_expr(ExprKind::Literal(Literal::Unit), span);
        self.push_stmt(StmtKind::Return(unit), span)
    }

    fn push_expr(&mut self, kind: ExprKind, span: Span) -> crate::common::ids::ExprId {
        self.program.push_expr(ExprNode { span, kind })
    }

    fn push_stmt(&mut self, kind: StmtKind, span: Span) -> crate::common::ids::StmtId {
        self.program.push_stmt(StmtNode { span, kind })
    }

    fn fresh_var(&mut self) -> VarId {
        let id = VarId::from_u32(self.next_var);
        self.next_var += 1;
        id
    }

    fn symbol_is_named(&self, _symbol: SymbolId, _expected: &str) -> bool {
        // We intentionally keep lowering independent from the interner in this layer.
        // Entry point wiring will move to typed/name-resolved passes.
        false
    }
}

fn map_unary_op(op: ast::UnaryOp) -> UnaryOp {
    match op {
        ast::UnaryOp::Neg => UnaryOp::Neg,
        ast::UnaryOp::Not => UnaryOp::Not,
    }
}

fn map_binary_op(op: ast::BinOp) -> BinaryOp {
    match op {
        ast::BinOp::Add => BinaryOp::Add,
        ast::BinOp::Sub => BinaryOp::Sub,
        ast::BinOp::Mul => BinaryOp::Mul,
        ast::BinOp::Div => BinaryOp::Div,
        ast::BinOp::Mod => BinaryOp::Mod,
        ast::BinOp::Eq => BinaryOp::Eq,
        ast::BinOp::Ne => BinaryOp::Ne,
        ast::BinOp::Lt => BinaryOp::Lt,
        ast::BinOp::Le => BinaryOp::Le,
        ast::BinOp::Gt => BinaryOp::Gt,
        ast::BinOp::Ge => BinaryOp::Ge,
        ast::BinOp::And => BinaryOp::And,
        ast::BinOp::Or => BinaryOp::Or,
    }
}
