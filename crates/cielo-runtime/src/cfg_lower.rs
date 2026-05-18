//! Lower the structured Linear IR into an explicit block-form CFG.
//!
//! The lowering is continuation-aware. In particular, `Val`'s value graph runs
//! before its `next` graph, and handler/stage cleanup blocks run between their
//! bodies and continuations. Treating those nodes as ordinary sibling edges is
//! the source of the old last-use/ARC ordering bugs.
//!
//! Linear IR is not alpha-renamed: handler inlining re-lowers a Core subtree
//! once per resume site per perform, so one `VarId` is the binding of many
//! statements. Values are therefore keyed by scope rather than by `VarId`, and
//! the block memo carries that scope, so each re-lowering gets its own values
//! and its own blocks (CIELO-47).

use std::collections::HashMap;
use std::rc::Rc;

use cielo_base::ids::{
    CfgBlockId, CfgExprId, CfgFuncId, CfgHandlerId, CfgValueId, LinearExprId, LinearFuncId,
    LinearStmtId, ResumptionId, VarId,
};
use cielo_ir::cfg::{
    CfgCallConvention, CfgExpr, CfgFunction, CfgHandlerClause, CfgInstruction, CfgMatchArm,
    CfgProgram, CfgProjectionMode, CfgTerminator,
};
use cielo_ir::core::Literal;
use cielo_ir::linear::{LinearExpr, LinearHandlerClause, LinearProgram, LinearStmt};
use cielo_ir::region::{Placement, RegionOwner, RegionSlot, RegionSlotKind};

pub fn run(linear: &LinearProgram) -> CfgProgram {
    let mut cfg = lower_program(linear);
    // Placement lives here rather than in a memory strategy so that no strategy
    // can forget it: an unplaced region is silently pessimal, never wrong.
    crate::region::place(&mut cfg);
    debug_assert!(
        cfg.validate().is_ok(),
        "cfg_lower produced an invalid control-flow graph: {:?}",
        cfg.validate().err().unwrap_or_default()
    );
    cfg
}

pub fn lower_program(linear: &LinearProgram) -> CfgProgram {
    Lowerer::new(linear).lower()
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Exit {
    Return,
    Yield(CfgBlockId),
    Discard(CfgBlockId),
}

/// A binding chain, innermost first. Persistent so extending it in one branch
/// leaves every other branch's view untouched.
#[derive(Clone, Default)]
struct Scope(Option<Rc<Binding>>);

struct Binding {
    var: VarId,
    value: CfgValueId,
    outer: Scope,
}

impl Scope {
    fn bind(&self, var: VarId, value: CfgValueId) -> Self {
        Self(Some(Rc::new(Binding {
            var,
            value,
            outer: self.clone(),
        })))
    }

    fn get(&self, var: VarId) -> Option<CfgValueId> {
        let mut cursor = self;
        while let Some(binding) = &cursor.0 {
            if binding.var == var {
                return Some(binding.value);
            }
            cursor = &binding.outer;
        }
        None
    }

    /// The part of the chain a statement can observe. `vars` is sorted, so the
    /// result is a canonical key.
    fn restrict(&self, vars: &[VarId]) -> ScopeKey {
        vars.iter()
            .filter_map(|var| self.get(*var).map(|value| (*var, value)))
            .collect()
    }
}

type ScopeKey = Box<[(VarId, CfgValueId)]>;

/// A residual clause whose blocks are lowered but whose `CfgFuncId` is not
/// assigned yet: [`Lowerer::lower`] indexes functions by position, so nothing
/// may be appended to `cfg.functions` while it is still walking them.
struct PendingClause {
    handler: CfgHandlerId,
    operation: cielo_base::ids::SymbolId,
    params: Vec<CfgValueId>,
    entry: CfgBlockId,
}

/// The blocks a defunctionalised clause's `ResumeJump`s need, while its clause
/// body is being lowered.
///
/// `entry` and `resumed` exist before the clause is walked because every site
/// names them; `arms` can only be collected during that walk, since the code
/// after a site has to be lowered where it can still see the clause's own
/// bindings.
struct OpenResumption {
    entry: CfgBlockId,
    /// The continuation's value, bound at each site's `result`. One value for
    /// every site: it is a block parameter, which `check_single_assignment`
    /// exempts precisely because predecessors, not instructions, fill it.
    resumed: CfgValueId,
    arms: Vec<(u32, CfgBlockId)>,
}

struct Lowerer<'a> {
    linear: &'a LinearProgram,
    cfg: CfgProgram,
    /// Free variables of each linear statement, indexed by arena position.
    free_vars: Vec<Vec<VarId>>,
    /// Values for variables no enclosing binder introduced. Only malformed
    /// input reaches this; `validate` then reports the read as undefined.
    unbound: HashMap<VarId, CfgValueId>,
    /// Keyed by block because `lower_borrowed` binds producers in the block
    /// that reads them. A hit from another block would hand back a value that
    /// this block has no path to a definition of.
    expressions: HashMap<(LinearExprId, CfgBlockId), CfgExprId>,
    /// The scope is part of the key, restricted to what the statement reads:
    /// two lowerings share a block exactly when they agree on every value in
    /// it. Restricting matters — an unrelated binding still in scope would
    /// otherwise split blocks that must stay shared, and `If` would duplicate
    /// its whole continuation into both branches.
    statements: HashMap<(LinearStmtId, Exit, ScopeKey), CfgBlockId>,
    functions: HashMap<LinearFuncId, CfgFuncId>,
    unit: Option<CfgExprId>,
    next_handler: usize,
    pending_clauses: Vec<PendingClause>,
    /// Resumptions whose clause body is currently being lowered.
    open_resumptions: HashMap<ResumptionId, OpenResumption>,
}

