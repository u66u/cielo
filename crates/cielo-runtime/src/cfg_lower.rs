//! Lower the structured Linear IR into an explicit block-form CFG.
//!
//! The lowering is continuation-aware. In particular, `Val`'s value graph runs
//! before its `next` graph, and handler/stage cleanup blocks run between their
//! bodies and continuations. Treating those nodes as ordinary sibling edges is
//! the source of the old last-use/ARC ordering bugs.

use std::collections::HashMap;

use cielo_base::ids::{
    CfgBlockId, CfgExprId, CfgFuncId, CfgHandlerId, CfgValueId, LinearExprId, LinearFuncId,
    LinearStmtId, VarId,
};
use cielo_ir::cfg::{
    CfgCallConvention, CfgExpr, CfgFunction, CfgInstruction, CfgMatchArm, CfgProgram,
    CfgProjectionMode, CfgTerminator,
};
use cielo_ir::core::Literal;
use cielo_ir::linear::{LinearExpr, LinearProgram, LinearStmt};
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

struct Lowerer<'a> {
    linear: &'a LinearProgram,
    cfg: CfgProgram,
    values: HashMap<VarId, CfgValueId>,
    /// Keyed by block because `lower_borrowed` binds producers in the block
    /// that reads them. A hit from another block would hand back a value that
    /// this block has no path to a definition of.
    expressions: HashMap<(LinearExprId, CfgBlockId), CfgExprId>,
    statements: HashMap<(LinearStmtId, Exit), CfgBlockId>,
    functions: HashMap<LinearFuncId, CfgFuncId>,
    unit: Option<CfgExprId>,
    next_handler: usize,
}

impl<'a> Lowerer<'a> {
    fn new(linear: &'a LinearProgram) -> Self {
        Self {
            linear,
            cfg: CfgProgram::default(),
            values: HashMap::new(),
            expressions: HashMap::new(),
            statements: HashMap::new(),
            functions: HashMap::new(),
            unit: None,
            next_handler: 0,
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
            let body = self.lower_stmt(function.body, Exit::Return);
            let params = function
                .params
                .iter()
                .map(|var| self.value_for_var(*var))
                .collect::<Vec<_>>();
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

        self.cfg.entrypoints = self
            .linear
            .entrypoints
            .iter()
            .filter_map(|id| self.functions.get(id).copied())
            .collect();
        self.cfg
    }

    fn value_for_var(&mut self, var: VarId) -> CfgValueId {
        if let Some(value) = self.values.get(&var) {
            return *value;
        }
        let value = self.cfg.push_value(Some(var));
        self.values.insert(var, value);
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
    fn lower_owned(&mut self, block: CfgBlockId, id: LinearExprId) -> CfgExprId {
        if let Some(lowered) = self.expressions.get(&(id, block)) {
            return *lowered;
        }
        let kind = match self.linear.expr(id).map(|node| node.kind.clone()) {
            Some(LinearExpr::Var(var)) => CfgExpr::Value(self.value_for_var(var)),
            Some(LinearExpr::Literal(literal)) => CfgExpr::Literal(literal),
            Some(LinearExpr::Unary { op, expr }) => CfgExpr::Unary {
                op,
                expr: self.lower_borrowed(block, expr),
            },
            Some(LinearExpr::Binary { op, lhs, rhs }) => CfgExpr::Binary {
                op,
                lhs: self.lower_borrowed(block, lhs),
                rhs: self.lower_borrowed(block, rhs),
            },
            Some(LinearExpr::Field { base, index }) => CfgExpr::Field {
                base: self.lower_borrowed(block, base),
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
                    .map(|arg| self.lower_owned(block, arg))
                    .collect(),
            },
            Some(LinearExpr::BuiltinCall { builtin, args }) => CfgExpr::BuiltinCall {
                builtin,
                args: args
                    .into_iter()
                    .map(|arg| self.lower_owned(block, arg))
                    .collect(),
            },
            Some(LinearExpr::MakeStruct { ty, fields }) => CfgExpr::MakeStruct {
                ty,
                fields: fields
                    .into_iter()
                    .map(|field| self.lower_owned(block, field))
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
                    .map(|field| self.lower_owned(block, field))
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
    fn lower_borrowed(&mut self, block: CfgBlockId, id: LinearExprId) -> CfgExprId {
        let lowered = self.lower_owned(block, id);
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

    fn lower_stmt(&mut self, id: LinearStmtId, exit: Exit) -> CfgBlockId {
        if let Some(lowered) = self.statements.get(&(id, exit)) {
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
                let value = self.lower_owned(block, value);
                self.set_exit(block, value, exit);
                block
            }
            LinearStmt::Let {
                binding,
                value,
                next,
            } => {
                let next = self.lower_stmt(next, exit);
                let block = self.cfg.push_block(Vec::new(), Some(id));
                let value = self.lower_owned(block, value);
                let result = self.value_for_var(binding);
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
                let next = self.lower_stmt(next, exit);
                let result = self.value_for_var(binding);
                let continuation = self.cfg.push_block(vec![result], Some(id));
                self.cfg.set_terminator(
                    continuation,
                    CfgTerminator::Goto {
                        target: next,
                        args: Vec::new(),
                    },
                );
                self.lower_stmt(value, Exit::Yield(continuation))
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
                CfgCallConvention::Control,
            ),
            LinearStmt::If {
                cond,
                then_branch,
                else_branch,
            } => {
                let then_target = self.lower_stmt(then_branch, exit);
                let else_target = self.lower_stmt(else_branch, exit);
                let block = self.cfg.push_block(Vec::new(), Some(id));
                let cond = self.lower_borrowed(block, cond);
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
                    let target = self.lower_stmt(arm.body, exit);
                    let binders = arm
                        .binders
                        .into_iter()
                        .map(|binder| self.value_for_var(binder))
                        .collect::<Vec<_>>();
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
                    Some(default) => self.lower_stmt(default, exit),
                    None => self.unit_exit_block(id, exit),
                };
                let block = self.cfg.push_block(Vec::new(), Some(id));
                let scrutinee = self.lower_borrowed(block, scrutinee);
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
                let next = self.lower_stmt(next, exit);
                let result = result.map(|var| self.value_for_var(var));
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
                    .map(|arg| self.lower_owned(block, arg))
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
            LinearStmt::Handle { effect, body, next } => {
                let handler = CfgHandlerId::new(self.next_handler);
                self.next_handler += 1;
                // The capability scope and the allocation scope are one scope:
                // the evidence record is the region's first slot, and anything
                // else the handler owns (continuation environments, closure
                // environments) joins it rather than getting its own lifetime.
                let region = self.cfg.push_region(
                    RegionOwner::Handler(handler),
                    vec![RegionSlot {
                        kind: RegionSlotKind::HandlerEvidence { effect },
                        placement: Placement::default(),
                    }],
                );
                let after = match next {
                    Some(next) => {
                        let next = self.lower_stmt(next, exit);
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
                let body = self.lower_stmt(body, after.1);
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
                        let next = self.lower_stmt(next, exit);
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
                let body = self.lower_stmt(body, after.1);
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

        self.statements.insert((id, exit), entry);
        entry
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
        result: VarId,
        callee: cielo_base::ids::SymbolId,
        callee_fn: LinearFuncId,
        args: Vec<LinearExprId>,
        next: LinearStmtId,
        exit: Exit,
        convention: CfgCallConvention,
    ) -> CfgBlockId {
        let next = self.lower_stmt(next, exit);
        let result = self.value_for_var(result);
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
            .map(|arg| self.lower_owned(block, arg))
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
