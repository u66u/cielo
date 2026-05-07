//! Block-form control-flow IR.
//!
//! This IR is deliberately separate from `linear`: CFG node identifiers cannot
//! accidentally be used to index a linear arena. Source variables are preserved
//! as optional provenance while synthetic block values are free to model
//! continuations and cleanup paths.
//!
//! Values are single-assignment: `validate` rejects a value two instructions
//! write. They are not in strict SSA form, and dominance is the wrong question
//! to ask of them — a value can be the parameter of several sibling
//! continuations that share a successor, defined on every path in without any
//! one definition dominating the read. `validate` checks definedness on every
//! path instead.

use std::collections::{HashMap, HashSet};

use crate::builtins::Builtin;
use crate::constants::{CtorFieldKey, CtorLiteralKey};
use crate::core::{BinaryOp, Literal, StageDirective, UnaryOp};
use crate::ownership::OperandRole;
use crate::region::{CfgRegion, RegionOwner, RegionSlot};
use cielo_base::{
    CfgBlockId, CfgExprId, CfgFuncId, CfgHandlerId, CfgInstId, CfgRegionId, CfgValueId,
    EffectLabelId, LinearExprId, LinearStmtId, SymbolId, VarId,
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
    regions: Vec<CfgRegion>,
}

impl CfgProgram {
    pub fn push_region(&mut self, owner: RegionOwner, slots: Vec<RegionSlot>) -> CfgRegionId {
        let id = CfgRegionId::new(self.regions.len());
        self.regions.push(CfgRegion { id, owner, slots });
        id
    }

    pub fn region(&self, id: CfgRegionId) -> Option<&CfgRegion> {
        self.regions.get(id.index())
    }

    pub fn region_mut(&mut self, id: CfgRegionId) -> Option<&mut CfgRegion> {
        self.regions.get_mut(id.index())
    }

    pub fn regions(&self) -> &[CfgRegion] {
        &self.regions
    }

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