impl<'a> Lowerer<'a> {
    fn new(linear: &'a LinearProgram) -> Self {
        Self {
            linear,
            cfg: CfgProgram::default(),
            free_vars: free_vars(linear),
            unbound: HashMap::new(),
            expressions: HashMap::new(),
            statements: HashMap::new(),
            functions: HashMap::new(),
            unit: None,
            next_handler: 0,
            pending_clauses: Vec::new(),
            open_resumptions: HashMap::new(),
        }
    }

    fn lower(mut self) -> CfgProgram {
        let functions = self.linear.functions.clone();
        // Assign ids before lowering any body, so a call to a function that has
        // not been lowered yet still resolves.
        for (index, function) in functions.iter().enumerate() {
            self.functions.insert(function.id, CfgFuncId::new(index));
        }
        for function in functions {
            // Statement sharing is preserved within a function. Keeping the
            // cache function-local also prevents accidental cross-function
            // block ownership if an input arena shares a root node.
            self.statements.clear();
            let mut scope = Scope::default();
            let mut params = Vec::with_capacity(function.params.len());
            for var in &function.params {
                let value = self.cfg.push_value(Some(*var));
                scope = scope.bind(*var, value);
                params.push(value);
            }
            let body = self.lower_stmt(function.body, Exit::Return, &scope);
            let entry = self.cfg.push_block(params.clone(), Some(function.body));
            self.cfg.set_terminator(
                entry,
                CfgTerminator::Goto {
                    target: body,
                    args: Vec::new(),
                },
            );

            let id = CfgFuncId::new(self.cfg.functions.len());
            debug_assert_eq!(self.functions.get(&function.id).copied(), Some(id));
            self.cfg.functions.push(CfgFunction {
                id,
                name: function.name,
                params,
                entry,
            });
        }

        self.emit_pending_clauses();
        self.cfg.entrypoints = self
            .linear
            .entrypoints
            .iter()
            .filter_map(|id| self.functions.get(id).copied())
            .collect();
        self.cfg
    }

    /// Appends one function per residual clause and records the handler's
    /// dispatch table. Grouped by handler so the emitted table is one array.
    fn emit_pending_clauses(&mut self) {
        let mut tables: Vec<(CfgHandlerId, Vec<CfgHandlerClause>)> = Vec::new();
        for pending in std::mem::take(&mut self.pending_clauses) {
            let id = CfgFuncId::new(self.cfg.functions.len());
            self.cfg.functions.push(CfgFunction {
                id,
                name: pending.operation,
                params: pending.params,
                entry: pending.entry,
            });
            let clause = CfgHandlerClause {
                operation: pending.operation,
                function: id,
            };
            match tables
                .iter_mut()
                .find(|(handler, _)| *handler == pending.handler)
            {
                Some((_, clauses)) => clauses.push(clause),
                None => tables.push((pending.handler, vec![clause])),
            }
        }
        for (handler, clauses) in tables {
            self.cfg.push_clause_table(handler, clauses);
        }
    }

