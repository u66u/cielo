use std::collections::HashMap;

use crate::analysis::borrow_hazard::BorrowHazardReport;
use crate::common::densemap::DenseMap;
use crate::common::diagnostics::DiagnosticBag;
use crate::common::ids::{EffectLabelId, ExprId, FuncId, HandlerId, SymbolId, TypeId, VarId};
use crate::frontend::ast::Program as AstProgram;
use crate::ir::core::{CoreProgram, Literal};
use crate::sema::effect::{EffectProperties, SortedEffectRow};
use crate::sema::ownership::OwnershipClass;
use crate::sema::ty::Persistability;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    Ct,
    Rt(Reason),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Knownness {
    Unknown,
    KnownLocal,
    KnownPersistable,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reason {
    UnclassifiedRuntime,
    Parameter { func: FuncId, index: u16 },
    DependsOnVar(VarId),
    EffectNotDischarged(EffectLabelId),
    HandlerIsRuntime(HandlerId),
    BranchOnRuntime(ExprId),
    NotPersistable(TypeId),
    UserForcedRuntime,
    CtOnlyWithRuntimeArgs(FuncId),
}

impl Reason {
    pub fn remap_func_ids(self, remap: &[Option<FuncId>]) -> Self {
        match self {
            Self::Parameter { func, index } => remap_func_id(remap, func)
                .map(|func| Self::Parameter { func, index })
                .unwrap_or(Self::UnclassifiedRuntime),
            Self::CtOnlyWithRuntimeArgs(func) => remap_func_id(remap, func)
                .map(Self::CtOnlyWithRuntimeArgs)
                .unwrap_or(Self::UnclassifiedRuntime),
            _ => self,
        }
    }

    pub fn stable_tag(self) -> String {
        match self {
            Self::UnclassifiedRuntime => "unclassified-runtime".to_owned(),
            Self::Parameter { func, index } => format!("param-f{}-{}", func.as_u32(), index),
            Self::DependsOnVar(var) => format!("depends-v{}", var.as_u32()),
            Self::EffectNotDischarged(effect) => format!("effect-e{}", effect.as_u32()),
            Self::HandlerIsRuntime(handler) => format!("handler-h{}", handler.as_u32()),
            Self::BranchOnRuntime(expr) => format!("branch-e{}", expr.as_u32()),
            Self::NotPersistable(ty) => format!("non-persistable-t{}", ty.as_u32()),
            Self::UserForcedRuntime => "forced-runtime".to_owned(),
            Self::CtOnlyWithRuntimeArgs(func) => format!("ct-only-f{}", func.as_u32()),
        }
    }

    pub fn describe_runtime(self) -> String {
        match self {
            Self::UnclassifiedRuntime => {
                "runtime classification has not been refined yet".to_owned()
            }
            Self::Parameter { func, index } => {
                format!("parameter #{} of f{} is runtime", index + 1, func.as_u32())
            }
            Self::DependsOnVar(var) => format!("depends on v{} which is runtime", var.as_u32()),
            Self::EffectNotDischarged(effect) => {
                format!("effect e{} is not thunkable/discharged", effect.as_u32())
            }
            Self::HandlerIsRuntime(handler) => format!("handler h{} is runtime", handler.as_u32()),
            Self::BranchOnRuntime(expr) => {
                format!("branch condition e{} is runtime", expr.as_u32())
            }
            Self::NotPersistable(ty) => format!("type t{} is not persistable", ty.as_u32()),
            Self::UserForcedRuntime => "explicitly marked @runtime".to_owned(),
            Self::CtOnlyWithRuntimeArgs(func) => {
                format!(
                    "ct-only function f{} was called with runtime args",
                    func.as_u32()
                )
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BranchDecision {
    LiveTrue,
    LiveFalse,
    Unknown,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HandlerDischarge {
    pub dischargeable: bool,
    pub reason: Option<Reason>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ClauseDischarge {
    pub dischargeable: bool,
    pub reason: Option<Reason>,
}

#[derive(Clone, Debug, Default)]
pub struct SemanticTables {
    pub type_of_expr: Vec<Option<TypeId>>,
    pub effects_of_expr: Vec<SortedEffectRow>,
    pub effects_of_stmt: Vec<SortedEffectRow>,
    pub ownership_of_expr: Vec<OwnershipClass>,
    pub ownership_of_var: DenseMap<VarId, OwnershipClass>,
    pub ownership_of_type: Vec<OwnershipClass>,
    pub persistability_of_type: Vec<Persistability>,
    pub effect_properties: HashMap<EffectLabelId, EffectProperties>,
}

impl SemanticTables {
    pub fn with_counts(expr_count: usize, stmt_count: usize) -> Self {
        Self {
            type_of_expr: vec![None; expr_count],
            effects_of_expr: vec![SortedEffectRow::empty(); expr_count],
            effects_of_stmt: vec![SortedEffectRow::empty(); stmt_count],
            ownership_of_expr: vec![OwnershipClass::BorrowedView; expr_count],
            ownership_of_var: DenseMap::default(),
            ownership_of_type: Vec::new(),
            persistability_of_type: Vec::new(),
            effect_properties: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct MonomorphizationSummary {
    pub source_to_mono: HashMap<FuncId, Vec<FuncId>>,
}

impl MonomorphizationSummary {
    pub fn remap_func_ids(&mut self, remap: &[Option<FuncId>]) {
        let source_to_mono = std::mem::take(&mut self.source_to_mono);
        for (source, monos) in source_to_mono {
            let Some(source) = remap_func_id(remap, source) else {
                continue;
            };
            let mut mapped = monos
                .into_iter()
                .filter_map(|mono| remap_func_id(remap, mono))
                .collect::<Vec<_>>();
            if mapped.is_empty() {
                continue;
            }
            mapped.sort_by_key(|id| id.index());
            mapped.dedup();
            self.source_to_mono.insert(source, mapped);
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct CtFileDep {
    pub path: String,
    pub content_hash: String,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CtCacheKey {
    pub target_word_size_bits: u8,
    pub target_endianness: String,
    pub target_pointer_alignment: u8,
    pub evaluator_policy: String,
    pub compiler_version: String,
}

impl Default for CtCacheKey {
    fn default() -> Self {
        Self {
            target_word_size_bits: 64,
            target_endianness: "little".to_owned(),
            target_pointer_alignment: 8,
            evaluator_policy: "v1-int-wrap-litnorm".to_owned(),
            compiler_version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CtEvalStats {
    pub iterations: u32,
    pub eval_attempts: u32,
    pub cache_hits: u32,
    pub cache_inserts: u32,
    pub folded_literals: u32,
    pub folded_unary: u32,
    pub folded_binary: u32,
    pub folded_float_host: u32,
    pub miss_missing_inputs: u32,
    pub miss_unsupported: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ResidualizeStats {
    pub embedded_literals: u32,
    pub pruned_if_branches: u32,
    pub pruned_match_branches: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct SpecializationStats {
    pub candidates_seen: u32,
    pub created: u32,
    pub reused_existing: u32,
    pub rewrites: u32,
    pub skipped_varying_shapes: u32,
    pub skipped_limits: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ArcStats {
    pub planned_retain_ops: u32,
    pub planned_release_ops: u32,
    pub eliminated_move_pairs: u32,
    pub removed_retain_ops: u32,
    pub removed_release_ops: u32,
    pub final_retain_ops: u32,
    pub final_release_ops: u32,
}

#[derive(Clone, Debug, Default)]
pub struct CtPropagationTables {
    pub ct_cache: DenseMap<ExprId, Literal>,
    pub branch_decisions: DenseMap<ExprId, BranchDecision>,
    pub file_deps: Vec<CtFileDep>,
    pub cache_key: CtCacheKey,
    pub eval_stats: CtEvalStats,
}

#[derive(Clone, Debug, Default)]
pub struct BtaTables {
    pub stage_of_expr: DenseMap<ExprId, Stage>,
    pub stage_of_var: DenseMap<VarId, Stage>,
    pub knownness_of_expr: DenseMap<ExprId, Knownness>,
    pub handler_discharge: DenseMap<HandlerId, HandlerDischarge>,
    pub clause_discharge: DenseMap<HandlerId, Vec<ClauseDischarge>>,
}

impl BtaTables {
    pub fn remap_func_ids(&mut self, remap: &[Option<FuncId>]) {
        for stage in self.stage_of_expr.values_mut() {
            remap_stage_reason(stage, remap);
        }
        for stage in self.stage_of_var.values_mut() {
            remap_stage_reason(stage, remap);
        }
        for discharge in self.handler_discharge.values_mut() {
            if let Some(reason) = discharge.reason {
                discharge.reason = Some(reason.remap_func_ids(remap));
            }
        }
        for clauses in self.clause_discharge.values_mut() {
            for clause in clauses {
                if let Some(reason) = clause.reason {
                    clause.reason = Some(reason.remap_func_ids(remap));
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum ScalarLiteralKey {
    Bool(bool),
    Int(i64),
    Char(char),
    Float(u64),
}

impl ScalarLiteralKey {
    pub fn from_literal(literal: &Literal) -> Option<Self> {
        match literal {
            Literal::Bool(value) => Some(Self::Bool(*value)),
            Literal::Int(value) => Some(Self::Int(*value)),
            Literal::Char(value) => Some(Self::Char(*value)),
            Literal::Float(value) if value.is_finite() => Some(Self::Float(value.to_bits())),
            Literal::Unit | Literal::Float(_) | Literal::String(_) => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum CtorFieldKey {
    Unit,
    Bool(bool),
    Int(i64),
    Char(char),
    Float(u64),
    String(String),
    Ctor(Box<CtorLiteralKey>),
}

impl CtorFieldKey {
    pub fn from_literal(literal: &Literal) -> Option<Self> {
        match literal {
            Literal::Unit => Some(Self::Unit),
            Literal::Bool(value) => Some(Self::Bool(*value)),
            Literal::Int(value) => Some(Self::Int(*value)),
            Literal::Char(value) => Some(Self::Char(*value)),
            Literal::Float(value) if value.is_finite() => Some(Self::Float(value.to_bits())),
            Literal::String(value) => Some(Self::String(value.clone())),
            Literal::Float(_) => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CtorLiteralKey {
    pub ty: SymbolId,
    pub variant: SymbolId,
    pub fields: Vec<CtorFieldKey>,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum ConstantKey {
    Scalar(ScalarLiteralKey),
    String(String),
    Ctor(CtorLiteralKey),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConstantEmbedStrategy {
    StaticConst,
    Pooled,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ConstantEntry {
    pub key: ConstantKey,
    pub strategy: ConstantEmbedStrategy,
    pub estimated_size_bytes: usize,
}

#[derive(Clone, Debug, Default)]
pub struct ConstantTable {
    pub entries: Vec<ConstantEntry>,
    pub entry_cap_bytes: usize,
    pub unit_cap_bytes: usize,
    pub total_size_bytes: usize,
}

#[derive(Clone, Debug, Default)]
pub struct ResidualTables {
    pub function_effect_summary: HashMap<FuncId, SortedEffectRow>,
    pub constant_table: ConstantTable,
    pub residualize_stats: ResidualizeStats,
    pub specialization_stats: SpecializationStats,
    pub arc_stats: ArcStats,
    pub borrow_hazards: BorrowHazardReport,
}

impl ResidualTables {
    pub fn remap_func_ids(&mut self, remap: &[Option<FuncId>]) {
        let summary = std::mem::take(&mut self.function_effect_summary);
        for (source, effects) in summary {
            let Some(mapped) = remap_func_id(remap, source) else {
                continue;
            };
            self.function_effect_summary.insert(mapped, effects);
        }
    }
}

fn remap_stage_reason(stage: &mut Stage, remap: &[Option<FuncId>]) {
    if let Stage::Rt(reason) = stage {
        *stage = Stage::Rt(reason.remap_func_ids(remap));
    }
}

fn remap_func_id(remap: &[Option<FuncId>], source: FuncId) -> Option<FuncId> {
    remap.get(source.index()).copied().flatten()
}

/// Easy definition of compiler phases
macro_rules! define_phase_state {
    ($name:ident { $($field:ident: $ty:ty),+ $(,)? }) => {
        #[derive(Clone, Debug)]
        pub struct $name {
            $( $field: $ty, )+
        }

        impl $name {
            pub fn new($($field: $ty),+) -> Self {
                Self { $($field),+ }
            }

            $(
                pub fn $field(&self) -> &$ty {
                    &self.$field
                }
            )+
        }
    };
}

macro_rules! define_phase_state_with_parts {
    ($name:ident { $($field:ident: $ty:ty),+ $(,)? }) => {
        define_phase_state!($name { $($field: $ty),+ });

        impl $name {
            pub(crate) fn into_parts(self) -> ($($ty),+) {
                ($(self.$field),+)
            }
        }
    };
}

define_phase_state!(Parsed {
    ast: AstProgram,
    diagnostics: DiagnosticBag,
});

impl Parsed {
    pub fn into_core_built(
        self,
        program: CoreProgram,
        extra_diagnostics: DiagnosticBag,
    ) -> CoreBuilt {
        let mut diagnostics = self.diagnostics;
        diagnostics.extend(extra_diagnostics);
        CoreBuilt::new(program, diagnostics)
    }
}

define_phase_state_with_parts!(CoreBuilt {
    program: CoreProgram,
    diagnostics: DiagnosticBag,
});

impl CoreBuilt {
    pub fn into_typed(self, sema: SemanticTables) -> Typed {
        let (program, diagnostics) = self.into_parts();
        Typed::new(program, diagnostics, sema)
    }
}

define_phase_state_with_parts!(Typed {
    program: CoreProgram,
    diagnostics: DiagnosticBag,
    sema: SemanticTables,
});

impl Typed {
    pub fn into_monomorphized(self, mono: MonomorphizationSummary) -> Monomorphized {
        let (program, diagnostics, sema) = self.into_parts();
        Monomorphized::new(program, diagnostics, sema, mono)
    }
}

define_phase_state_with_parts!(Monomorphized {
    program: CoreProgram,
    diagnostics: DiagnosticBag,
    sema: SemanticTables,
    mono: MonomorphizationSummary,
});

impl Monomorphized {
    pub fn into_ct_propagated(self, ct: CtPropagationTables) -> CtPropagated {
        let (program, diagnostics, sema, mono) = self.into_parts();
        CtPropagated::new(program, diagnostics, sema, mono, ct)
    }
}

define_phase_state_with_parts!(CtPropagated {
    program: CoreProgram,
    diagnostics: DiagnosticBag,
    sema: SemanticTables,
    mono: MonomorphizationSummary,
    ct: CtPropagationTables,
});

impl CtPropagated {
    pub fn into_bta_classified(self, bta: BtaTables) -> BtaClassified {
        let (program, diagnostics, sema, mono, ct) = self.into_parts();
        BtaClassified::new(program, diagnostics, sema, mono, ct, bta)
    }
}

define_phase_state_with_parts!(BtaClassified {
    program: CoreProgram,
    diagnostics: DiagnosticBag,
    sema: SemanticTables,
    mono: MonomorphizationSummary,
    ct: CtPropagationTables,
    bta: BtaTables,
});

impl BtaClassified {
    pub(crate) fn program_mut(&mut self) -> &mut CoreProgram {
        &mut self.program
    }

    pub fn into_residualized(self, residual: ResidualTables) -> Residualized {
        let (program, diagnostics, sema, mono, ct, bta) = self.into_parts();
        Residualized::new(program, diagnostics, sema, mono, ct, bta, residual)
    }
}

define_phase_state_with_parts!(Residualized {
    program: CoreProgram,
    diagnostics: DiagnosticBag,
    sema: SemanticTables,
    mono: MonomorphizationSummary,
    ct: CtPropagationTables,
    bta: BtaTables,
    residual: ResidualTables,
});

impl Residualized {
    pub(crate) fn program_and_diagnostics_mut(&mut self) -> (&CoreProgram, &mut DiagnosticBag) {
        (&self.program, &mut self.diagnostics)
    }

    pub(crate) fn residual_mut(&mut self) -> &mut ResidualTables {
        &mut self.residual
    }
}
