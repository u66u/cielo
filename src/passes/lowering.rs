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