    /// Lowers a residual clause into its own function body.
    ///
    /// The statement cache is function-local, so it is swapped out here for
    /// the same reason [`Lowerer::lower`] clears it per function: a block
    /// reused across the boundary would belong to the wrong function.
    ///
    /// The parameters are synthetic rather than the clause's own variables,
    /// which the ownership tables classify as borrowed views. The dispatcher
    /// hands over an owned reference, so it has to arrive in a value the ARC
    /// pass treats as managed and then bind through a `Let`, exactly as the
    /// inlining path binds a clause parameter at the perform site. Making the
    /// borrowed variable the parameter instead drops the reference silently.
    fn lower_clause(&mut self, handler: CfgHandlerId, clause: &LinearHandlerClause) {
        let outer = std::mem::take(&mut self.statements);
        let params = clause
            .params
            .iter()
            .map(|_| self.synthetic_value())
            .collect::<Vec<_>>();
        // A clause body is its own function, so it starts from an empty scope
        // carrying only its parameters -- never the enclosing body's bindings.
        let mut scope = Scope::default();
        let mut bound = Vec::with_capacity(clause.params.len());
        for var in &clause.params {
            let value = self.cfg.push_value(Some(*var));
            scope = scope.bind(*var, value);
            bound.push(value);
        }
        let body = self.lower_stmt(clause.body, Exit::Return, &scope);
        let entry = self.cfg.push_block(params.clone(), Some(clause.body));
        for (param, result) in params.iter().zip(bound) {
            let value = self.cfg.push_expr(CfgExpr::Value(*param), None);
            self.cfg.push_instruction(
                entry,
                CfgInstruction::Let { result, value },
                Some(clause.body),
            );
        }
        self.cfg.set_terminator(
            entry,
            CfgTerminator::Goto {
                target: body,
                args: Vec::new(),
            },
        );
        self.statements = outer;
        self.pending_clauses.push(PendingClause {
            handler,
            operation: clause.operation,
            params,
            entry,
        });
    }

    fn value_for_var(&mut self, scope: &Scope, var: VarId) -> CfgValueId {
        if let Some(value) = scope.get(var) {
            return value;
        }
        if let Some(value) = self.unbound.get(&var) {
            return *value;
        }
        let value = self.cfg.push_value(Some(var));
        self.unbound.insert(var, value);
        value
    }

    fn synthetic_value(&mut self) -> CfgValueId {
        self.cfg.push_value(None)
    }

    fn unit_expr(&mut self) -> CfgExprId {
        if let Some(unit) = self.unit {
            return unit;
        }
        let unit = self.cfg.push_expr(CfgExpr::Literal(Literal::Unit), None);
        self.unit = Some(unit);
        unit
    }

    /// Lowers `id` into `block` for a position that takes ownership of the
    /// result: an instruction that binds it, a constructor field, a call or
    /// builtin argument, a block argument, a return. Each of those has a
    /// consumer that eventually releases the reference, so the outermost node
    /// needs no name of its own.
    fn lower_owned(&mut self, block: CfgBlockId, id: LinearExprId, scope: &Scope) -> CfgExprId {
        if let Some(lowered) = self.expressions.get(&(id, block)) {
            return *lowered;
        }
        let kind = match self.linear.expr(id).map(|node| node.kind.clone()) {
            Some(LinearExpr::Var(var)) => CfgExpr::Value(self.value_for_var(scope, var)),
            Some(LinearExpr::Literal(literal)) => CfgExpr::Literal(literal),
            Some(LinearExpr::Unary { op, expr }) => CfgExpr::Unary {
                op,
                expr: self.lower_borrowed(block, expr, scope),
            },
            Some(LinearExpr::Binary { op, lhs, rhs }) => CfgExpr::Binary {
                op,
                lhs: self.lower_borrowed(block, lhs, scope),
                rhs: self.lower_borrowed(block, rhs, scope),
            },
            Some(LinearExpr::Field { base, index }) => CfgExpr::Field {
                base: self.lower_borrowed(block, base, scope),
                index,
            },
            Some(LinearExpr::PureCall {
                callee,
                callee_fn,
                args,
            }) => CfgExpr::PureCall {
                callee,
                callee_fn: self.cfg_func_id(callee_fn),
                args: args
                    .into_iter()
                    .map(|arg| self.lower_owned(block, arg, scope))
                    .collect(),
            },
            Some(LinearExpr::BuiltinCall { builtin, args }) => CfgExpr::BuiltinCall {
                builtin,
                args: args
                    .into_iter()
                    .map(|arg| self.lower_owned(block, arg, scope))
                    .collect(),
            },
            Some(LinearExpr::MakeStruct { ty, fields }) => CfgExpr::MakeStruct {
                ty,
                fields: fields
                    .into_iter()
                    .map(|field| self.lower_owned(block, field, scope))
                    .collect(),
            },
            Some(LinearExpr::MakeEnum {
                ty,
                variant,
                fields,
            }) => CfgExpr::MakeEnum {
                ty,
                variant,
                fields: fields
                    .into_iter()
                    .map(|field| self.lower_owned(block, field, scope))
                    .collect(),
            },
            Some(LinearExpr::MakeClosure {
                callee,
                callee_fn,
                captures,
            }) => CfgExpr::MakeClosure {
                callee,
                callee_fn: self.cfg_func_id(callee_fn),
                captures: captures
                    .into_iter()
                    .map(|capture| self.lower_owned(block, capture, scope))
                    .collect(),
            },
            // The callee is borrowed, so it is bound to a value of its own
            // first: the call may not release the closure it dispatches on.
            Some(LinearExpr::CallClosure { callee, args }) => CfgExpr::CallClosure {
                callee: self.lower_borrowed(block, callee, scope),
                args: args
                    .into_iter()
                    .map(|arg| self.lower_owned(block, arg, scope))
                    .collect(),
            },
            Some(LinearExpr::Error) | None => CfgExpr::Error,
        };
        let lowered = self.cfg.push_expr(kind, Some(id));
        self.expressions.insert((id, block), lowered);
        lowered
    }

