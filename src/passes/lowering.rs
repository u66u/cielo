use std::collections::{HashMap, HashSet};

// Pass 1/8: lowering (AST -> Core)
//
// Inputs:
// - Parsed AST (`frontend::ast::Program`)
// - LowerConfig (entrypoint symbols)
//
// Outputs:
// - CoreProgram with dense Expr/Stmt/Handler/Function IDs
// - Diagnostics collected during structural lowering
//
// Invariants:
// - Expr nodes remain pure in Core
// - Effectful constructs (`do`, `handle`) are lowered to Stmt nodes
// - Function declarations exist before body lowering (for call resolution)
//
// Diagnostics:
// - Unknown vars/functions/effects
// - Unsupported expression forms in v0 lowering
//
// Complexity:
// - Linear in AST size (single walk + reverse statement stitching per block)

use crate::common::diagnostics::DiagnosticBag;
use crate::common::ids::{EffectLabelId, FuncId, SymbolId, TypeId, VarId};
use crate::common::span::Span;
use crate::frontend::ast::{self, ExprKind as AstExprKind, Item, Stmt as AstStmt};
use crate::ir::core::{
    AdtEnumDecl, AdtEnumVariantDecl, AdtStructDecl, BinaryOp, CoreProgram, ExprKind, ExprNode,
    FunctionDecl, HandlerClause, HandlerDef, Literal, StageDirective, StmtKind, StmtNode, UnaryOp,
};
use crate::sema::effect::SortedEffectRow;

#[derive(Clone, Debug)]
pub struct LowerOutput {
    pub program: CoreProgram,
    pub diagnostics: DiagnosticBag,
}

#[derive(Clone, Debug)]
pub struct LowerConfig {
    pub entrypoints: Vec<SymbolId>,
}

impl Default for LowerConfig {
    fn default() -> Self {
        Self {
            entrypoints: Vec::new(),
        }
    }
}

impl LowerConfig {
    pub fn with_entrypoint(entrypoint: SymbolId) -> Self {
        Self {
            entrypoints: vec![entrypoint],
        }
    }
}

pub fn lower_program(ast: &ast::Program, config: LowerConfig) -> LowerOutput {
    let mut lowerer = Lowerer::new(config);
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
    effect_labels: HashMap<SymbolId, EffectLabelId>,
    effect_ops: HashMap<(EffectLabelId, SymbolId), usize>,
    struct_ctors: HashMap<SymbolId, usize>,
    enum_ctors: HashMap<SymbolId, (SymbolId, usize)>,
    config: LowerConfig,
}

enum LoweredValue {
    Expr(crate::common::ids::ExprId),
    Stmt(crate::common::ids::StmtId),
}

impl Lowerer {
    fn new(config: LowerConfig) -> Self {
        Self {
            program: CoreProgram::new(),
            diagnostics: DiagnosticBag::default(),
            next_var: 0,
            functions_by_name: HashMap::new(),
            effect_labels: HashMap::new(),
            effect_ops: HashMap::new(),
            struct_ctors: HashMap::new(),
            enum_ctors: HashMap::new(),
            config,
        }
    }

