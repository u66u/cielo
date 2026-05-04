//! Block-form control-flow IR.
//!
//! This IR is deliberately separate from `linear`: CFG node identifiers cannot
//! accidentally be used to index a linear arena. Values are SSA-like. Source
//! variables are preserved as optional provenance while synthetic block values
//! are free to model continuations and cleanup paths.

use crate::builtins::Builtin;
use crate::core::{BinaryOp, Literal, StageDirective, UnaryOp};
use crate::ownership::OperandRole;
use cielo_base::{
    CfgBlockId, CfgExprId, CfgFuncId, CfgHandlerId, CfgInstId, CfgValueId, EffectLabelId,
    LinearExprId, LinearStmtId, SymbolId, VarId,
};
use smallvec::{SmallVec, smallvec};
use std::hash::{Hash, Hasher};

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
    BuiltinCall {
        builtin: Builtin,
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

impl CfgExpr {
    /// See [`crate::core::ExprKind::operands`]. Reference counting reads the
    /// roles here directly, so an operand listed as `Owned` gets a retain and
    /// one listed as `Read` does not.
    pub fn operands(&self) -> SmallVec<[(CfgExprId, OperandRole); 4]> {
        match self {
            Self::Value(_) | Self::Literal(_) | Self::Error => SmallVec::new(),
            Self::Unary { expr: operand, .. } | Self::Field { base: operand, .. } => {
                smallvec![(*operand, OperandRole::Read)]
            }
            Self::Binary { lhs, rhs, .. } => {
                smallvec![(*lhs, OperandRole::Read), (*rhs, OperandRole::Read)]
            }
            Self::PureCall { args: operands, .. }
            | Self::MakeStruct {
                fields: operands, ..
            }
            | Self::MakeEnum {
                fields: operands, ..
            } => operands
                .iter()
                .map(|operand| (*operand, OperandRole::Owned))
                .collect(),
        }
    }

    pub fn child_exprs(&self) -> SmallVec<[CfgExprId; 4]> {
        self.operands().into_iter().map(|(expr, _)| expr).collect()
    }

    pub const fn tag(&self) -> &'static str {
        match self {
            Self::Literal(_) => "lit",
            Self::Value(_) => "val",
            Self::Unary { .. } => "un",
            Self::Field { .. } => "field",
            Self::Binary { .. } => "bin",
            Self::PureCall { .. } => "call",
            Self::MakeStruct { .. } => "mk_struct",
            Self::MakeEnum { .. } => "mk_enum",
            Self::Error => "err",
        }
    }

    pub fn hash_own<H: Hasher>(&self, hasher: &mut H) {
        self.tag().hash(hasher);
        match self {
            Self::Value(value) => value.as_u32().hash(hasher),
            Self::Literal(literal) => literal.hash_structural(hasher),
            Self::Unary { op, .. } => std::mem::discriminant(op).hash(hasher),
            Self::Binary { op, .. } => std::mem::discriminant(op).hash(hasher),
            Self::Field { index, .. } => index.hash(hasher),
            Self::PureCall { callee, .. } => callee.as_u32().hash(hasher),
            Self::MakeStruct { ty, .. } => ty.as_u32().hash(hasher),
            Self::MakeEnum { ty, variant, .. } => {
                ty.as_u32().hash(hasher);
                variant.as_u32().hash(hasher);
            }
            Self::Error => {}
        }
    }
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

impl CfgInstruction {
    /// `Let` binds its operand to a value that outlives the instruction, so it
    /// takes ownership; `Eval` discards the result and only reads.
    pub fn operands(&self) -> SmallVec<[(CfgExprId, OperandRole); 2]> {
        match self {
            Self::Let { value, .. } => smallvec![(*value, OperandRole::Owned)],
            Self::Eval { value, .. } => smallvec![(*value, OperandRole::Read)],
            Self::HandlerEnter { .. }
            | Self::HandlerExit { .. }
            | Self::StageEnter { .. }
            | Self::StageExit { .. }
            | Self::Hole
            | Self::Error => SmallVec::new(),
        }
    }

    pub fn child_exprs(&self) -> SmallVec<[CfgExprId; 2]> {
        self.operands().into_iter().map(|(expr, _)| expr).collect()
    }

    /// The value this instruction defines, if any.
    pub fn result(&self) -> Option<CfgValueId> {
        match self {
            Self::Let { result, .. } | Self::Eval { result, .. } => Some(*result),
            Self::HandlerEnter { .. }
            | Self::HandlerExit { .. }
            | Self::StageEnter { .. }
            | Self::StageExit { .. }
            | Self::Hole
            | Self::Error => None,
        }
    }
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

    /// Every successor slot, for rewriting an edge in place.
    pub fn successors_mut(&mut self) -> SmallVec<[&mut CfgBlockId; 4]> {
        match self {
            Self::Return(_) | Self::Unreachable => SmallVec::new(),
            Self::Goto { target, .. }
            | Self::Call { target, .. }
            | Self::Perform { target, .. } => {
                smallvec![target]
            }
            Self::Branch {
                then_target,
                else_target,
                ..
            } => smallvec![then_target, else_target],
            Self::Match { arms, default, .. } => {
                let mut targets: SmallVec<[&mut CfgBlockId; 4]> =
                    arms.iter_mut().map(|arm| &mut arm.target).collect();
                targets.push(default);
                targets
            }
            Self::Switch {
                targets, default, ..
            } => {
                let mut slots: SmallVec<[&mut CfgBlockId; 4]> = targets.iter_mut().collect();
                slots.push(default);
                slots
            }
        }
    }

    /// Operands read by the terminator itself, with the role each one gets.
    ///
    /// Everything handed to a successor block, a callee, or the caller is
    /// `Owned`; a selector that only picks an edge is `Read`. Reference counting
    /// depends on that split, so it lives here rather than in the ARC pass.
    pub fn operands(&self) -> SmallVec<[(CfgExprId, OperandRole); 4]> {
        match self {
            Self::Unreachable => SmallVec::new(),
            Self::Return(value) => smallvec![(*value, OperandRole::Owned)],
            Self::Branch { cond: selector, .. }
            | Self::Match {
                scrutinee: selector,
                ..
            }
            | Self::Switch { selector, .. } => smallvec![(*selector, OperandRole::Read)],
            Self::Goto { args, .. } | Self::Call { args, .. } | Self::Perform { args, .. } => {
                args.iter().map(|arg| (*arg, OperandRole::Owned)).collect()
            }
        }
    }

    pub fn child_exprs(&self) -> SmallVec<[CfgExprId; 4]> {
        self.operands().into_iter().map(|(expr, _)| expr).collect()
    }

    /// Values the terminator makes available to its successors: a call or
    /// perform result, and the binders projected out on a match arm.
    pub fn defined_values(&self) -> SmallVec<[CfgValueId; 4]> {
        match self {
            Self::Return(_)
            | Self::Goto { .. }
            | Self::Branch { .. }
            | Self::Switch { .. }
            | Self::Unreachable => SmallVec::new(),
            Self::Call { result, .. } => smallvec![*result],
            Self::Perform { result, .. } => result.iter().copied().collect(),
            Self::Match { arms, .. } => arms
                .iter()
                .flat_map(|arm| arm.binders.iter().copied())
                .collect(),
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