    /// Lowers `id` into `block` for a position that only reads the result and
    /// releases nothing: a `Unary`, `Binary` or `Field` operand, or the selector
    /// of a branching terminator. A producer there hands back a reference that
    /// no `CfgValueId` names, and ARC keys every op on a value, so it is bound
    /// to a fresh one first.
    ///
    /// Operands are lowered before their parent, so the bindings land innermost
    /// first and each one only mentions values already bound above it.
    fn lower_borrowed(&mut self, block: CfgBlockId, id: LinearExprId, scope: &Scope) -> CfgExprId {
        let lowered = self.lower_owned(block, id, scope);
        let Some(node) = self.cfg.expr(lowered) else {
            return lowered;
        };
        if !node.kind.produces_owned() {
            return lowered;
        }
        let source = node.source;
        let result = self.synthetic_value();
        let stmt = self.cfg.block(block).and_then(|block| block.source);
        self.cfg.push_instruction(
            block,
            CfgInstruction::Let {
                result,
                value: lowered,
            },
            stmt,
        );
        self.cfg.push_expr(CfgExpr::Value(result), source)
    }

    fn lower_stmt(&mut self, id: LinearStmtId, exit: Exit, scope: &Scope) -> CfgBlockId {
        let key = (id, exit, scope.restrict(self.free_vars_of(id)));
        if let Some(lowered) = self.statements.get(&key) {
            return *lowered;
        }

        let kind = self
            .linear
            .stmt(id)
            .map(|node| node.kind.clone())
            .unwrap_or(LinearStmt::Error);
        let entry = match kind {
            LinearStmt::Return(value) => {
                let block = self.cfg.push_block(Vec::new(), Some(id));
                let value = self.lower_owned(block, value, scope);
                self.set_exit(block, value, exit);
                block
            }
            LinearStmt::Let {
                binding,
                value,
                next,
            } => {
                let result = self.cfg.push_value(Some(binding));
                let next = self.lower_stmt(next, exit, &scope.bind(binding, result));
                let block = self.cfg.push_block(Vec::new(), Some(id));
                let value = self.lower_owned(block, value, scope);
                self.cfg
                    .push_instruction(block, CfgInstruction::Let { result, value }, Some(id));
                self.cfg.set_terminator(
                    block,
                    CfgTerminator::Goto {
                        target: next,
                        args: Vec::new(),
                    },
                );
                block
            }
            LinearStmt::Val {
                binding,
                value,
                next,
            } => {
                let result = self.cfg.push_value(Some(binding));
                let next = self.lower_stmt(next, exit, &scope.bind(binding, result));
                let continuation = self.cfg.push_block(vec![result], Some(id));
                self.cfg.set_terminator(
                    continuation,
                    CfgTerminator::Goto {
                        target: next,
                        args: Vec::new(),
                    },
                );
                self.lower_stmt(value, Exit::Yield(continuation), scope)
            }
            LinearStmt::PureCall {
                result,
                callee,
                callee_fn,
                args,
                next,
            } => self.lower_call(
                id,
                result,
                callee,
                callee_fn,
                args,
                next,
                exit,
                scope,
                CfgCallConvention::Pure,
            ),
            LinearStmt::DirectCall {
                result,
                callee,
                callee_fn,
                args,
                next,
            } => self.lower_call(
                id,
                result,
                callee,
                callee_fn,
                args,
                next,
                exit,
                scope,
                CfgCallConvention::Direct,
            ),
            LinearStmt::ControlCall {
                result,
                callee,
                callee_fn,
                args,
                next,
            } => self.lower_call(
                id,
                result,
                callee,
                callee_fn,
                args,
                next,
                exit,
                scope,
                CfgCallConvention::Control,
            ),
            LinearStmt::If {
                cond,
                then_branch,
                else_branch,
            } => {
                let then_target = self.lower_stmt(then_branch, exit, scope);
                let else_target = self.lower_stmt(else_branch, exit, scope);
                let block = self.cfg.push_block(Vec::new(), Some(id));
                let cond = self.lower_borrowed(block, cond, scope);
                self.cfg.set_terminator(
                    block,
                    CfgTerminator::Branch {
                        cond,
                        then_target,
                        else_target,
                    },
                );
                block
            }
            LinearStmt::Match {
                scrutinee,
                arms,
                default,
            } => {
                let mut lowered_arms = Vec::with_capacity(arms.len());
                for arm in arms {
                    let mut arm_scope = scope.clone();
                    let mut binders = Vec::with_capacity(arm.binders.len());
                    for binder in arm.binders {
                        let value = self.cfg.push_value(Some(binder));
                        arm_scope = arm_scope.bind(binder, value);
                        binders.push(value);
                    }
                    let target = self.lower_stmt(arm.body, exit, &arm_scope);
                    // Codegen assigns these by name at the match site, so the
                    // wrapper's parameters have to be the arm's binders
                    // themselves, not fresh values (CIELO-48).
                    let wrapper = self.cfg.push_block(binders.clone(), Some(id));
                    self.cfg.set_terminator(
                        wrapper,
                        CfgTerminator::Goto {
                            target,
                            args: Vec::new(),
                        },
                    );
                    lowered_arms.push(CfgMatchArm {
                        tag: arm.tag,
                        projections: vec![CfgProjectionMode::Borrow; binders.len()],
                        binders,
                        target: wrapper,
                    });
                }
                let default = match default {
                    Some(default) => self.lower_stmt(default, exit, scope),
                    None => self.unit_exit_block(id, exit),
                };
                let block = self.cfg.push_block(Vec::new(), Some(id));
                let scrutinee = self.lower_borrowed(block, scrutinee, scope);
                self.cfg.set_terminator(
                    block,
                    CfgTerminator::Match {
                        scrutinee,
                        arms: lowered_arms,
                        default,
                    },
                );
                block
            }
            LinearStmt::Perform {
                result,
                effect,
                operation,
                args,
                next,
            } => {
                let mut next_scope = scope.clone();
                let result = result.map(|var| {
                    let value = self.cfg.push_value(Some(var));
                    next_scope = next_scope.bind(var, value);
                    value
                });
                let next = self.lower_stmt(next, exit, &next_scope);
                let continuation = self
                    .cfg
                    .push_block(result.into_iter().collect::<Vec<_>>(), Some(id));
                self.cfg.set_terminator(
                    continuation,
                    CfgTerminator::Goto {
                        target: next,
                        args: Vec::new(),
                    },
                );
                let block = self.cfg.push_block(Vec::new(), Some(id));
                let args = args
                    .into_iter()
                    .map(|arg| self.lower_owned(block, arg, scope))
                    .collect();
                self.cfg.set_terminator(
                    block,
                    CfgTerminator::Perform {
                        effect,
                        operation,
                        args,
                        result,
                        target: continuation,
                    },
                );
                block
            }
            LinearStmt::Handle {
                effect,
                clauses,
                body,
                next,
            } => {
                let handler = CfgHandlerId::new(self.next_handler);
                self.next_handler += 1;
                for clause in &clauses {
                    self.lower_clause(handler, clause);
                }
                // The capability scope and the allocation scope are one scope:
                // the evidence record is the region's first slot, and anything
                // else the handler owns (continuation environments, closure
                // environments) joins it rather than getting its own lifetime.
                let region = self.cfg.push_region(
                    RegionOwner::Handler { handler, effect },
                    vec![RegionSlot {
                        kind: RegionSlotKind::HandlerEvidence { effect },
                        placement: Placement::default(),
                    }],
                );
                let after = match next {
                    Some(next) => {
                        let next = self.lower_stmt(next, exit, scope);
                        let after = self.cfg.push_block(Vec::new(), Some(id));
                        self.cfg.push_instruction(
                            after,
                            CfgInstruction::HandlerExit { handler, effect },
                            Some(id),
                        );
                        self.cfg.push_instruction(
                            after,
                            CfgInstruction::RegionExit { region },
                            Some(id),
                        );
                        self.cfg.set_terminator(
                            after,
                            CfgTerminator::Goto {
                                target: next,
                                args: Vec::new(),
                            },
                        );
                        (after, Exit::Discard(after))
                    }
                    None => {
                        let forwarded = self.synthetic_value();
                        let after = self.cfg.push_block(vec![forwarded], Some(id));
                        self.cfg.push_instruction(
                            after,
                            CfgInstruction::HandlerExit { handler, effect },
                            Some(id),
                        );
                        self.cfg.push_instruction(
                            after,
                            CfgInstruction::RegionExit { region },
                            Some(id),
                        );
                        let forwarded_expr = self.cfg.push_expr(CfgExpr::Value(forwarded), None);
                        self.set_exit(after, forwarded_expr, exit);
                        (after, Exit::Yield(after))
                    }
                };
                let body = self.lower_stmt(body, after.1, scope);
                let block = self.cfg.push_block(Vec::new(), Some(id));
                // Region open precedes handler push so the evidence storage
                // exists before its address reaches the handler stack.
                self.cfg
                    .push_instruction(block, CfgInstruction::RegionEnter { region }, Some(id));
                self.cfg.push_instruction(
                    block,
                    CfgInstruction::HandlerEnter { handler, effect },
                    Some(id),
                );
                self.cfg.set_terminator(
                    block,
                    CfgTerminator::Goto {
                        target: body,
                        args: Vec::new(),
                    },
                );
                block
            }
            LinearStmt::Stage { stage, body, next } => {
                let after = match next {
                    Some(next) => {
                        let next = self.lower_stmt(next, exit, scope);
                        let after = self.cfg.push_block(Vec::new(), Some(id));
                        self.cfg.push_instruction(
                            after,
                            CfgInstruction::StageExit { stage },
                            Some(id),
                        );
                        self.cfg.set_terminator(
                            after,
                            CfgTerminator::Goto {
                                target: next,
                                args: Vec::new(),
                            },
                        );
                        (after, Exit::Discard(after))
                    }
                    None => {
                        let forwarded = self.synthetic_value();
                        let after = self.cfg.push_block(vec![forwarded], Some(id));
                        self.cfg.push_instruction(
                            after,
                            CfgInstruction::StageExit { stage },
                            Some(id),
                        );
                        let forwarded_expr = self.cfg.push_expr(CfgExpr::Value(forwarded), None);
                        self.set_exit(after, forwarded_expr, exit);
                        (after, Exit::Yield(after))
                    }
                };
                let body = self.lower_stmt(body, after.1, scope);
                let block = self.cfg.push_block(Vec::new(), Some(id));
                self.cfg
                    .push_instruction(block, CfgInstruction::StageEnter { stage }, Some(id));
                self.cfg.set_terminator(
                    block,
                    CfgTerminator::Goto {
                        target: body,
                        args: Vec::new(),
                    },
                );
                block
            }
            LinearStmt::Resumption {
                id: resumption,
                clause,
                param,
                continuation,
            } => {
                let argument = self.cfg.push_value(Some(param));
                // The code pointer, and the only thing that has to survive the
                // round trip through the continuation. A block parameter rather
                // than storage: it is filled by the jump edge, so the sites can
                // disagree about it without two instructions writing one value.
                let label = self.synthetic_value();
                let resumed = self.synthetic_value();
                let entry = self.cfg.push_block(vec![argument, label], Some(id));
                let dispatch = self.cfg.push_block(vec![resumed], Some(id));
                let body = self.lower_stmt(
                    continuation,
                    Exit::Yield(dispatch),
                    &scope.bind(param, argument),
                );
                self.cfg.set_terminator(
                    entry,
                    CfgTerminator::Goto {
                        target: body,
                        args: Vec::new(),
                    },
                );

                let shadowed = self.open_resumptions.insert(
                    resumption,
                    OpenResumption {
                        entry,
                        resumed,
                        arms: Vec::new(),
                    },
                );
                let clause = self.lower_stmt(clause, exit, scope);
                let open = self
                    .open_resumptions
                    .remove(&resumption)
                    .expect("the clause body cannot close its own resumption");
                if let Some(shadowed) = shadowed {
                    self.open_resumptions.insert(resumption, shadowed);
                }

                // Sized by the largest label, not by how many arms came back: a
                // site the clause body never reached leaves a hole, and a table
                // one short would send the site above it to the default instead.
                let default = self.cfg.push_block(Vec::new(), Some(id));
                self.cfg.set_terminator(default, CfgTerminator::Unreachable);
                let width = open
                    .arms
                    .iter()
                    .map(|(label, _)| *label as usize + 1)
                    .max()
                    .unwrap_or(0);
                let mut targets = vec![default; width];
                for (label, target) in open.arms {
                    if let Some(slot) = targets.get_mut(label as usize) {
                        *slot = target;
                    }
                }
                let selector = self.cfg.push_expr(CfgExpr::Value(label), None);
                self.cfg.set_terminator(
                    dispatch,
                    CfgTerminator::Switch {
                        selector,
                        targets,
                        default,
                    },
                );
                clause
            }
            LinearStmt::ResumeJump {
                resumption,
                label,
                arg,
                result,
                next,
            } => {
                match self
                    .open_resumptions
                    .get(&resumption)
                    .map(|open| (open.entry, open.resumed))
                {
                    Some((entry, resumed)) => {
                        let after = self.lower_stmt(next, exit, &scope.bind(result, resumed));
                        if let Some(open) = self.open_resumptions.get_mut(&resumption) {
                            open.arms.push((label, after));
                        }
                        let block = self.cfg.push_block(Vec::new(), Some(id));
                        let arg = self.lower_owned(block, arg, scope);
                        let selector = self
                            .cfg
                            .push_expr(CfgExpr::Literal(Literal::Int(i64::from(label))), None);
                        self.cfg.set_terminator(
                            block,
                            CfgTerminator::Goto {
                                target: entry,
                                args: vec![arg, selector],
                            },
                        );
                        block
                    }
                    // Only malformed input reaches this: a jump whose resumption
                    // is not the one being lowered. An error node keeps the graph
                    // well-formed, so `validate` reports it instead of panicking.
                    None => {
                        let block = self.cfg.push_block(Vec::new(), Some(id));
                        self.cfg
                            .push_instruction(block, CfgInstruction::Error, Some(id));
                        let unit = self.unit_expr();
                        self.set_exit(block, unit, exit);
                        block
                    }
                }
            }
            LinearStmt::Hole => {
                let block = self.cfg.push_block(Vec::new(), Some(id));
                self.cfg
                    .push_instruction(block, CfgInstruction::Hole, Some(id));
                let unit = self.unit_expr();
                self.set_exit(block, unit, exit);
                block
            }
            LinearStmt::Error => {
                let block = self.cfg.push_block(Vec::new(), Some(id));
                self.cfg
                    .push_instruction(block, CfgInstruction::Error, Some(id));
                let unit = self.unit_expr();
                self.set_exit(block, unit, exit);
                block
            }
        };

        self.statements.insert(key, entry);
        entry
    }

