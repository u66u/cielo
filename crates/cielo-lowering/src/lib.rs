use std::collections::{HashMap, HashSet};

// Pass 1/9: lowering (AST -> Core)
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

use cielo_base::diagnostics::DiagnosticBag;
use cielo_base::{EffectLabelId, FuncId, HandlerId, Interner, Span, SymbolId, VarId};
use cielo_frontend::ast::{
    self, BuiltinType, EffectCapabilityHint, EffectPropertyHint, ExprKind as AstExprKind, Item,
    Stmt as AstStmt, TypeExpr, TypeExprKind,
};
use cielo_ir::core::{
    AdtEnumDecl, AdtEnumVariantDecl, AdtStructDecl, BinaryOp, CoreProgram, CoreTypeRef, EffectDecl,
    EffectOperationDecl, ExprKind, ExprNode, FunctionDecl, HandlerClause, HandlerDef, Literal,
    MatchArm, PrimitiveTypeRef, StageDirective, StmtKind, StmtNode, UnaryOp,
};
use cielo_ir::effect::{CapabilityLevel, EffectFlags, EffectProperties, SortedEffectRow};
use cielo_ir::target::{Endianness, TargetSpec};

#[derive(Clone, Debug)]
pub struct LowerOutput {
    pub program: CoreProgram,
    pub diagnostics: DiagnosticBag,
}

impl LowerOutput {
    pub fn new(program: CoreProgram, diagnostics: DiagnosticBag) -> Self {
        Self {
            program,
            diagnostics,
        }
    }

    pub fn program(&self) -> &CoreProgram {
        &self.program
    }

    pub fn diagnostics(&self) -> &DiagnosticBag {
        &self.diagnostics
    }

    pub fn into_parts(self) -> (CoreProgram, DiagnosticBag) {
        (self.program, self.diagnostics)
    }
}

#[derive(Clone, Debug, Default)]
pub struct LowerConfig {
    pub entrypoints: Vec<SymbolId>,
    pub target_spec: Option<TargetSpec>,
    pub target_builtins: Option<TargetBuiltinSymbols>,
}

#[derive(Clone, Copy, Debug)]
pub struct TargetBuiltinSymbols {
    target_word_size_bits: SymbolId,
    target_pointer_alignment: SymbolId,
    target_is_big_endian: SymbolId,
}

impl TargetBuiltinSymbols {
    pub fn intern(interner: &mut Interner) -> Self {
        Self {
            target_word_size_bits: interner.intern("target_word_size_bits"),
            target_pointer_alignment: interner.intern("target_pointer_alignment"),
            target_is_big_endian: interner.intern("target_is_big_endian"),
        }
    }
}

impl LowerConfig {
    pub fn with_entrypoint(entrypoint: SymbolId) -> Self {
        Self {
            entrypoints: vec![entrypoint],
            target_spec: None,
            target_builtins: None,
        }
    }

