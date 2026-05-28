use std::collections::{HashMap, HashSet};
use std::sync::Arc;

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
// - A statement-shaped operand binds to a `Val` in the statement list its own
//   result lands in, so hoisting never crosses a branch
// - Function declarations exist before body lowering (for call resolution)
//
// Diagnostics:
// - Unknown vars/functions/effects
//
// Complexity:
// - Linear in AST size (single walk + reverse statement stitching per block)

use cielo_base::diagnostics::{DiagnosticBag, ErrorNode};
use cielo_base::{EffectLabelId, FuncId, HandlerId, Interner, Span, SymbolId, VarId};
use cielo_frontend::ast::{
    self, BuiltinType, EffectCapabilityHint, EffectPropertyHint, ExprKind as AstExprKind, Item,
    Stmt as AstStmt, TypeExpr, TypeExprKind,
};
use cielo_ir::builtins::BuiltinSymbols;
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
    pub builtins: BuiltinSymbols,
    /// Diagnostic text only. Without it a symbol is printed as its id, which is
    /// how the later passes render names they cannot resolve.
    pub names: Option<Arc<Interner>>,
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
            builtins: BuiltinSymbols::default(),
            names: None,
        }
    }

    pub fn with_builtins(mut self, builtins: BuiltinSymbols) -> Self {
        self.builtins = builtins;
        self
    }

    pub fn with_names(mut self, names: Arc<Interner>) -> Self {
        self.names = Some(names);
        self
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
    effect_ops: HashMap<(EffectLabelId, SymbolId), Vec<CoreTypeRef>>,
    struct_ctors: HashMap<SymbolId, Vec<CoreTypeRef>>,
    /// Variant name -> every enum declaring it. Two enums may share a variant
    /// name, so an unqualified use is resolved against the expected type.
    enum_ctors: HashMap<SymbolId, Vec<EnumCtor>>,
    handler_decls: HashMap<SymbolId, ast::HandlerDecl>,
    active_resume_vars: HashSet<VarId>,
    /// Type parameters of the function whose body is being lowered, so a `let`
    /// annotation naming one lowers to `Param` rather than `Named`.
    type_params: Vec<SymbolId>,
    /// Name symbol of the function whose body is being lowered. A lifted
    /// closure body borrows it: codegen keys function names on the dense id,
    /// and handler specialization already clones bodies that share a symbol.
    enclosing_fn_name: SymbolId,
    config: LowerConfig,
}

enum LoweredValue {
    Expr(cielo_base::ExprId),
    Stmt(cielo_base::StmtId),
}

/// One entry of a statement list under construction. A list is built forwards
/// and stitched backwards by `stitch`, because each Core statement names its
/// successor.
///
/// Statement lists are also where operand hoisting lands: `lower_expr` takes
/// the list its result will sit in, so a statement-shaped operand can bind
/// itself to a `Val` and leave a variable behind.
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

#[derive(Clone, Copy)]
enum ShortCircuit {
    And,
    Or,
}

impl ShortCircuit {
    const fn from_op(op: ast::BinOp) -> Option<Self> {
        match op {
            ast::BinOp::And => Some(Self::And),
            ast::BinOp::Or => Some(Self::Or),
            _ => None,
        }
    }

    /// The value the operator yields without consulting its right operand.
    const fn shortcut(self) -> bool {
        matches!(self, Self::Or)
    }
}