    fn free_vars_of(&self, id: LinearStmtId) -> &[VarId] {
        self.free_vars
            .get(id.index())
            .map_or(&[][..], Vec::as_slice)
    }

    fn cfg_func_id(&self, callee: LinearFuncId) -> CfgFuncId {
        self.functions
            .get(&callee)
            .copied()
            .unwrap_or(CfgFuncId::INVALID)
    }

    #[allow(clippy::too_many_arguments)]
    fn lower_call(
        &mut self,
        id: LinearStmtId,
        binding: VarId,
        callee: cielo_base::ids::SymbolId,
        callee_fn: LinearFuncId,
        args: Vec<LinearExprId>,
        next: LinearStmtId,
        exit: Exit,
        scope: &Scope,
        convention: CfgCallConvention,
    ) -> CfgBlockId {
        let result = self.cfg.push_value(Some(binding));
        let next = self.lower_stmt(next, exit, &scope.bind(binding, result));
        let continuation = self.cfg.push_block(vec![result], Some(id));
        self.cfg.set_terminator(
            continuation,
            CfgTerminator::Goto {
                target: next,
                args: Vec::new(),
            },
        );
        let block = self.cfg.push_block(Vec::new(), Some(id));
        let args = args
            .into_iter()
            .map(|arg| self.lower_owned(block, arg, scope))
            .collect();
        self.cfg.set_terminator(
            block,
            CfgTerminator::Call {
                convention,
                callee,
                callee_fn: self.cfg_func_id(callee_fn),
                args,
                result,
                target: continuation,
            },
        );
        block
    }

