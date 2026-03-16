//! Lower the structured Linear IR into an explicit block-form CFG.
//!
//! The lowering is continuation-aware. In particular, `Val`'s value graph runs
//! before its `next` graph, and handler/stage cleanup blocks run between their
//! bodies and continuations. Treating those nodes as ordinary sibling edges is
//! the source of the old last-use/ARC ordering bugs.

use std::collections::HashMap;

use crate::common::ids::{
    CfgBlockId, CfgExprId, CfgFuncId, CfgHandlerId, CfgValueId, LinearExprId, LinearFuncId,
    LinearStmtId, VarId,
};
use crate::ir::cfg::{
    CfgCallConvention, CfgExpr, CfgFunction, CfgInstruction, CfgMatchArm, CfgProgram,
    CfgProjectionMode, CfgTerminator,
};
use crate::ir::core::Literal;
use crate::ir::linear::{CallConvention, LinearExpr, LinearProgram, LinearStmt};
use crate::passes::linearize::Linearized;

#[derive(Clone, Debug)]
pub struct CfgLowered {
    pub linearized: Linearized,
    pub cfg: CfgProgram,
}

pub fn run(linearized: Linearized) -> CfgLowered {
    let cfg = lower_program(&linearized.linear);
    debug_assert!(
        cfg.validate().is_ok(),
        "cfg_lower produced an invalid control-flow graph"
    );
    CfgLowered { linearized, cfg }
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
    expressions: HashMap<LinearExprId, CfgExprId>,
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
            self.functions.insert(function.id, id);
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

    fn lower_expr(&mut self, id: LinearExprId) -> CfgExprId {
        if let Some(lowered) = self.expressions.get(&id) {
            return *lowered;
        }
        let kind = match self.linear.expr(id).map(|node| node.kind.clone()) {
            Some(LinearExpr::Var(var)) => CfgExpr::Value(self.value_for_var(var)),
            Some(LinearExpr::Literal(literal)) => CfgExpr::Literal(literal),
            Some(LinearExpr::Unary { op, expr }) => CfgExpr::Unary {
                op,
                expr: self.lower_expr(expr),
            },
            Some(LinearExpr::Binary { op, lhs, rhs }) => CfgExpr::Binary {
                op,
                lhs: self.lower_expr(lhs),
                rhs: self.lower_expr(rhs),
            },
            Some(LinearExpr::PureCall { callee, args }) => CfgExpr::PureCall {
                callee,
                args: args.into_iter().map(|arg| self.lower_expr(arg)).collect(),
            },
            Some(LinearExpr::MakeStruct { ty, fields }) => CfgExpr::MakeStruct {
                ty,
                fields: fields
                    .into_iter()
                    .map(|field| self.lower_expr(field))
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
                    .map(|field| self.lower_expr(field))
                    .collect(),
            },
            Some(LinearExpr::Error) | None => CfgExpr::Error,
        };
        let lowered = self.cfg.push_expr(kind, Some(id));
        self.expressions.insert(id, lowered);
        lowered
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
                let value = self.lower_expr(value);
                let block = self.cfg.push_block(Vec::new(), Some(id));
                self.set_exit(block, value, exit);
                block
            }
            LinearStmt::Let {
                binding,
                value,
                next,
            } => {
                let next = self.lower_stmt(next, exit);
                let value = self.lower_expr(value);
                let result = self.value_for_var(binding);
                let block = self.cfg.push_block(Vec::new(), Some(id));
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
                args,
                next,
            } => self.lower_call(
                id,
                result,
                callee,
                args,
                next,
                exit,
                CfgCallConvention::Pure,
            ),
            LinearStmt::DirectCall {
                result,
                callee,
                args,
                next,
            } => self.lower_call(
                id,
                result,
                callee,
                args,
                next,
                exit,
                CfgCallConvention::Direct,
            ),
            LinearStmt::ControlCall {
                result,
                callee,
                args,
                next,
            } => self.lower_call(
                id,
                result,
                callee,
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
                let cond = self.lower_expr(cond);
                let block = self.cfg.push_block(Vec::new(), Some(id));
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
                let scrutinee = self.lower_expr(scrutinee);
                let block = self.cfg.push_block(Vec::new(), Some(id));
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
                let args = args.into_iter().map(|arg| self.lower_expr(arg)).collect();
                let block = self.cfg.push_block(Vec::new(), Some(id));
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
                let after = match next {
                    Some(next) => {
                        let next = self.lower_stmt(next, exit);
                        let after = self.cfg.push_block(Vec::new(), Some(id));
                        self.cfg.push_instruction(
                            after,
                            CfgInstruction::HandlerExit { handler, effect },
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
                        let forwarded_expr = self.cfg.push_expr(CfgExpr::Value(forwarded), None);
                        self.set_exit(after, forwarded_expr, exit);
                        (after, Exit::Yield(after))
                    }
                };
                let body = self.lower_stmt(body, after.1);
                let block = self.cfg.push_block(Vec::new(), Some(id));
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

    #[allow(clippy::too_many_arguments)]
    fn lower_call(
        &mut self,
        id: LinearStmtId,
        result: VarId,
        callee: crate::common::ids::SymbolId,
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
        let args = args.into_iter().map(|arg| self.lower_expr(arg)).collect();
        let block = self.cfg.push_block(Vec::new(), Some(id));
        self.cfg.set_terminator(
            block,
            CfgTerminator::Call {
                convention,
                callee,
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

impl From<CallConvention> for CfgCallConvention {
    fn from(value: CallConvention) -> Self {
        match value {
            CallConvention::Pure => Self::Pure,
            CallConvention::Direct => Self::Direct,
            CallConvention::Control => Self::Control,
        }
    }
}