    fn lower(&mut self, ast: &ast::Program) {
        let mut function_work = Vec::new();
        for item in &ast.items {
            match item {
                Item::Effect(effect) => {
                    let effect_id = EffectLabelId::new(self.effect_labels.len());
                    self.effect_labels.insert(effect.name, effect_id);
                    for operation in &effect.operations {
                        self.effect_ops
                            .insert((effect_id, operation.name), operation.params.len());
                    }
                }
                Item::Struct(decl) => {
                    self.struct_ctors.insert(decl.name, decl.fields.len());
                    self.program.add_struct(AdtStructDecl {
                        name: decl.name,
                        field_count: decl.fields.len(),
                        span: decl.span,
                    });
                }
                Item::Enum(decl) => {
                    let mut variants = Vec::with_capacity(decl.variants.len());
                    for variant in &decl.variants {
                        self.enum_ctors
                            .insert(variant.name, (decl.name, variant.fields.len()));
                        variants.push(AdtEnumVariantDecl {
                            name: variant.name,
                            field_count: variant.fields.len(),
                            span: variant.span,
                        });
                    }
                    self.program.add_enum(AdtEnumDecl {
                        name: decl.name,
                        variants,
                        span: decl.span,
                    });
                }
                _ => {}
            }
        }

        for item in &ast.items {
            if let Item::Function(function) = item {
                let mut param_vars = Vec::with_capacity(function.params.len());
                for _ in &function.params {
                    param_vars.push(self.fresh_var());
                }
                let dummy = self.make_dummy_body(function.span);
                let mut declared_effects = Vec::with_capacity(function.effects.len());
                for name in &function.effects {
                    if let Some(effect_id) = self.effect_labels.get(name).copied() {
                        declared_effects.push(effect_id);
                    } else {
                        self.diagnostics.error(
                            "LOWER_UNKNOWN_EFFECT_ANNOT",
                            "Unknown effect in function `with` annotation",
                            function.span,
                        );
                    }
                }
                let func_id = self.program.add_function(FunctionDecl {
                    name: function.name,
                    params: param_vars.clone(),
                    param_types: vec![TypeId::INVALID; param_vars.len()],
                    return_type: TypeId::INVALID,
                    declared_effects: SortedEffectRow::new(declared_effects),
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
                    .filter(|_| self.config.entrypoints.contains(&function.name)),
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
        enum Action {
            Let {
                span: Span,
                binding: VarId,
                value: crate::common::ids::ExprId,
            },
            Val {
                span: Span,
                binding: VarId,
                value: crate::common::ids::StmtId,
            },
            Perform {
                span: Span,
                effect: EffectLabelId,
                operation: SymbolId,
                args: Vec<crate::common::ids::ExprId>,
            },
        }

        let mut locals = outer_locals.clone();
        let mut actions: Vec<Action> = Vec::new();

        for stmt in &block.statements {
            match stmt {
                AstStmt::Let {
                    name, value, span, ..
                } => {
                    let binding = self.fresh_var();
                    let lowered = self.lower_binding_value(value, &locals);
                    locals.insert(*name, binding);
                    match lowered {
                        LoweredValue::Expr(value) => actions.push(Action::Let {
                            span: *span,
                            binding,
                            value,
                        }),
                        LoweredValue::Stmt(value) => actions.push(Action::Val {
                            span: *span,
                            binding,
                            value,
                        }),
                    }
                }
                AstStmt::Expr { value, span } => {
                    let temp = self.fresh_var();
                    match self.lower_binding_value(value, &locals) {
                        LoweredValue::Expr(value) => actions.push(Action::Let {
                            span: *span,
                            binding: temp,
                            value,
                        }),
                        LoweredValue::Stmt(value) => actions.push(Action::Val {
                            span: *span,
                            binding: temp,
                            value,
                        }),
                    }
                }
                AstStmt::Perform {
                    effect,
                    operation,
                    args,
                    span,
                } => {
                    let effect_label =
                        self.effect_labels.get(effect).copied().unwrap_or_else(|| {
                            self.diagnostics.error(
                                "LOWER_UNKNOWN_EFFECT",
                                "Unknown effect in `do` statement during AST->Core lowering",
                                *span,
                            );
                            EffectLabelId::INVALID
                        });

                    if effect_label.is_valid() {
                        match self.effect_ops.get(&(effect_label, *operation)) {
                            Some(expected) if *expected != args.len() => {
                                self.diagnostics.error(
                                    "LOWER_BAD_EFFECT_OP_ARITY",
                                    format!(
                                        "Effect operation argument count mismatch: expected {}, got {}",
                                        expected,
                                        args.len()
                                    ),
                                    *span,
                                );
                            }
                            Some(_) => {}
                            None => {
                                self.diagnostics.error(
                                    "LOWER_UNKNOWN_EFFECT_OP",
                                    "Unknown operation for this effect in `do` statement",
                                    *span,
                                );
                            }
                        }
                    }

                    let lowered_args = args
                        .iter()
                        .map(|arg| self.lower_expr(arg, &locals))
                        .collect();
                    actions.push(Action::Perform {
                        span: *span,
                        effect: effect_label,
                        operation: *operation,
                        args: lowered_args,
                    });
                }
                AstStmt::Error(error) => {
                    let expr_id = self.push_expr(ExprKind::Error(error.clone()), error.span);
                    let temp = self.fresh_var();
                    actions.push(Action::Let {
                        span: error.span,
                        binding: temp,
                        value: expr_id,
                    });
                }
            }
        }

        let mut next = if let Some(tail) = &block.tail {
            match self.lower_binding_value(tail, &locals) {
                LoweredValue::Expr(value) => self.push_stmt(StmtKind::Return(value), block.span),
                LoweredValue::Stmt(value) => {
                    let binding = self.fresh_var();
                    let return_value = self.push_expr(ExprKind::Var(binding), tail.span);
                    let return_stmt = self.push_stmt(StmtKind::Return(return_value), tail.span);
                    self.push_stmt(
                        StmtKind::Val {
                            binding,
                            value,
                            next: return_stmt,
                        },
                        tail.span,
                    )
                }
            }
        } else {
            let unit = self.push_expr(ExprKind::Literal(Literal::Unit), block.span);
            self.push_stmt(StmtKind::Return(unit), block.span)
        };

        for action in actions.into_iter().rev() {
            next = match action {
                Action::Let {
                    span,
                    binding,
                    value,
                } => self.push_stmt(
                    StmtKind::Let {
                        binding,
                        value,
                        next,
                    },
                    span,
                ),
                Action::Val {
                    span,
                    binding,
                    value,
                } => self.push_stmt(
                    StmtKind::Val {
                        binding,
                        value,
                        next,
                    },
                    span,
                ),
                Action::Perform {
                    span,
                    effect,
                    operation,
                    args,
                } => self.push_stmt(
                    StmtKind::Perform {
                        result: None,
                        effect,
                        operation,
                        args,
                        next,
                    },
                    span,
                ),
            };
        }
        next
    }

    fn lower_binding_value(
        &mut self,
        value: &ast::Expr,
        locals: &HashMap<SymbolId, VarId>,
    ) -> LoweredValue {
        if let Some(stmt) = self.lower_effectful_expr(value, locals) {
            LoweredValue::Stmt(stmt)
        } else {
            LoweredValue::Expr(self.lower_expr(value, locals))
        }
    }

    fn lower_effectful_expr(
        &mut self,
        expr: &ast::Expr,
        locals: &HashMap<SymbolId, VarId>,
    ) -> Option<crate::common::ids::StmtId> {
        if let AstExprKind::StageBlock { stage, block } = &expr.kind {
            let mut block_locals = locals.clone();
            let body = self.lower_block(block, &mut block_locals);
            return Some(self.push_stmt(
                StmtKind::Stage {
                    stage: map_stage_marker(*stage),
                    body,
                    next: None,
                },
                expr.span,
            ));
        }

        if let AstExprKind::Call { callee, args } = &expr.kind
            && let AstExprKind::Var(symbol) = callee.kind
            && let Some(&func_id) = self.functions_by_name.get(&symbol)
        {
            let effects = self
                .program
                .function(func_id)
                .map(|f| f.declared_effects.clone())
                .filter(|row| !row.is_empty());
            let Some(effects) = effects else {
                return None;
            };
            let result = self.fresh_var();
            let arg_ids = args
                .iter()
                .map(|arg| self.lower_expr(arg, locals))
                .collect();
            let return_expr = self.push_expr(ExprKind::Var(result), expr.span);
            let return_stmt = self.push_stmt(StmtKind::Return(return_expr), expr.span);
            return Some(self.push_stmt(
                StmtKind::Call {
                    result,
                    callee: func_id,
                    args: arg_ids,
                    effects,
                    next: return_stmt,
                },
                expr.span,
            ));
        }

        let AstExprKind::Handle {
            body,
            effect,
            clauses,
        } = &expr.kind
        else {
            return None;
        };

        let effect_label = self.effect_labels.get(effect).copied().unwrap_or_else(|| {
            self.diagnostics.error(
                "LOWER_UNKNOWN_HANDLER_EFFECT",
                "Unknown effect in `handle` expression during AST->Core lowering",
                expr.span,
            );
            EffectLabelId::INVALID
        });

        let return_param = self.fresh_var();
        let return_expr = self.push_expr(ExprKind::Var(return_param), expr.span);
        let return_body = self.push_stmt(StmtKind::Return(return_expr), expr.span);

        let mut core_clauses = Vec::with_capacity(clauses.len());
        let mut seen_clause_ops = HashSet::new();
        for clause in clauses {
            let mut clause_locals = locals.clone();
            let mut params = Vec::with_capacity(clause.params.len());
            for param in &clause.params {
                let var = self.fresh_var();
                clause_locals.insert(*param, var);
                params.push(var);
            }

            if !seen_clause_ops.insert(clause.operation) {
                self.diagnostics.error(
                    "LOWER_DUP_HANDLER_CLAUSE",
                    "Duplicate handler clause for operation",
                    clause.span,
                );
            }

            if effect_label.is_valid() {
                match self.effect_ops.get(&(effect_label, clause.operation)) {
                    Some(expected) if *expected != clause.params.len() => {
                        self.diagnostics.error(
                            "LOWER_BAD_HANDLER_CLAUSE_ARITY",
                            format!(
                                "Handler clause parameter count mismatch: expected {}, got {}",
                                expected,
                                clause.params.len()
                            ),
                            clause.span,
                        );
                    }
                    Some(_) => {}
                    None => {
                        self.diagnostics.error(
                            "LOWER_UNKNOWN_HANDLER_OP",
                            "Unknown operation in handler clause for this effect",
                            clause.span,
                        );
                    }
                }
            }

            let clause_body = self.lower_block(&clause.body, &mut clause_locals);
            core_clauses.push(HandlerClause {
                operation: clause.operation,
                params,
                resume_param: None,
                body: clause_body,
                span: clause.span,
            });
        }

        let handler_id = self.program.add_handler(HandlerDef {
            effect: effect_label,
            return_param,
            return_body,
            clauses: core_clauses,
            span: expr.span,
        });

        let body_stmt = if let Some(stmt) = self.lower_effectful_expr(body, locals) {
            stmt
        } else {
            match &body.kind {
                AstExprKind::Block(block) => {
                    let mut block_locals = locals.clone();
                    self.lower_block(block, &mut block_locals)
                }
                _ => {
                    let body_expr = self.lower_expr(body, locals);
                    self.push_stmt(StmtKind::Return(body_expr), body.span)
                }
            }
        };
        let handled = self.push_stmt(
            StmtKind::Handle {
                handler: handler_id,
                body: body_stmt,
                next: None,
            },
            expr.span,
        );
        Some(handled)
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
                        let is_effectful = self
                            .program
                            .function(*func_id)
                            .is_some_and(|f| !f.declared_effects.is_empty());
                        if is_effectful {
                            let error = self.diagnostics.error_node(
                                "LOWER_EFFECTFUL_CALL_PURE_CTX",
                                "Effectful call used where a pure expression is required in v0",
                                expr.span,
                            );
                            return self.push_expr(ExprKind::Error(error), expr.span);
                        }
                        ExprKind::PureCall {
                            callee: *func_id,
                            args: args
                                .iter()
                                .map(|arg| self.lower_expr(arg, locals))
                                .collect(),
                        }
                    } else if let Some((enum_name, expected)) =
                        self.enum_ctors.get(&symbol).copied()
                    {
                        if expected != args.len() {
                            self.diagnostics.error(
                                "LOWER_BAD_ENUM_CTOR_ARITY",
                                format!(
                                    "Enum constructor argument count mismatch: expected {}, got {}",
                                    expected,
                                    args.len()
                                ),
                                expr.span,
                            );
                        }
                        ExprKind::MakeEnum {
                            ty: enum_name,
                            variant: symbol,
                            fields: args
                                .iter()
                                .map(|arg| self.lower_expr(arg, locals))
                                .collect(),
                        }
                    } else if let Some(expected) = self.struct_ctors.get(&symbol).copied() {
                        if expected != args.len() {
                            self.diagnostics.error(
                                "LOWER_BAD_STRUCT_CTOR_ARITY",
                                format!(
                                    "Struct constructor argument count mismatch: expected {}, got {}",
                                    expected,
                                    args.len()
                                ),
                                expr.span,
                            );
                        }
                        ExprKind::MakeStruct {
                            ty: symbol,
                            fields: args
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
            AstExprKind::If { .. }
            | AstExprKind::Block(_)
            | AstExprKind::StageBlock { .. }
            | AstExprKind::Handle { .. } => {
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

fn map_stage_marker(stage: ast::StageMarker) -> StageDirective {
    match stage {
        ast::StageMarker::Comptime => StageDirective::Comptime,
        ast::StageMarker::Runtime => StageDirective::Runtime,
    }
}