    fn unit_exit_block(&mut self, source: LinearStmtId, exit: Exit) -> CfgBlockId {
        let block = self.cfg.push_block(Vec::new(), Some(source));
        let unit = self.unit_expr();
        self.set_exit(block, unit, exit);
        block
    }

    fn set_exit(&mut self, block: CfgBlockId, value: CfgExprId, exit: Exit) {
        let terminator = match exit {
            Exit::Return => CfgTerminator::Return(value),
            Exit::Yield(target) => CfgTerminator::Goto {
                target,
                args: vec![value],
            },
            Exit::Discard(target) => {
                let result = self.synthetic_value();
                let source = self.cfg.block(block).and_then(|block| block.source);
                self.cfg
                    .push_instruction(block, CfgInstruction::Eval { result, value }, source);
                CfgTerminator::Goto {
                    target,
                    args: Vec::new(),
                }
            }
        };
        self.cfg.set_terminator(block, terminator);
    }
}

/// Free variables of every linear statement, indexed by arena position, sorted.
///
/// Both arenas are built children-first, so a single forward pass suffices and
/// a forward reference panics rather than silently under-approximating: too
/// small a set here would let two lowerings that disagree on a value share a
/// block.
fn free_vars(linear: &LinearProgram) -> Vec<Vec<VarId>> {
    let mut exprs: Vec<Vec<VarId>> = Vec::with_capacity(linear.exprs().len());
    for node in linear.exprs() {
        let mut vars = match node.kind {
            LinearExpr::Var(var) => vec![var],
            _ => Vec::new(),
        };
        for child in node.kind.child_exprs() {
            vars.extend_from_slice(&exprs[child.index()]);
        }
        exprs.push(sorted(vars));
    }

    let mut stmts: Vec<Vec<VarId>> = Vec::with_capacity(linear.stmts().len());
    for node in linear.stmts() {
        let mut vars = Vec::new();
        let read_expr = |vars: &mut Vec<VarId>, expr: &LinearExprId| {
            vars.extend_from_slice(&exprs[expr.index()]);
        };
        match &node.kind {
            LinearStmt::Return(value) => read_expr(&mut vars, value),
            LinearStmt::Let {
                binding,
                value,
                next,
            } => {
                read_expr(&mut vars, value);
                extend_unbound(
                    &mut vars,
                    &stmts[next.index()],
                    std::slice::from_ref(binding),
                );
            }
            LinearStmt::Val {
                binding,
                value,
                next,
            } => {
                vars.extend_from_slice(&stmts[value.index()]);
                extend_unbound(
                    &mut vars,
                    &stmts[next.index()],
                    std::slice::from_ref(binding),
                );
            }
            LinearStmt::PureCall {
                result, args, next, ..
            }
            | LinearStmt::DirectCall {
                result, args, next, ..
            }
            | LinearStmt::ControlCall {
                result, args, next, ..
            } => {
                for arg in args {
                    read_expr(&mut vars, arg);
                }
                extend_unbound(
                    &mut vars,
                    &stmts[next.index()],
                    std::slice::from_ref(result),
                );
            }
            LinearStmt::If {
                cond,
                then_branch,
                else_branch,
            } => {
                read_expr(&mut vars, cond);
                vars.extend_from_slice(&stmts[then_branch.index()]);
                vars.extend_from_slice(&stmts[else_branch.index()]);
            }
            LinearStmt::Match {
                scrutinee,
                arms,
                default,
            } => {
                read_expr(&mut vars, scrutinee);
                for arm in arms {
                    extend_unbound(&mut vars, &stmts[arm.body.index()], &arm.binders);
                }
                if let Some(default) = default {
                    vars.extend_from_slice(&stmts[default.index()]);
                }
            }
            LinearStmt::Perform {
                result, args, next, ..
            } => {
                for arg in args {
                    read_expr(&mut vars, arg);
                }
                let bound = result
                    .as_ref()
                    .map(std::slice::from_ref)
                    .unwrap_or_default();
                extend_unbound(&mut vars, &stmts[next.index()], bound);
            }
            LinearStmt::Handle { body, next, .. } | LinearStmt::Stage { body, next, .. } => {
                vars.extend_from_slice(&stmts[body.index()]);
                if let Some(next) = next {
                    vars.extend_from_slice(&stmts[next.index()]);
                }
            }
            LinearStmt::Resumption {
                clause,
                param,
                continuation,
                ..
            } => {
                vars.extend_from_slice(&stmts[clause.index()]);
                extend_unbound(
                    &mut vars,
                    &stmts[continuation.index()],
                    std::slice::from_ref(param),
                );
            }
            LinearStmt::ResumeJump {
                arg, result, next, ..
            } => {
                read_expr(&mut vars, arg);
                extend_unbound(
                    &mut vars,
                    &stmts[next.index()],
                    std::slice::from_ref(result),
                );
            }
            LinearStmt::Hole | LinearStmt::Error => {}
        }
        stmts.push(sorted(vars));
    }
    stmts
}

fn extend_unbound(vars: &mut Vec<VarId>, inner: &[VarId], bound: &[VarId]) {
    vars.extend(inner.iter().copied().filter(|var| !bound.contains(var)));
}

fn sorted(mut vars: Vec<VarId>) -> Vec<VarId> {
    vars.sort_unstable_by_key(|var| var.as_u32());
    vars.dedup();
    vars
}