#[derive(Clone, Debug)]
struct EnumCtor {
    enum_name: SymbolId,
    fields: Vec<CoreTypeRef>,
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
            type_params: Vec::new(),
            enclosing_fn_name: SymbolId::INVALID,
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
                        let param_types = operation
                            .params
                            .iter()
                            .map(|param| lower_type_ref(&param.ty, &[]))
                            .collect::<Vec<_>>();
                        self.effect_ops
                            .insert((effect_id, operation.name), param_types.clone());
                        operations.push(EffectOperationDecl {
                            name: operation.name,
                            param_types,
                            return_type: operation
                                .return_type
                                .as_ref()
                                .map(|ty| lower_type_ref(ty, &[]))
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
                    let fields = decl
                        .fields
                        .iter()
                        .map(|field| lower_type_ref(&field.ty, &decl.type_params))
                        .collect::<Vec<_>>();
                    self.struct_ctors.insert(decl.name, fields.clone());
                    self.program.add_struct(AdtStructDecl {
                        name: decl.name,
                        type_params: decl.type_params.clone(),
                        fields,
                        field_names: decl.fields.iter().map(|field| field.name).collect(),
                        span: decl.span,
                    });
                }
                Item::Enum(decl) => {
                    let mut variants = Vec::with_capacity(decl.variants.len());
                    for variant in &decl.variants {
                        let fields = variant
                            .fields
                            .iter()
                            .map(|field| lower_type_ref(field, &decl.type_params))
                            .collect::<Vec<_>>();
                        self.enum_ctors
                            .entry(variant.name)
                            .or_default()
                            .push(EnumCtor {
                                enum_name: decl.name,
                                fields: fields.clone(),
                            });
                        variants.push(AdtEnumVariantDecl {
                            name: variant.name,
                            fields,
                            span: variant.span,
                        });
                    }
                    self.program.add_enum(AdtEnumDecl {
                        name: decl.name,
                        type_params: decl.type_params.clone(),
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
                        .map(|param| lower_type_ref(&param.ty, &function.type_params))
                        .collect(),
                    return_type: function
                        .return_type
                        .as_ref()
                        .map(|ty| lower_type_ref(ty, &function.type_params))
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

            self.type_params.clone_from(&function.type_params);
            self.enclosing_fn_name = function.name;
            let expected = function
                .return_type
                .as_ref()
                .and_then(|ty| expected_adt(&lower_type_ref(ty, &function.type_params)));
            let body = self.lower_block(&function.body, &mut locals, expected);
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

    /// `expected` is the ADT the block's tail must produce, when the enclosing
    /// syntax fixes it; it only ever picks between same-named enum variants.
    fn lower_block(
        &mut self,
        block: &ast::BlockExpr,
        outer_locals: &mut HashMap<SymbolId, VarId>,
        expected: Option<SymbolId>,
    ) -> cielo_base::StmtId {
        let mut locals = outer_locals.clone();
        let mut actions: Vec<Action> = Vec::new();

        for stmt in &block.statements {
            match stmt {
                AstStmt::Let {
                    name,
                    ty,
                    value,
                    span,
                } => {
                    let binding = self.fresh_var();
                    let declared = ty.as_ref().map(|ty| lower_type_ref(ty, &self.type_params));
                    let lowered = self.lower_binding_value(
                        value,
                        &locals,
                        declared.as_ref().and_then(expected_adt),
                        &mut actions,
                    );
                    if let Some(declared) = declared {
                        self.program.set_declared_var_type(binding, declared);
                    }
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
                    match self.lower_binding_value(value, &locals, None, &mut actions) {
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
                    // A `Perform` over an unresolved effect would trip the
                    // pre-staging effect assertion before this diagnostic is
                    // read, so the statement degrades to an error binding.
                    let Some(effect_label) = self.effect_labels.get(effect).copied() else {
                        let error = self.diagnostics.error_node(
                            "LOWER_UNKNOWN_EFFECT",
                            "Unknown effect in `do` statement during AST->Core lowering",
                            *span,
                        );
                        for arg in args {
                            let _ = self.lower_expr(arg, &locals, None, &mut actions);
                        }
                        let value = self.push_expr(ExprKind::Error(error), *span);
                        let binding = match *binding {
                            Some(name) => {
                                let var = self.fresh_var();
                                locals.insert(name, var);
                                var
                            }
                            None => self.fresh_var(),
                        };
                        actions.push(Action::Let {
                            span: *span,
                            binding,
                            value,
                        });
                        continue;
                    };

                    let param_types = self.effect_ops.get(&(effect_label, *operation)).cloned();
                    match &param_types {
                        Some(expected) if expected.len() != args.len() => {
                            self.diagnostics.error(
                                "LOWER_BAD_EFFECT_OP_ARITY",
                                format!(
                                    "Effect operation argument count mismatch: expected {}, got {}",
                                    expected.len(),
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

                    let expectations = param_expectations(param_types.as_deref());
                    let lowered_args = args
                        .iter()
                        .enumerate()
                        .map(|(index, arg)| {
                            self.lower_expr(
                                arg,
                                &locals,
                                expectations.get(index).copied().flatten(),
                                &mut actions,
                            )
                        })
                        .collect::<Vec<_>>();
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

        // The tail hoists into the same list as the statements above it: the
        // list is stitched positionally, so appending here still places the
        // hoisted bindings ahead of the tail they feed.
        let next = if let Some(tail) = &block.tail {
            match self.lower_binding_value(tail, &locals, expected, &mut actions) {
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

        self.stitch(actions, next)
    }

    /// Threads `next` back through `actions`, so the first action runs first.
    fn stitch(&mut self, actions: Vec<Action>, next: cielo_base::StmtId) -> cielo_base::StmtId {
        let mut next = next;
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

    /// Binds a statement-shaped operand to a fresh variable in `hoist` and
    /// yields that variable, which is the only thing a pure `ExprKind` can hold.
    ///
    /// `hoist` always belongs to the statement list the operand's own result
    /// lands in, never an enclosing one. That is what keeps a hoist from
    /// crossing a branch: an `if`/`match` arm is lowered as its own list, so an
    /// effectful operand inside an arm binds inside that arm and only runs when
    /// the arm does.
    fn hoist_stmt(
        &mut self,
        value: cielo_base::StmtId,
        hoist: &mut Vec<Action>,
        span: Span,
    ) -> cielo_base::ExprId {
        let binding = self.fresh_var();
        hoist.push(Action::Val {
            span,
            binding,
            value,
        });
        self.push_expr(ExprKind::Var(binding), span)
    }

    fn lower_binding_value(
        &mut self,
        value: &ast::Expr,
        locals: &HashMap<SymbolId, VarId>,
        expected: Option<SymbolId>,
        hoist: &mut Vec<Action>,
    ) -> LoweredValue {
        if let Some(stmt) = self.lower_effectful_expr(value, locals, expected) {
            LoweredValue::Stmt(stmt)
        } else {
            LoweredValue::Expr(self.lower_expr(value, locals, expected, hoist))
        }
    }

    fn lower_effectful_expr(
        &mut self,
        expr: &ast::Expr,
        locals: &HashMap<SymbolId, VarId>,
        expected: Option<SymbolId>,
    ) -> Option<cielo_base::StmtId> {
        if let AstExprKind::StageBlock { stage, block } = &expr.kind {
            let mut block_locals = locals.clone();
            let body = self.lower_block(block, &mut block_locals, expected);
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
            // The condition runs unconditionally, so its hoists sit outside the
            // branch. Each arm is a block of its own and hoists into itself.
            let mut hoisted = Vec::new();
            let cond = self.lower_expr(cond, locals, None, &mut hoisted);
            let mut then_locals = locals.clone();
            let then_branch = self.lower_block(then_branch, &mut then_locals, expected);
            let else_branch = if let Some(else_block) = else_branch {
                let mut else_locals = locals.clone();
                self.lower_block(else_block, &mut else_locals, expected)
            } else {
                let unit = self.push_expr(ExprKind::Literal(Literal::Unit), expr.span);
                self.push_stmt(StmtKind::Return(unit), expr.span)
            };
            let branch = self.push_stmt(
                StmtKind::If {
                    cond,
                    then_branch,
                    else_branch,
                },
                expr.span,
            );
            return Some(self.stitch(hoisted, branch));
        }

        if let AstExprKind::Binary { op, lhs, rhs } = &expr.kind
            && let Some(op) = ShortCircuit::from_op(*op)
        {
            return Some(self.lower_short_circuit(op, lhs, rhs, locals, expr.span));
        }

        if let AstExprKind::Match {
            scrutinee,
            clauses,
            default,
        } = &expr.kind
        {
            // As for `If`: the scrutinee is unconditional, each arm is its own
            // block.
            let mut hoisted = Vec::new();
            let scrutinee = self.lower_expr(scrutinee, locals, None, &mut hoisted);
            let mut arms = Vec::with_capacity(clauses.len());
            for clause in clauses {
                let mut clause_locals = locals.clone();
                let mut binders = Vec::with_capacity(clause.binders.len());
                for binder in &clause.binders {
                    let var = self.fresh_var();
                    clause_locals.insert(*binder, var);
                    binders.push(var);
                }
                let body = self.lower_block(&clause.body, &mut clause_locals, expected);
                arms.push(MatchArm {
                    tag: clause.tag,
                    binders,
                    body,
                    span: clause.span,
                });
            }
            let default = default.as_ref().map(|block| {
                let mut default_locals = locals.clone();
                self.lower_block(block, &mut default_locals, expected)
            });
            let matched = self.push_stmt(
                StmtKind::Match {
                    scrutinee,
                    arms,
                    default,
                },
                expr.span,
            );
            return Some(self.stitch(hoisted, matched));
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
                let mut hoisted = Vec::new();
                let arg = if args.len() == 1 {
                    self.lower_expr(&args[0], locals, None, &mut hoisted)
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
                let resumed = self.push_stmt(
                    StmtKind::Resume {
                        result,
                        resume: resume_var,
                        arg,
                        next: return_stmt,
                    },
                    expr.span,
                );
                return Some(self.stitch(hoisted, resumed));
            }

            if let Some(&func_id) = self.functions_by_name.get(&symbol) {
                let effects = self
                    .program
                    .function(func_id)
                    .map(|f| f.declared_effects.clone())
                    .filter(|row| !row.is_empty());
                let effects = effects?;
                let result = self.fresh_var();
                let mut hoisted = Vec::new();
                let arg_ids = self.lower_call_args(func_id, args, locals, &mut hoisted);
                let return_expr = self.push_expr(ExprKind::Var(result), expr.span);
                let return_stmt = self.push_stmt(StmtKind::Return(return_expr), expr.span);
                let called = self.push_stmt(
                    StmtKind::Call {
                        result,
                        callee: func_id,
                        args: arg_ids,
                        effects,
                        next: return_stmt,
                    },
                    expr.span,
                );
                return Some(self.stitch(hoisted, called));
            }
        }

        let AstExprKind::Handle { body, handler } = &expr.kind else {
            return None;
        };

        let handler_id = match handler {
            ast::HandlerRef::Inline { effect, clauses } => self
                .resolve_handler_effect(*effect, expr.span)
                .map(|label| self.lower_handler_def(label, clauses, locals, expr.span)),
            ast::HandlerRef::Named(name) => match self.handler_decls.get(name).cloned() {
                // Module scope binds nothing, so clauses lower under empty locals.
                Some(decl) => self
                    .resolve_handler_effect(decl.effect, decl.span)
                    .map(|label| {
                        self.lower_handler_def(label, &decl.clauses, &HashMap::new(), decl.span)
                    }),
                None => {
                    self.diagnostics.error(
                        "LOWER_UNKNOWN_HANDLER",
                        "Unknown handler name in `handle` expression",
                        expr.span,
                    );
                    None
                }
            },
        };

        // Emitting a handler over an unresolved effect would trip the
        // pre-staging effect assertion before these diagnostics are read.
        let Some(handler_id) = handler_id else {
            return Some(self.lower_expr_as_body(body, locals, expected));
        };

        let body_stmt = self.lower_expr_as_body(body, locals, expected);
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

    fn lower_expr_as_body(
        &mut self,
        body: &ast::Expr,
        locals: &HashMap<SymbolId, VarId>,
        expected: Option<SymbolId>,
    ) -> cielo_base::StmtId {
        if let Some(stmt) = self.lower_effectful_expr(body, locals, expected) {
            return stmt;
        }
        match &body.kind {
            AstExprKind::Block(block) => {
                let mut block_locals = locals.clone();
                self.lower_block(block, &mut block_locals, expected)
            }
            _ => {
                let mut hoisted = Vec::new();
                let body_expr = self.lower_expr(body, locals, expected, &mut hoisted);
                let returned = self.push_stmt(StmtKind::Return(body_expr), body.span);
                self.stitch(hoisted, returned)
            }
        }
    }

    /// `a && b` becomes `if a { b } else { false }` and `a || b` becomes
    /// `if a { true } else { b }`, so the right operand only runs when the left
    /// does not already decide the answer.
    ///
    /// Core has no conditional expression, so the result is a statement. Every
    /// position reaches it: statement lists take it directly, and an operand
    /// hoists it into the list its own result lands in, so `BinaryOp::And`/`Or`
    /// no longer survive lowering.
    fn lower_short_circuit(
        &mut self,
        op: ShortCircuit,
        lhs: &ast::Expr,
        rhs: &ast::Expr,
        locals: &HashMap<SymbolId, VarId>,
        span: Span,
    ) -> cielo_base::StmtId {
        let mut hoisted = Vec::new();
        let cond = self.lower_expr(lhs, locals, None, &mut hoisted);
        // The right operand only runs on one side, so it is lowered as a branch
        // body and hoists into that body rather than into `hoisted`.
        let rhs_branch = self.lower_expr_as_body(rhs, locals, None);
        let shortcut = self.push_expr(ExprKind::Literal(Literal::Bool(op.shortcut())), span);
        let shortcut_branch = self.push_stmt(StmtKind::Return(shortcut), span);
        let (then_branch, else_branch) = match op {
            ShortCircuit::And => (rhs_branch, shortcut_branch),
            ShortCircuit::Or => (shortcut_branch, rhs_branch),
        };
        let branch = self.push_stmt(
            StmtKind::If {
                cond,
                then_branch,
                else_branch,
            },
            span,
        );
        self.stitch(hoisted, branch)
    }

    fn resolve_handler_effect(&mut self, effect: SymbolId, span: Span) -> Option<EffectLabelId> {
        let label = self.effect_labels.get(&effect).copied();
        if label.is_none() {
            self.diagnostics.error(
                "LOWER_UNKNOWN_HANDLER_EFFECT",
                "Unknown effect in `handle` expression during AST->Core lowering",
                span,
            );
        }
        label
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

            match self.effect_ops.get(&(effect_label, clause.operation)) {
                Some(params) => {
                    let expected = params.len();
                    if clause_param_symbols.len() == expected + 1 {
                        resume_symbol = clause_param_symbols.pop();
                    } else if clause_param_symbols.len() != expected {
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
                    lowerer.lower_block(&clause.body, &mut clause_locals, None)
                })
            } else {
                self.lower_block(&clause.body, &mut clause_locals, None)
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

    /// Lowers to a pure Core expression, pushing anything statement-shaped it
    /// meets onto `hoist` as a `Val` and reading the result back as a variable.
    ///
    /// Operands are lowered left to right and `hoist` is appended in that order,
    /// so that is the order performs run in. It is observable now that an
    /// operand can perform, so it is fixed here rather than left to fall out of
    /// field-initializer order.
    fn lower_expr(
        &mut self,
        expr: &ast::Expr,
        locals: &HashMap<SymbolId, VarId>,
        expected: Option<SymbolId>,
        hoist: &mut Vec<Action>,
    ) -> cielo_base::ExprId {
        let kind = match &expr.kind {
            AstExprKind::Field { base, field } => ExprKind::Field {
                base: self.lower_expr(base, locals, None, hoist),
                field: *field,
            },
            AstExprKind::Int(value) => ExprKind::Literal(Literal::Int(*value)),
            AstExprKind::Float(value) => ExprKind::Literal(Literal::Float(*value)),
            AstExprKind::Bool(value) => ExprKind::Literal(Literal::Bool(*value)),
            AstExprKind::Char(value) => ExprKind::Literal(Literal::Char(*value)),
            AstExprKind::String(value) => ExprKind::Literal(Literal::String(value.clone())),
            // Locals win over constructors, so a binding may shadow a variant name.
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
                } else if self.enum_ctors.contains_key(name) {
                    match self.resolve_enum_ctor(*name, expected) {
                        Some(ctor) if ctor.fields.is_empty() => ExprKind::MakeEnum {
                            ty: ctor.enum_name,
                            variant: *name,
                            fields: Vec::new(),
                        },
                        Some(ctor) => {
                            let error = self.diagnostics.error_node(
                                "LOWER_BAD_ENUM_CTOR_ARITY",
                                format!(
                                    "Enum constructor argument count mismatch: expected {}, got 0",
                                    ctor.fields.len()
                                ),
                                expr.span,
                            );
                            ExprKind::Error(error)
                        }
                        None => ExprKind::Error(self.ambiguous_ctor_error(*name, expr.span)),
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
                expr: self.lower_expr(inner, locals, None, hoist),
            },
            AstExprKind::Binary { op, lhs, rhs } => {
                if let Some(op) = ShortCircuit::from_op(*op) {
                    let value = self.lower_short_circuit(op, lhs, rhs, locals, expr.span);
                    return self.hoist_stmt(value, hoist, expr.span);
                }
                let lhs = self.lower_expr(lhs, locals, None, hoist);
                let rhs = self.lower_expr(rhs, locals, None, hoist);
                ExprKind::Binary {
                    op: map_binary_op(*op),
                    lhs,
                    rhs,
                }
            }
            AstExprKind::Call { callee, args } => {
                // `resume(..)` and a call to an effectful function are both
                // statements in Core; hoisting them is what lets them appear
                // under an operator or as another call's argument.
                if let Some(value) = self.lower_effectful_expr(expr, locals, expected) {
                    return self.hoist_stmt(value, hoist, expr.span);
                }
                if let AstExprKind::Var(symbol) = callee.kind {
                    if let Some(var) = locals.get(&symbol).copied() {
                        // A local wins over a function of the same name here for
                        // the same reason it does in `Var` position.
                        let callee = self.push_expr(ExprKind::Var(var), callee.span);
                        ExprKind::CallClosure {
                            callee,
                            args: args
                                .iter()
                                .map(|arg| self.lower_expr(arg, locals, None, hoist))
                                .collect(),
                        }
                    } else if let Some(&func_id) = self.functions_by_name.get(&symbol) {
                        // Only a pure callee reaches here: an effectful one was
                        // hoisted above.
                        ExprKind::PureCall {
                            callee: func_id,
                            args: self.lower_call_args(func_id, args, locals, hoist),
                        }
                    } else if self.enum_ctors.contains_key(&symbol) {
                        match self.resolve_enum_ctor(symbol, expected) {
                            Some(ctor) => {
                                if ctor.fields.len() != args.len() {
                                    self.diagnostics.error(
                                        "LOWER_BAD_ENUM_CTOR_ARITY",
                                        format!(
                                            "Enum constructor argument count mismatch: expected {}, got {}",
                                            ctor.fields.len(),
                                            args.len()
                                        ),
                                        expr.span,
                                    );
                                }
                                ExprKind::MakeEnum {
                                    ty: ctor.enum_name,
                                    variant: symbol,
                                    fields: self.lower_field_args(
                                        &ctor.fields,
                                        args,
                                        locals,
                                        hoist,
                                    ),
                                }
                            }
                            None => ExprKind::Error(self.ambiguous_ctor_error(symbol, expr.span)),
                        }
                    } else if let Some(field_types) = self.struct_ctors.get(&symbol).cloned() {
                        if field_types.len() != args.len() {
                            self.diagnostics.error(
                                "LOWER_BAD_STRUCT_CTOR_ARITY",
                                format!(
                                    "Struct constructor argument count mismatch: expected {}, got {}",
                                    field_types.len(),
                                    args.len()
                                ),
                                expr.span,
                            );
                        }
                        ExprKind::MakeStruct {
                            ty: symbol,
                            fields: self.lower_field_args(&field_types, args, locals, hoist),
                        }
                    } else if let Some(target_builtin) =
                        self.lower_target_builtin_call(symbol, args, locals, expr.span, hoist)
                    {
                        target_builtin
                    } else if let Some(builtin) = self.config.builtins.lookup(symbol) {
                        // Checked after user functions, so a source-level
                        // definition of the same name still wins.
                        ExprKind::BuiltinCall {
                            builtin,
                            args: args
                                .iter()
                                .map(|arg| self.lower_expr(arg, locals, None, hoist))
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
                    let callee = self.lower_expr(callee, locals, None, hoist);
                    ExprKind::CallClosure {
                        callee,
                        args: args
                            .iter()
                            .map(|arg| self.lower_expr(arg, locals, None, hoist))
                            .collect(),
                    }
                }
            }
            AstExprKind::Lambda { params, body } => {
                self.lower_lambda(params, body, locals, expr.span)
            }
            AstExprKind::If { .. }
            | AstExprKind::Match { .. }
            | AstExprKind::Block(_)
            | AstExprKind::StageBlock { .. }
            | AstExprKind::Handle { .. } => {
                let value = self.lower_expr_as_body(expr, locals, expected);
                return self.hoist_stmt(value, hoist, expr.span);
            }
            AstExprKind::Error(error) => ExprKind::Error(error.clone()),
        };
        self.push_expr(kind, expr.span)
    }

    /// Lifts `|params| body` into a top-level function and yields the closure
    /// value naming it.
    ///
    /// Captures are the identifiers the body mentions that resolve to an
    /// enclosing binding. Deliberately over-approximated: a name the body
    /// rebinds before its first use is captured and then unused, which costs
    /// one retain, whereas under-approximating would leave the lifted body
    /// reading a variable nothing binds.
    fn lower_lambda(
        &mut self,
        params: &[SymbolId],
        body: &ast::BlockExpr,
        locals: &HashMap<SymbolId, VarId>,
        span: Span,
    ) -> ExprKind {
        let mut mentioned = HashSet::new();
        collect_block_symbols(body, &mut mentioned);
        let mut captured = mentioned
            .into_iter()
            .filter(|symbol| !params.contains(symbol) && locals.contains_key(symbol))
            .collect::<Vec<_>>();
        captured.sort_unstable_by_key(|symbol| symbol.as_u32());
        // A closure is exactly the construct that would make `resume` a value,
        // which is what the no-runtime-control-operator result rests on.
        captured.retain(|symbol| {
            let escapes = locals
                .get(symbol)
                .is_some_and(|var| self.active_resume_vars.contains(var));
            if escapes {
                self.diagnostics.error(
                    "LOWER_RESUME_VALUE_ESCAPE",
                    "`resume` cannot be captured or passed as a value in v1",
                    span,
                );
            }
            !escapes
        });

        let mut body_locals = HashMap::new();
        let mut lifted_params = Vec::with_capacity(captured.len() + params.len());
        let mut capture_args = Vec::with_capacity(captured.len());
        for symbol in &captured {
            let var = self.fresh_var();
            body_locals.insert(*symbol, var);
            lifted_params.push(var);
            let outer = locals[symbol];
            capture_args.push(self.push_expr(ExprKind::Var(outer), span));
        }
        for symbol in params {
            let var = self.fresh_var();
            body_locals.insert(*symbol, var);
            lifted_params.push(var);
        }

        let param_count = lifted_params.len();
        let placeholder = self.make_dummy_body(span);
        let func = self.program.add_function(FunctionDecl {
            name: self.enclosing_fn_name,
            params: lifted_params,
            // Nothing writes a lambda's parameter or result types. Typecheck
            // infers them; ownership classifies a lifted body's parameters
            // managed, which is the direction that cannot corrupt memory.
            param_types: vec![CoreTypeRef::Unknown; param_count],
            return_type: CoreTypeRef::Unknown,
            declared_effects: SortedEffectRow::new(Vec::new()),
            body: placeholder,
            ct_only: false,
            span,
        });
        let lowered_body = self.lower_block(body, &mut body_locals, None);
        if let Some(decl) = self.program.function_mut(func) {
            decl.body = lowered_body;
        }
        if self.stmt_performs_effect(lowered_body) {
            let error = self.diagnostics.error_node(
                "LOWER_EFFECTFUL_CLOSURE",
                "A closure body may not perform an effect or call an effectful function: a written `with` row on a function type is checked, not inferred from a lambda",
                span,
            );
            return ExprKind::Error(error);
        }

        ExprKind::MakeClosure {
            func,
            captures: capture_args,
        }
    }

    /// True when the statement graph reaches an operation with effects.
    ///
    /// A function *type* can now carry a row, but nothing derives one from a
    /// lambda body, and `CallClosure` is an expression, whose effects are
    /// always empty under the Expr/Stmt split — so a perform inside a lifted
    /// body would never reach the caller's row. A handler *inside* the body
    /// does not help either: the perform below it is what is rejected.
    fn stmt_performs_effect(&self, root: cielo_base::StmtId) -> bool {
        let mut seen = HashSet::new();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            let Some(stmt) = self.program.stmt(id) else {
                continue;
            };
            match &stmt.kind {
                StmtKind::Perform { .. } | StmtKind::Resume { .. } => return true,
                StmtKind::Call { effects, .. } if !effects.is_empty() => return true,
                _ => {}
            }
            stack.extend(stmt.child_stmts());
        }
        false
    }

    /// Picks the enum an unqualified variant name belongs to. A name declared
    /// by a single enum needs no context; otherwise only the expected type can
    /// decide, and `None` means the caller must report the ambiguity.
    fn resolve_enum_ctor(&self, variant: SymbolId, expected: Option<SymbolId>) -> Option<EnumCtor> {
        let candidates = self.enum_ctors.get(&variant)?;
        let first = candidates.first()?;
        if candidates
            .iter()
            .all(|ctor| ctor.enum_name == first.enum_name)
        {
            return Some(first.clone());
        }
        let expected = expected?;
        candidates
            .iter()
            .find(|ctor| ctor.enum_name == expected)
            .cloned()
    }

    fn ambiguous_ctor_error(&mut self, variant: SymbolId, span: Span) -> ErrorNode {
        let owners = self
            .enum_ctors
            .get(&variant)
            .map(|candidates| {
                candidates
                    .iter()
                    .map(|ctor| ctor.enum_name)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let names = self.describe_symbols(&owners);
        self.diagnostics.error_node(
            "LOWER_AMBIGUOUS_ENUM_CTOR",
            format!(
                "Enum constructor is declared by more than one enum ({names}); annotate the expected type"
            ),
            span,
        )
    }

    fn describe_symbols(&self, symbols: &[SymbolId]) -> String {
        symbols
            .iter()
            .map(|symbol| {
                match self
                    .config
                    .names
                    .as_deref()
                    .and_then(|names| names.resolve(*symbol))
                {
                    Some(text) => text.to_owned(),
                    None => format!("#{}", symbol.as_u32()),
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn lower_call_args(
        &mut self,
        callee: FuncId,
        args: &[ast::Expr],
        locals: &HashMap<SymbolId, VarId>,
        hoist: &mut Vec<Action>,
    ) -> Vec<cielo_base::ExprId> {
        let param_types = self
            .program
            .function(callee)
            .map(|function| function.param_types.clone())
            .unwrap_or_default();
        self.lower_field_args(&param_types, args, locals, hoist)
    }

    fn lower_field_args(
        &mut self,
        declared: &[CoreTypeRef],
        args: &[ast::Expr],
        locals: &HashMap<SymbolId, VarId>,
        hoist: &mut Vec<Action>,
    ) -> Vec<cielo_base::ExprId> {
        args.iter()
            .enumerate()
            .map(|(index, arg)| {
                let expected = declared.get(index).and_then(expected_adt);
                self.lower_expr(arg, locals, expected, hoist)
            })
            .collect()
    }

    fn lower_target_builtin_call(
        &mut self,
        callee: SymbolId,
        args: &[ast::Expr],
        locals: &HashMap<SymbolId, VarId>,
        span: Span,
        hoist: &mut Vec<Action>,
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
            let _ = self.lower_expr(arg, locals, None, hoist);
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

/// Every identifier a block mentions, binders included. Callers intersect the
/// result with an enclosing scope, so a name bound only inside drops out.
fn collect_block_symbols(block: &ast::BlockExpr, out: &mut HashSet<SymbolId>) {
    for stmt in &block.statements {
        match stmt {
            AstStmt::Let { value, .. } => collect_expr_symbols(value, out),
            AstStmt::Expr { value, .. } => collect_expr_symbols(value, out),
            AstStmt::Perform { args, .. } => {
                for arg in args {
                    collect_expr_symbols(arg, out);
                }
            }
            AstStmt::Error(_) => {}
        }
    }
    if let Some(tail) = &block.tail {
        collect_expr_symbols(tail, out);
    }
}

fn collect_expr_symbols(expr: &ast::Expr, out: &mut HashSet<SymbolId>) {
    match &expr.kind {
        AstExprKind::Var(name) => {
            out.insert(*name);
        }
        AstExprKind::Int(_)
        | AstExprKind::Float(_)
        | AstExprKind::Bool(_)
        | AstExprKind::Char(_)
        | AstExprKind::String(_) => {}
        AstExprKind::Call { callee, args } => {
            collect_expr_symbols(callee, out);
            for arg in args {
                collect_expr_symbols(arg, out);
            }
        }
        AstExprKind::Field { base, .. } => collect_expr_symbols(base, out),
        AstExprKind::Binary { lhs, rhs, .. } => {
            collect_expr_symbols(lhs, out);
            collect_expr_symbols(rhs, out);
        }
        AstExprKind::Unary { expr, .. } => collect_expr_symbols(expr, out),
        AstExprKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            collect_expr_symbols(cond, out);
            collect_block_symbols(then_branch, out);
            if let Some(block) = else_branch {
                collect_block_symbols(block, out);
            }
        }
        AstExprKind::Match {
            scrutinee,
            clauses,
            default,
        } => {
            collect_expr_symbols(scrutinee, out);
            for clause in clauses {
                collect_block_symbols(&clause.body, out);
            }
            if let Some(block) = default {
                collect_block_symbols(block, out);
            }
        }
        AstExprKind::Block(block) => collect_block_symbols(block, out),
        AstExprKind::StageBlock { block, .. } => collect_block_symbols(block, out),
        AstExprKind::Handle { body, handler } => {
            collect_expr_symbols(body, out);
            if let ast::HandlerRef::Inline { clauses, .. } = handler {
                for clause in clauses {
                    collect_block_symbols(&clause.body, out);
                }
            }
        }
        // A nested lambda's own captures come from this body, so its free
        // names have to reach the outer capture list too.
        AstExprKind::Lambda { body, .. } => collect_block_symbols(body, out),
        AstExprKind::Error(_) => {}
    }
}

/// The ADT a declared type names, if any. A type parameter yields `None`: at
/// lowering time nothing pins it down.
fn expected_adt(ty: &CoreTypeRef) -> Option<SymbolId> {
    match ty {
        CoreTypeRef::Named(name) | CoreTypeRef::Applied { name, .. } => Some(*name),
        _ => None,
    }
}

fn param_expectations(params: Option<&[CoreTypeRef]>) -> Vec<Option<SymbolId>> {
    params
        .unwrap_or_default()
        .iter()
        .map(expected_adt)
        .collect()
}

/// `type_params` are the enclosing declaration's `[T]` names. A path that
/// matches one becomes `Param`; everything else stays `Named` for typecheck to
/// resolve or reject.
fn lower_type_ref(ty: &TypeExpr, type_params: &[SymbolId]) -> CoreTypeRef {
    match &ty.kind {
        TypeExprKind::Unit => CoreTypeRef::Unit,
        TypeExprKind::Builtin(builtin) => CoreTypeRef::Primitive(map_builtin_type(*builtin)),
        TypeExprKind::Path { name, args } if args.is_empty() => {
            if type_params.contains(name) {
                CoreTypeRef::Param(*name)
            } else {
                CoreTypeRef::Named(*name)
            }
        }
        TypeExprKind::Path { name, args } => CoreTypeRef::Applied {
            name: *name,
            args: args
                .iter()
                .map(|arg| lower_type_ref(arg, type_params))
                .collect(),
        },
        TypeExprKind::Func {
            params,
            ret,
            effects,
        } => CoreTypeRef::Func {
            params: params
                .iter()
                .map(|param| lower_type_ref(param, type_params))
                .collect(),
            ret: Box::new(lower_type_ref(ret, type_params)),
            effects: canonical_effect_row(effects),
        },
        TypeExprKind::Error(_) => CoreTypeRef::Unknown,
    }
}

/// Row order is not part of a type's identity, so it is normalized here rather
/// than left for every consumer to sort. Names are not resolved to labels:
/// `lower_type_ref` runs while effect declarations are still being registered.
fn canonical_effect_row(names: &[SymbolId]) -> Vec<SymbolId> {
    let mut row = names.to_vec();
    row.sort_unstable_by_key(|name| name.as_u32());
    row.dedup();
    row
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