    /// Checks arena references, successor references, continuation arity, that
    /// no value is assigned by two instructions, and that every value an
    /// expression reads is defined on the way in.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();
        self.check_single_assignment(&mut errors);
        self.check_value_definitions(&mut errors);
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
                let Some(node) = self.instruction(*instruction) else {
                    errors.push(format!(
                        "block {} has invalid instruction {}",
                        block.id, instruction
                    ));
                    continue;
                };
                // A dangling region id would make the escape analysis skip the
                // region and the backend emit a slot with no scope to free it.
                if let CfgInstruction::RegionEnter { region }
                | CfgInstruction::RegionExit { region } = node.kind
                    && self.region(region).is_none()
                {
                    errors.push(format!(
                        "block {} references unknown region {}",
                        block.id, region
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

    /// Reports a value that two instructions assign.
    ///
    /// Handler inlining re-lowers a Core subtree once per resume site per
    /// perform, and every copy of a Core variable used to collapse onto one
    /// `CfgValueId`: nineteen `Let`s writing one value on a two-site handler.
    /// Every analysis that reasons per definition — `UniquenessQuery`, retain
    /// counts derived from use counts — silently merged them (CIELO-47).
    ///
    /// Instruction results only. A parameter is assigned by predecessors, and
    /// the ARC pass deliberately gives an edge block its successor's parameters
    /// (CIELO-48), so a value parameterising two blocks is one definition.
    fn check_single_assignment(&self, errors: &mut Vec<String>) {
        let mut assigned_by: HashMap<CfgValueId, CfgInstId> = HashMap::new();
        for block in &self.blocks {
            for instruction in &block.instructions {
                let Some(node) = self.instruction(*instruction) else {
                    continue;
                };
                let Some(result) = node.kind.result() else {
                    continue;
                };
                if let Some(previous) = assigned_by.insert(result, node.id) {
                    errors.push(format!(
                        "value {result} is assigned by both instruction {previous} and {}",
                        node.id
                    ));
                }
            }
        }
    }

    /// Reports reads of a value that some path into the block leaves undefined.
    ///
    /// A pass that binds a subexpression to a fresh value has to put the
    /// binding in the block that reads it. Caching such a binding across blocks
    /// yields a graph that still passes every structural check above and still
    /// emits, but reads an uninitialised local on the path that skips the
    /// definition.
    ///
    /// Definedness, not dominance: one `CfgValueId` is deliberately the
    /// parameter of several sibling continuations that share a `next` block, so
    /// a value can be defined on every path in without any single definition
    /// dominating the read.
    fn check_value_definitions(&self, errors: &mut Vec<String>) {
        for function in &self.functions {
            for (block, incoming) in self.definitions_on_entry(function.entry) {
                let Some(node) = self.block(block) else {
                    continue;
                };
                let mut defined = incoming;
                defined.extend(node.params.iter().copied());
                for instruction in &node.instructions {
                    let Some(instruction) = self.instruction(*instruction) else {
                        continue;
                    };
                    if let CfgInstruction::Let { result, value }
                    | CfgInstruction::Eval { result, value } = instruction.kind
                    {
                        self.check_expr_reads(value, &defined, block, errors);
                        defined.insert(result);
                    }
                }
                for operand in node.terminator.child_exprs() {
                    self.check_expr_reads(operand, &defined, block, errors);
                }
            }
        }
    }

    fn check_expr_reads(
        &self,
        expression: CfgExprId,
        defined: &HashSet<CfgValueId>,
        block: CfgBlockId,
        errors: &mut Vec<String>,
    ) {
        let Some(node) = self.expr(expression) else {
            return;
        };
        match &node.kind {
            CfgExpr::Value(value) => {
                if !defined.contains(value) {
                    errors.push(format!(
                        "block {block} reads {value}, which some path into it leaves undefined"
                    ));
                }
            }
            CfgExpr::Unary { expr, .. } | CfgExpr::Field { base: expr, .. } => {
                self.check_expr_reads(*expr, defined, block, errors);
            }
            CfgExpr::Binary { lhs, rhs, .. } => {
                self.check_expr_reads(*lhs, defined, block, errors);
                self.check_expr_reads(*rhs, defined, block, errors);
            }
            CfgExpr::PureCall { args, .. }
            | CfgExpr::BuiltinCall { args, .. }
            | CfgExpr::MakeStruct { fields: args, .. }
            | CfgExpr::MakeEnum { fields: args, .. } => {
                for arg in args {
                    self.check_expr_reads(*arg, defined, block, errors);
                }
            }
            CfgExpr::Literal(_) | CfgExpr::Error => {}
        }
    }

    /// Every value `block` itself defines: its parameters, which a `Call`,
    /// `Perform` or match arm edge also fills in, plus its instruction results.
    fn definitions_in(&self, block: CfgBlockId) -> Vec<CfgValueId> {
        let Some(node) = self.block(block) else {
            return Vec::new();
        };
        let mut defined = node.params.clone();
        for instruction in &node.instructions {
            if let Some(CfgInstruction::Let { result, .. } | CfgInstruction::Eval { result, .. }) =
                self.instruction(*instruction).map(|node| &node.kind)
            {
                defined.push(*result);
            }
        }
        defined
    }

    /// For each block reachable from `entry`, the values every path into it has
    /// already defined. Unreachable blocks are absent: nothing emits them, so
    /// nothing in them can be read.
    ///
    /// A "must" fixpoint, so the non-entry blocks start optimistic at the full
    /// set and shrink. Starting them empty would instead make a value defined
    /// only inside a cycle look undefined forever.
    fn definitions_on_entry(&self, entry: CfgBlockId) -> Vec<(CfgBlockId, HashSet<CfgValueId>)> {
        let mut reachable = vec![entry];
        let mut seen = HashSet::from([entry]);
        let mut index = 0;
        while index < reachable.len() {
            let block = reachable[index];
            index += 1;
            let Some(node) = self.block(block) else {
                continue;
            };
            for successor in node.terminator.successors() {
                if self.block(successor).is_some() && seen.insert(successor) {
                    reachable.push(successor);
                }
            }
        }

        let mut predecessors: HashMap<CfgBlockId, Vec<CfgBlockId>> = HashMap::new();
        let mut produced: HashMap<CfgBlockId, Vec<CfgValueId>> = HashMap::new();
        for block in &reachable {
            produced.insert(*block, self.definitions_in(*block));
            let Some(node) = self.block(*block) else {
                continue;
            };
            for successor in node.terminator.successors() {
                if seen.contains(&successor) {
                    predecessors.entry(successor).or_default().push(*block);
                }
            }
        }

        let universe = produced.values().flatten().copied().collect::<HashSet<_>>();
        let mut on_exit = reachable
            .iter()
            .map(|block| {
                let set = if *block == entry {
                    produced[block].iter().copied().collect()
                } else {
                    universe.clone()
                };
                (*block, set)
            })
            .collect::<HashMap<_, HashSet<_>>>();

        let mut changed = true;
        while changed {
            changed = false;
            for block in &reachable {
                if *block == entry {
                    continue;
                }
                let mut incoming: Option<HashSet<CfgValueId>> = None;
                for predecessor in predecessors.get(block).map(Vec::as_slice).unwrap_or(&[]) {
                    let available = &on_exit[predecessor];
                    incoming = Some(match incoming {
                        Some(current) => current.intersection(available).copied().collect(),
                        None => available.clone(),
                    });
                }
                let mut next = incoming.unwrap_or_default();
                next.extend(produced[block].iter().copied());
                if next != on_exit[block] {
                    on_exit.insert(*block, next);
                    changed = true;
                }
            }
        }

        reachable
            .into_iter()
            .map(|block| {
                let incoming = predecessors
                    .get(&block)
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                    .iter()
                    .map(|predecessor| &on_exit[predecessor])
                    .cloned()
                    .reduce(|current, available| {
                        current.intersection(&available).copied().collect()
                    })
                    .unwrap_or_default();
                (block, incoming)
            })
            .collect()
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
            Self::BuiltinCall { args: operands, .. }
            | Self::PureCall { args: operands, .. }
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
            Self::BuiltinCall { .. } => "builtin",
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
            // The name, not the discriminant: fingerprints reach snapshot
            // files, so reordering the enum must not change them.
            Self::BuiltinCall { builtin, .. } => builtin.name().hash(hasher),
            Self::MakeStruct { ty, .. } => ty.as_u32().hash(hasher),
            Self::MakeEnum { ty, variant, .. } => {
                ty.as_u32().hash(hasher);
                variant.as_u32().hash(hasher);
            }
            Self::Error => {}
        }
    }
}

/// The constant-pool key a constructor expression hashes to, or `None` when a
/// field is not a literal. `None` is the interesting answer for memory
/// analyses: the backend cannot pool such a constructor, so it must emit a
/// fresh `cielo_make_ctor` allocation.
pub fn ctor_literal_key(
    program: &CfgProgram,
    ty: SymbolId,
    variant: SymbolId,
    fields: &[CfgExprId],
) -> Option<CtorLiteralKey> {
    let fields = fields
        .iter()
        .map(|field| ctor_field_key(program, *field))
        .collect::<Option<Vec<_>>>()?;
    Some(CtorLiteralKey {
        ty,
        variant,
        fields,
    })
}

fn ctor_field_key(program: &CfgProgram, expression: CfgExprId) -> Option<CtorFieldKey> {
    match &program.expr(expression)?.kind {
        CfgExpr::Literal(literal) => CtorFieldKey::from_literal(literal),
        CfgExpr::MakeStruct { ty, fields } => Some(CtorFieldKey::Ctor(Box::new(ctor_literal_key(
            program,
            *ty,
            SymbolId::INVALID,
            fields,
        )?))),
        CfgExpr::MakeEnum {
            ty,
            variant,
            fields,
        } => Some(CtorFieldKey::Ctor(Box::new(ctor_literal_key(
            program, *ty, *variant, fields,
        )?))),
        CfgExpr::Value(_)
        | CfgExpr::Unary { .. }
        | CfgExpr::Binary { .. }
        | CfgExpr::PureCall { .. }
        | CfgExpr::BuiltinCall { .. }
        | CfgExpr::Field { .. }
        | CfgExpr::Error => None,
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
    /// Opens the allocation scope its slots live in. Separate from
    /// `HandlerEnter` because a continuation or closure environment opens a
    /// region with no handler to enter.
    RegionEnter {
        region: CfgRegionId,
    },
    RegionExit {
        region: CfgRegionId,
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
            | Self::RegionEnter { .. }
            | Self::RegionExit { .. }
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
            | Self::RegionEnter { .. }
            | Self::RegionExit { .. }
            | Self::StageEnter { .. }
            | Self::StageExit { .. }
            | Self::Hole
            | Self::Error => None,
        }
    }
}

impl CfgExpr {
    /// Whether evaluating this hands back a reference the evaluator now owns.
    ///
    /// ARC keys every op on a `CfgValueId`, so an owning expression that is not
    /// bound to one has no name a release can mention and leaks. Lowering uses
    /// this to decide what to materialise, which makes the `false` arms the
    /// dangerous ones: a new variant belongs on the `true` side unless the
    /// runtime helper it emits provably yields a scalar or a borrow. Being
    /// wrong the other way only costs a temporary, since retain and release are
    /// no-ops on an unmanaged tag.
    pub fn produces_owned(&self) -> bool {
        match self {
            // `cielo_ctor_field_copy` retains, so a projection is owned even
            // though it allocates nothing.
            Self::Field { .. }
            | Self::PureCall { .. }
            | Self::BuiltinCall { .. }
            | Self::MakeStruct { .. }
            | Self::MakeEnum { .. } => true,
            // `Unary` and `Binary` are the `cv_*` scalar helpers, and literals
            // are immortal statics.
            Self::Value(_) | Self::Literal(_) | Self::Unary { .. } | Self::Binary { .. } => false,
            Self::Error => false,
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
    /// Destructive take gated on a runtime `rc == 1 && !immortal` test.
    Move,
    /// Destructive take whose gate the uniqueness query discharged statically.
    MoveUnique,
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
