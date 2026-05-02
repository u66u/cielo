//! Block-form control-flow IR.
//!
//! This IR is deliberately separate from `linear`: CFG node identifiers cannot
//! accidentally be used to index a linear arena. Values are SSA-like. Source
//! variables are preserved as optional provenance while synthetic block values
//! are free to model continuations and cleanup paths.

use crate::core::{BinaryOp, Literal, StageDirective, UnaryOp};
use cielo_base::{
    CfgBlockId, CfgExprId, CfgFuncId, CfgHandlerId, CfgInstId, CfgValueId, EffectLabelId,
    LinearExprId, LinearStmtId, SymbolId, VarId,
};

#[derive(Clone, Debug, Default)]
pub struct CfgProgram {
    pub functions: Vec<CfgFunction>,
    pub entrypoints: Vec<CfgFuncId>,
    values: Vec<CfgValue>,
    exprs: Vec<CfgExprNode>,
    instructions: Vec<CfgInstructionNode>,
    blocks: Vec<CfgBlock>,
}

impl CfgProgram {
    pub fn push_value(&mut self, source_var: Option<VarId>) -> CfgValueId {
        let id = CfgValueId::new(self.values.len());
        self.values.push(CfgValue { id, source_var });
        id
    }

    pub fn push_expr(&mut self, kind: CfgExpr, source: Option<LinearExprId>) -> CfgExprId {
        let id = CfgExprId::new(self.exprs.len());
        self.exprs.push(CfgExprNode { kind, source });
        id
    }

    pub fn push_block(
        &mut self,
        params: Vec<CfgValueId>,
        source: Option<LinearStmtId>,
    ) -> CfgBlockId {
        let id = CfgBlockId::new(self.blocks.len());
        self.blocks.push(CfgBlock {
            id,
            params,
            instructions: Vec::new(),
            terminator: CfgTerminator::Unreachable,
            entry_arc: Vec::new(),
            terminator_arc: CfgArcOps::default(),
            source,
        });
        id
    }

    pub fn push_instruction(
        &mut self,
        block: CfgBlockId,
        kind: CfgInstruction,
        source: Option<LinearStmtId>,
    ) -> CfgInstId {
        let id = CfgInstId::new(self.instructions.len());
        self.instructions.push(CfgInstructionNode {
            id,
            kind,
            source,
            arc: CfgArcOps::default(),
        });
        self.blocks[block.index()].instructions.push(id);
        id
    }

    pub fn set_terminator(&mut self, block: CfgBlockId, terminator: CfgTerminator) {
        self.blocks[block.index()].terminator = terminator;
    }

    pub fn instruction_mut(&mut self, id: CfgInstId) -> Option<&mut CfgInstructionNode> {
        self.instructions.get_mut(id.index())
    }

    pub fn block_mut(&mut self, id: CfgBlockId) -> Option<&mut CfgBlock> {
        self.blocks.get_mut(id.index())
    }

    pub fn value(&self, id: CfgValueId) -> Option<&CfgValue> {
        self.values.get(id.index())
    }

    pub fn expr(&self, id: CfgExprId) -> Option<&CfgExprNode> {
        self.exprs.get(id.index())
    }

    pub fn instruction(&self, id: CfgInstId) -> Option<&CfgInstructionNode> {
        self.instructions.get(id.index())
    }

    pub fn block(&self, id: CfgBlockId) -> Option<&CfgBlock> {
        self.blocks.get(id.index())
    }

    pub fn values(&self) -> &[CfgValue] {
        &self.values
    }

    pub fn exprs(&self) -> &[CfgExprNode] {
        &self.exprs
    }

    pub fn instructions(&self) -> &[CfgInstructionNode] {
        &self.instructions
    }

    pub fn blocks(&self) -> &[CfgBlock] {
        &self.blocks
    }