    pub fn with_target_builtins(
        mut self,
        target_spec: TargetSpec,
        target_builtins: TargetBuiltinSymbols,
    ) -> Self {
        self.target_spec = Some(target_spec);
        self.target_builtins = Some(target_builtins);
        self
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
    handler_decls: HashMap<SymbolId, ast::HandlerDecl>,
    active_resume_vars: HashSet<VarId>,
    config: LowerConfig,
}

enum LoweredValue {
    Expr(cielo_base::ExprId),
    Stmt(cielo_base::StmtId),
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
            handler_decls: HashMap::new(),
            active_resume_vars: HashSet::new(),
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
                    let mut operations = Vec::with_capacity(effect.operations.len());
                    for operation in &effect.operations {
                        self.effect_ops
                            .insert((effect_id, operation.name), operation.params.len());
                        operations.push(EffectOperationDecl {
                            name: operation.name,
                            param_types: operation
                                .params
                                .iter()
                                .map(|param| lower_type_ref(&param.ty))
                                .collect(),
                            return_type: operation
                                .return_type
                                .as_ref()
                                .map(lower_type_ref)
                                .unwrap_or(CoreTypeRef::Unit),
                            span: operation.span,
                        });
                    }
                    self.program.add_effect(EffectDecl {
                        label: effect_id,
                        name: effect.name,
                        properties: map_effect_property_hint(effect.properties),
                        operations,
                        span: effect.span,
                    });
                }
                Item::Struct(decl) => {
                    self.struct_ctors.insert(decl.name, decl.fields.len());
                    self.program.add_struct(AdtStructDecl {
                        name: decl.name,
                        fields: decl
                            .fields
                            .iter()
                            .map(|field| lower_type_ref(&field.ty))
                            .collect(),
                        field_names: decl.fields.iter().map(|field| field.name).collect(),
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
                            fields: variant.fields.iter().map(lower_type_ref).collect(),
                            span: variant.span,
                        });
                    }
                    self.program.add_enum(AdtEnumDecl {
                        name: decl.name,
                        variants,
                        span: decl.span,
                    });
                }
                Item::Handler(decl) => {
                    let shadowed = self.handler_decls.insert(decl.name, decl.clone());
                    if shadowed.is_some() {
                        self.diagnostics.error(
                            "LOWER_DUP_HANDLER_DECL",
                            "Duplicate handler declaration",
                            decl.span,
                        );
                    }
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
                    param_types: function
                        .params
                        .iter()
                        .map(|param| lower_type_ref(&param.ty))
                        .collect(),
                    return_type: function
                        .return_type
                        .as_ref()
                        .map(lower_type_ref)
                        .unwrap_or(CoreTypeRef::Unknown),
                    declared_effects: SortedEffectRow::new(declared_effects),
                    body: dummy,
                    ct_only: function.ct_only,
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
    ) -> cielo_base::StmtId {
        enum Action {
            Let {
                span: Span,
                binding: VarId,
                value: cielo_base::ExprId,
            },
            Val {
                span: Span,
                binding: VarId,
                value: cielo_base::StmtId,
            },
            Perform {
                span: Span,
                binding: Option<VarId>,
                effect: EffectLabelId,
                operation: SymbolId,
                args: Vec<cielo_base::ExprId>,
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
                    binding,
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
                    let result = binding.map(|name| {
                        let var = self.fresh_var();
                        locals.insert(name, var);
                        var
                    });
                    actions.push(Action::Perform {
                        span: *span,
                        binding: result,
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
                    binding,
                    effect,
                    operation,
                    args,
                } => self.push_stmt(
                    StmtKind::Perform {
                        result: binding,
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
    ) -> Option<cielo_base::StmtId> {
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

        if let AstExprKind::If {
            cond,
            then_branch,
            else_branch,
        } = &expr.kind
        {
            let cond = self.lower_expr(cond, locals);
            let mut then_locals = locals.clone();
            let then_branch = self.lower_block(then_branch, &mut then_locals);
            let else_branch = if let Some(else_block) = else_branch {
                let mut else_locals = locals.clone();
                self.lower_block(else_block, &mut else_locals)
            } else {
                let unit = self.push_expr(ExprKind::Literal(Literal::Unit), expr.span);
                self.push_stmt(StmtKind::Return(unit), expr.span)
            };
            return Some(self.push_stmt(
                StmtKind::If {
                    cond,
                    then_branch,
                    else_branch,
                },
                expr.span,
            ));
        }

        if let AstExprKind::Match {
            scrutinee,
            clauses,
            default,
        } = &expr.kind
        {
            let scrutinee = self.lower_expr(scrutinee, locals);
            let mut arms = Vec::with_capacity(clauses.len());
            for clause in clauses {
                let mut clause_locals = locals.clone();
                let mut binders = Vec::with_capacity(clause.binders.len());
                for binder in &clause.binders {
                    let var = self.fresh_var();
                    clause_locals.insert(*binder, var);
                    binders.push(var);
                }
                let body = self.lower_block(&clause.body, &mut clause_locals);
                arms.push(MatchArm {
                    tag: clause.tag,
                    binders,
                    body,
                    span: clause.span,
                });
            }
            let default = default.as_ref().map(|block| {
                let mut default_locals = locals.clone();
                self.lower_block(block, &mut default_locals)
            });
            return Some(self.push_stmt(
                StmtKind::Match {
                    scrutinee,
                    arms,
                    default,
                },
                expr.span,
            ));
        }

        if let AstExprKind::Call { callee, args } = &expr.kind
            && let AstExprKind::Var(symbol) = callee.kind
        {
            if let Some(resume_var) = locals
                .get(&symbol)
                .copied()
                .filter(|var| self.active_resume_vars.contains(var))
            {
                let result = self.fresh_var();
                let arg = if args.len() == 1 {
                    self.lower_expr(&args[0], locals)
                } else {
                    self.diagnostics.error(
                        "LOWER_RESUME_ARITY",
                        format!(
                            "`resume` expects exactly one argument in v1, got {}",
                            args.len()
                        ),
                        expr.span,
                    );
                    let error = self.diagnostics.error_node(
                        "LOWER_RESUME_ARITY",
                        "Invalid `resume` call arity",
                        expr.span,
                    );
                    self.push_expr(ExprKind::Error(error), expr.span)
                };
                let return_expr = self.push_expr(ExprKind::Var(result), expr.span);
                let return_stmt = self.push_stmt(StmtKind::Return(return_expr), expr.span);
                return Some(self.push_stmt(
                    StmtKind::Resume {
                        result,
                        resume: resume_var,
                        arg,
                        next: return_stmt,
                    },
                    expr.span,
                ));
            }

            if let Some(&func_id) = self.functions_by_name.get(&symbol) {
                let effects = self
                    .program
                    .function(func_id)
                    .map(|f| f.declared_effects.clone())
                    .filter(|row| !row.is_empty());
                let effects = effects?;
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
        }

        let AstExprKind::Handle { body, handler } = &expr.kind else {
            return None;
        };

        let handler_id = match handler {
            ast::HandlerRef::Inline { effect, clauses } => {
                let effect_label = self.resolve_handler_effect(*effect, expr.span);
                self.lower_handler_def(effect_label, clauses, locals, expr.span)
            }
            ast::HandlerRef::Named(name) => match self.handler_decls.get(name).cloned() {
                // Module scope binds nothing, so clauses lower under empty locals.
                Some(decl) => {
                    let effect_label = self.resolve_handler_effect(decl.effect, decl.span);
                    self.lower_handler_def(effect_label, &decl.clauses, &HashMap::new(), decl.span)
                }
                None => {
                    self.diagnostics.error(
                        "LOWER_UNKNOWN_HANDLER",
                        "Unknown handler name in `handle` expression",
                        expr.span,
                    );
                    // Emitting a handler over an unresolved effect would trip the
                    // pre-staging effect assertion before this diagnostic is read.
                    return Some(self.lower_handle_body(body, locals));
                }
            },
        };

        let body_stmt = self.lower_handle_body(body, locals);
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

    fn lower_handle_body(
        &mut self,
        body: &ast::Expr,
        locals: &HashMap<SymbolId, VarId>,
    ) -> cielo_base::StmtId {
        if let Some(stmt) = self.lower_effectful_expr(body, locals) {
            return stmt;
        }
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
    }

    fn resolve_handler_effect(&mut self, effect: SymbolId, span: Span) -> EffectLabelId {
        self.effect_labels.get(&effect).copied().unwrap_or_else(|| {
            self.diagnostics.error(
                "LOWER_UNKNOWN_HANDLER_EFFECT",
                "Unknown effect in `handle` expression during AST->Core lowering",
                span,
            );
            EffectLabelId::INVALID
        })
    }

    /// Builds a `HandlerDef` from clause syntax. A named handler lowers once per
    /// `handle` site through here, so its Core is indistinguishable from writing
    /// the same clauses inline.
    fn lower_handler_def(
        &mut self,
        effect_label: EffectLabelId,
        clauses: &[ast::HandleClause],
        locals: &HashMap<SymbolId, VarId>,
        span: Span,
    ) -> HandlerId {
        let return_param = self.fresh_var();
        let return_expr = self.push_expr(ExprKind::Var(return_param), span);
        let return_body = self.push_stmt(StmtKind::Return(return_expr), span);

        let mut core_clauses = Vec::with_capacity(clauses.len());
        let mut seen_clause_ops = HashSet::new();
        for clause in clauses {
            let mut clause_locals = locals.clone();
            let mut clause_param_symbols = clause.params.clone();
            let mut resume_symbol = None;

            if !seen_clause_ops.insert(clause.operation) {
                self.diagnostics.error(
                    "LOWER_DUP_HANDLER_CLAUSE",
                    "Duplicate handler clause for operation",
                    clause.span,
                );
            }

            if effect_label.is_valid() {
                match self.effect_ops.get(&(effect_label, clause.operation)) {
                    Some(expected) => {
                        if clause_param_symbols.len() == expected + 1 {
                            resume_symbol = clause_param_symbols.pop();
                        } else if clause_param_symbols.len() != *expected {
                            self.diagnostics.error(
                                "LOWER_BAD_HANDLER_CLAUSE_ARITY",
                                format!(
                                    "Handler clause parameter count mismatch: expected {} or {} (with resume), got {}",
                                    expected,
                                    expected + 1,
                                    clause.params.len()
                                ),
                                clause.span,
                            );
                        }
                    }
                    None => {
                        self.diagnostics.error(
                            "LOWER_UNKNOWN_HANDLER_OP",
                            "Unknown operation in handler clause for this effect",
                            clause.span,
                        );
                    }
                }
            }

            let mut params = Vec::with_capacity(clause_param_symbols.len());
            for param in clause_param_symbols {
                let var = self.fresh_var();
                clause_locals.insert(param, var);
                params.push(var);
            }

            let resume_param = resume_symbol.map(|symbol| {
                let var = self.fresh_var();
                clause_locals.insert(symbol, var);
                var
            });

            let clause_body = if let Some(resume_var) = resume_param {
                self.with_resume_var(resume_var, |lowerer| {
                    lowerer.lower_block(&clause.body, &mut clause_locals)
                })
            } else {
                self.lower_block(&clause.body, &mut clause_locals)
            };
            core_clauses.push(HandlerClause {
                operation: clause.operation,
                params,
                resume_param,
                body: clause_body,
                span: clause.span,
            });
        }

        self.program.add_handler(HandlerDef {
            effect: effect_label,
            return_param,
            return_body,
            clauses: core_clauses,
            span,
        })
    }

    fn lower_expr(
        &mut self,
        expr: &ast::Expr,
        locals: &HashMap<SymbolId, VarId>,
    ) -> cielo_base::ExprId {
        let kind = match &expr.kind {
            AstExprKind::Field { base, field } => ExprKind::Field {
                base: self.lower_expr(base, locals),
                field: *field,
            },
            AstExprKind::Int(value) => ExprKind::Literal(Literal::Int(*value)),
            AstExprKind::Bool(value) => ExprKind::Literal(Literal::Bool(*value)),
            AstExprKind::String(value) => ExprKind::Literal(Literal::String(value.clone())),
            AstExprKind::Var(name) => {
                if let Some(var_id) = locals.get(name) {
                    if self.active_resume_vars.contains(var_id) {
                        let error = self.diagnostics.error_node(
                            "LOWER_RESUME_VALUE_ESCAPE",
                            "`resume` cannot be captured or passed as a value in v1",
                            expr.span,
                        );
                        ExprKind::Error(error)
                    } else {
                        ExprKind::Var(*var_id)
                    }
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
                    if locals
                        .get(&symbol)
                        .copied()
                        .filter(|var| self.active_resume_vars.contains(var))
                        .is_some()
                    {
                        let error = self.diagnostics.error_node(
                            "LOWER_RESUME_PURE_CTX",
                            "`resume(...)` is effectful and must appear in statement/binding position",
                            expr.span,
                        );
                        ExprKind::Error(error)
                    } else if let Some(func_id) = self.functions_by_name.get(&symbol) {
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
                    } else if let Some(target_builtin) =
                        self.lower_target_builtin_call(symbol, args, locals, expr.span)
                    {
                        target_builtin
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
            | AstExprKind::Match { .. }
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

    fn lower_target_builtin_call(
        &mut self,
        callee: SymbolId,
        args: &[ast::Expr],
        locals: &HashMap<SymbolId, VarId>,
        span: Span,
    ) -> Option<ExprKind> {
        let target_spec = self.config.target_spec?;
        let target_builtins = self.config.target_builtins?;

        let literal = if callee == target_builtins.target_word_size_bits {
            Literal::Int(i64::from(target_spec.word_size_bits))
        } else if callee == target_builtins.target_pointer_alignment {
            Literal::Int(i64::from(target_spec.pointer_alignment))
        } else if callee == target_builtins.target_is_big_endian {
            Literal::Bool(matches!(target_spec.endianness, Endianness::Big))
        } else {
            return None;
        };

        if args.is_empty() {
            return Some(ExprKind::Literal(literal));
        }

        for arg in args {
            let _ = self.lower_expr(arg, locals);
        }
        let error = self.diagnostics.error_node(
            "LOWER_TARGET_BUILTIN_ARITY",
            "Target query builtins do not take arguments",
            span,
        );
        Some(ExprKind::Error(error))
    }

    fn make_dummy_body(&mut self, span: Span) -> cielo_base::StmtId {
        let unit = self.push_expr(ExprKind::Literal(Literal::Unit), span);
        self.push_stmt(StmtKind::Return(unit), span)
    }

    fn push_expr(&mut self, kind: ExprKind, span: Span) -> cielo_base::ExprId {
        self.program.push_expr(ExprNode { span, kind })
    }

    fn push_stmt(&mut self, kind: StmtKind, span: Span) -> cielo_base::StmtId {
        self.program.push_stmt(StmtNode { span, kind })
    }

    fn with_resume_var<R>(&mut self, resume_var: VarId, f: impl FnOnce(&mut Self) -> R) -> R {
        self.active_resume_vars.insert(resume_var);
        let out = f(self);
        self.active_resume_vars.remove(&resume_var);
        out
    }

    fn fresh_var(&mut self) -> VarId {
        let id = VarId::from_u32(self.next_var);
        self.next_var += 1;
        id
    }
}

fn lower_type_ref(ty: &TypeExpr) -> CoreTypeRef {
    match &ty.kind {
        TypeExprKind::Unit => CoreTypeRef::Unit,
        TypeExprKind::Builtin(builtin) => CoreTypeRef::Primitive(map_builtin_type(*builtin)),
        TypeExprKind::Path { name, .. } => CoreTypeRef::Named(*name),
        TypeExprKind::Error(_) => CoreTypeRef::Unknown,
    }
}

fn map_effect_property_hint(hint: EffectPropertyHint) -> EffectProperties {
    let mut flags = EffectFlags::empty();
    if hint.discardable {
        flags = flags | EffectFlags::DISCARDABLE;
    }
    if hint.commutative {
        flags = flags | EffectFlags::COMMUTATIVE;
    }
    if hint.opaque_for_staging {
        flags = flags | EffectFlags::OPAQUE_FOR_STAGING;
    }
    if hint.ct_only {
        flags = flags | EffectFlags::CT_ONLY;
    }

    let level = match hint.capability {
        EffectCapabilityHint::Pure => CapabilityLevel::Pure,
        EffectCapabilityHint::Diverge => CapabilityLevel::Diverge,
        EffectCapabilityHint::Alloc => CapabilityLevel::Alloc,
        EffectCapabilityHint::LocalState => {
            flags = flags | EffectFlags::LOCAL_STATE;
            CapabilityLevel::LocalState
        }
        EffectCapabilityHint::SharedState => {
            flags = flags | EffectFlags::SHARED_STATE | EffectFlags::OPAQUE_FOR_STAGING;
            CapabilityLevel::SharedState
        }
        EffectCapabilityHint::Io => {
            flags = flags | EffectFlags::OPAQUE_FOR_STAGING;
            CapabilityLevel::Io
        }
        EffectCapabilityHint::Ffi => {
            flags = flags | EffectFlags::OPAQUE_FOR_STAGING;
            CapabilityLevel::Ffi
        }
    };

    EffectProperties::new(level, flags)
}

fn map_builtin_type(ty: BuiltinType) -> PrimitiveTypeRef {
    match ty {
        BuiltinType::Bool => PrimitiveTypeRef::Bool,
        BuiltinType::Int => PrimitiveTypeRef::Int,
        BuiltinType::Float => PrimitiveTypeRef::Float,
        BuiltinType::Char => PrimitiveTypeRef::Char,
        BuiltinType::String => PrimitiveTypeRef::String,
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