    /// Checks arena references, successor references, and continuation arity.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();
        for block in &self.blocks {
            for param in &block.params {
                if self.value(*param).is_none() {
                    errors.push(format!(
                        "block {} has invalid parameter {}",
                        block.id, param
                    ));
                }
            }
            for instruction in &block.instructions {
                if self.instruction(*instruction).is_none() {
                    errors.push(format!(
                        "block {} has invalid instruction {}",
                        block.id, instruction
                    ));
                }
            }
            for (target, argument_count) in block.terminator.successors_with_arity() {
                match self.block(target) {
                    Some(target_block) if target_block.params.len() != argument_count => {
                        errors.push(format!(
                            "edge {} -> {} passes {} values to {} parameters",
                            block.id,
                            target,
                            argument_count,
                            target_block.params.len()
                        ));
                    }
                    Some(_) => {}
                    None => errors.push(format!(
                        "block {} has invalid successor {}",
                        block.id, target
                    )),
                }
            }
            if let CfgTerminator::Match { arms, .. } = &block.terminator {
                for arm in arms {
                    if arm.binders.len() != arm.projections.len() {
                        errors.push(format!(
                            "match edge {} -> {} has {} binders but {} projection modes",
                            block.id,
                            arm.target,
                            arm.binders.len(),
                            arm.projections.len()
                        ));
                    }
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

#[derive(Clone, Debug)]
pub struct CfgValue {
    pub id: CfgValueId,
    pub source_var: Option<VarId>,
}

#[derive(Clone, Debug)]
pub struct CfgExprNode {
    pub kind: CfgExpr,
    pub source: Option<LinearExprId>,
}

#[derive(Clone, Debug)]
pub enum CfgExpr {
    Value(CfgValueId),
    Literal(Literal),
    Unary {
        op: UnaryOp,
        expr: CfgExprId,
    },
    Binary {
        op: BinaryOp,
        lhs: CfgExprId,
        rhs: CfgExprId,
    },
    PureCall {
        callee: SymbolId,
        callee_fn: CfgFuncId,
        args: Vec<CfgExprId>,
    },
    Field {
        base: CfgExprId,
        index: u32,
    },
    MakeStruct {
        ty: SymbolId,
        fields: Vec<CfgExprId>,
    },
    MakeEnum {
        ty: SymbolId,
        variant: SymbolId,
        fields: Vec<CfgExprId>,
    },
    Error,
}

#[derive(Clone, Debug)]
pub struct CfgInstructionNode {
    pub id: CfgInstId,
    pub kind: CfgInstruction,
    pub source: Option<LinearStmtId>,
    pub arc: CfgArcOps,
}

#[derive(Clone, Debug)]
pub enum CfgInstruction {
    Let {
        result: CfgValueId,
        value: CfgExprId,
    },
    Eval {
        result: CfgValueId,
        value: CfgExprId,
    },
    HandlerEnter {
        handler: CfgHandlerId,
        effect: EffectLabelId,
    },
    HandlerExit {
        handler: CfgHandlerId,
        effect: EffectLabelId,
    },
    StageEnter {
        stage: StageDirective,
    },
    StageExit {
        stage: StageDirective,
    },
    Hole,
    Error,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum CfgCallConvention {
    Pure,
    Direct,
    Control,
}

impl From<crate::linear::CallConvention> for CfgCallConvention {
    fn from(value: crate::linear::CallConvention) -> Self {
        match value {
            crate::linear::CallConvention::Pure => Self::Pure,
            crate::linear::CallConvention::Direct => Self::Direct,
            crate::linear::CallConvention::Control => Self::Control,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CfgMatchArm {
    pub tag: SymbolId,
    /// Values defined by the arm edge and available in `target`.
    pub binders: Vec<CfgValueId>,
    pub projections: Vec<CfgProjectionMode>,
    pub target: CfgBlockId,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum CfgProjectionMode {
    Borrow,
    Copy,
    Move,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum CfgArcOpKind {
    Retain,
    Release,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CfgArcOp {
    pub kind: CfgArcOpKind,
    pub value: CfgValueId,
}

#[derive(Clone, Debug, Default)]
pub struct CfgArcOps {
    pub pre: Vec<CfgArcOp>,
    pub post: Vec<CfgArcOp>,
}

#[derive(Clone, Debug)]
pub enum CfgTerminator {
    Return(CfgExprId),
    Goto {
        target: CfgBlockId,
        args: Vec<CfgExprId>,
    },
    Branch {
        cond: CfgExprId,
        then_target: CfgBlockId,
        else_target: CfgBlockId,
    },
    Match {
        scrutinee: CfgExprId,
        arms: Vec<CfgMatchArm>,
        /// Linear IR has an implicit unit-valued fallback. It is explicit here.
        default: CfgBlockId,
    },
    /// Integer dispatch. Targets take no arguments: a continuation shared by
    /// several dispatch sites receives its values through its own block params.
    Switch {
        selector: CfgExprId,
        targets: Vec<CfgBlockId>,
        default: CfgBlockId,
    },
    Call {
        convention: CfgCallConvention,
        callee: SymbolId,
        callee_fn: CfgFuncId,
        args: Vec<CfgExprId>,
        result: CfgValueId,
        target: CfgBlockId,
    },
    Perform {
        effect: EffectLabelId,
        operation: SymbolId,
        args: Vec<CfgExprId>,
        result: Option<CfgValueId>,
        target: CfgBlockId,
    },
    Unreachable,
}

impl CfgTerminator {
    pub fn successors(&self) -> Vec<CfgBlockId> {
        self.successors_with_arity()
            .into_iter()
            .map(|(target, _)| target)
            .collect()
    }

    pub fn successors_with_arity(&self) -> Vec<(CfgBlockId, usize)> {
        match self {
            Self::Return(_) | Self::Unreachable => Vec::new(),
            Self::Goto { target, args } => vec![(*target, args.len())],
            Self::Branch {
                then_target,
                else_target,
                ..
            } => vec![(*then_target, 0), (*else_target, 0)],
            Self::Match { arms, default, .. } => {
                let mut successors = arms
                    .iter()
                    .map(|arm| (arm.target, arm.binders.len()))
                    .collect::<Vec<_>>();
                successors.push((*default, 0));
                successors
            }
            Self::Switch {
                targets, default, ..
            } => {
                let mut successors = targets
                    .iter()
                    .map(|target| (*target, 0))
                    .collect::<Vec<_>>();
                successors.push((*default, 0));
                successors
            }
            Self::Call { target, .. } => vec![(*target, 1)],
            Self::Perform { result, target, .. } => {
                vec![(*target, usize::from(result.is_some()))]
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct CfgBlock {
    pub id: CfgBlockId,
    pub params: Vec<CfgValueId>,
    pub instructions: Vec<CfgInstId>,
    pub terminator: CfgTerminator,
    pub entry_arc: Vec<CfgArcOp>,
    pub terminator_arc: CfgArcOps,
    pub source: Option<LinearStmtId>,
}

#[derive(Clone, Debug)]
pub struct CfgFunction {
    pub id: CfgFuncId,
    pub name: SymbolId,
    pub params: Vec<CfgValueId>,
    pub entry: CfgBlockId,
}
