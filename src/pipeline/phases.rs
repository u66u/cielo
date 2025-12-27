use std::collections::HashMap;

use crate::common::densemap::DenseMap;
use crate::common::diagnostics::DiagnosticBag;
use crate::common::ids::{EffectLabelId, ExprId, FuncId, HandlerId, TypeId, VarId};
use crate::frontend::ast::Program as AstProgram;
use crate::ir::core::{CoreProgram, Literal};
use crate::sema::effect::{EffectProperties, SortedEffectRow};
use crate::sema::ty::Persistability;

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

#[derive(Clone, Debug, Default)]
pub struct SemanticTables {
    pub type_of_expr: Vec<Option<TypeId>>,
    pub effects_of_expr: Vec<SortedEffectRow>,
    pub effects_of_stmt: Vec<SortedEffectRow>,
    pub persistability_of_type: Vec<Persistability>,
    pub effect_properties: HashMap<EffectLabelId, EffectProperties>,
}

impl SemanticTables {
    pub fn with_counts(expr_count: usize, stmt_count: usize) -> Self {
        Self {
            type_of_expr: vec![None; expr_count],
            effects_of_expr: vec![SortedEffectRow::empty(); expr_count],
            effects_of_stmt: vec![SortedEffectRow::empty(); stmt_count],
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

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct CtFileDep {
    pub path: String,
    pub content_hash: String,
}

#[derive(Clone, PartialEq, Eq, Debug)]
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
    }
}

#[derive(Clone, Debug, Default)]
pub struct ResidualTables {
    pub function_effect_summary: HashMap<FuncId, SortedEffectRow>,
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

#[derive(Clone, Debug)]
pub struct Parsed {
    ast: AstProgram,
    diagnostics: DiagnosticBag,
}

impl Parsed {
    pub fn new(ast: AstProgram, diagnostics: DiagnosticBag) -> Self {
        Self { ast, diagnostics }
    }

    pub fn ast(&self) -> &AstProgram {
        &self.ast
    }

    pub fn diagnostics(&self) -> &DiagnosticBag {
        &self.diagnostics
    }

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

#[derive(Clone, Debug)]
pub struct CoreBuilt {
    program: CoreProgram,
    diagnostics: DiagnosticBag,
}

impl CoreBuilt {
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

    pub(crate) fn into_parts(self) -> (CoreProgram, DiagnosticBag) {
        (self.program, self.diagnostics)
    }

    pub fn into_typed(self, sema: SemanticTables) -> Typed {
        let (program, diagnostics) = self.into_parts();
        Typed::new(program, diagnostics, sema)
    }
}

#[derive(Clone, Debug)]
pub struct Typed {
    program: CoreProgram,
    diagnostics: DiagnosticBag,
    sema: SemanticTables,
}

impl Typed {
    pub fn new(program: CoreProgram, diagnostics: DiagnosticBag, sema: SemanticTables) -> Self {
        Self {
            program,
            diagnostics,
            sema,
        }
    }

    pub fn program(&self) -> &CoreProgram {
        &self.program
    }

    pub fn diagnostics(&self) -> &DiagnosticBag {
        &self.diagnostics
    }

    pub fn sema(&self) -> &SemanticTables {
        &self.sema
    }

    pub(crate) fn into_parts(self) -> (CoreProgram, DiagnosticBag, SemanticTables) {
        (self.program, self.diagnostics, self.sema)
    }

    pub fn into_monomorphized(self, mono: MonomorphizationSummary) -> Monomorphized {
        let (program, diagnostics, sema) = self.into_parts();
        Monomorphized::new(program, diagnostics, sema, mono)
    }
}

#[derive(Clone, Debug)]
pub struct Monomorphized {
    program: CoreProgram,
    diagnostics: DiagnosticBag,
    sema: SemanticTables,
    mono: MonomorphizationSummary,
}

impl Monomorphized {
    pub fn new(
        program: CoreProgram,
        diagnostics: DiagnosticBag,
        sema: SemanticTables,
        mono: MonomorphizationSummary,
    ) -> Self {
        Self {
            program,
            diagnostics,
            sema,
            mono,
        }
    }

    pub fn program(&self) -> &CoreProgram {
        &self.program
    }

    pub fn diagnostics(&self) -> &DiagnosticBag {
        &self.diagnostics
    }

    pub fn sema(&self) -> &SemanticTables {
        &self.sema
    }

    pub fn mono(&self) -> &MonomorphizationSummary {
        &self.mono
    }

    pub fn into_ct_propagated(self, ct: CtPropagationTables) -> CtPropagated {
        let (program, diagnostics, sema, mono) = self.into_parts();
        CtPropagated::new(program, diagnostics, sema, mono, ct)
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        CoreProgram,
        DiagnosticBag,
        SemanticTables,
        MonomorphizationSummary,
    ) {
        (self.program, self.diagnostics, self.sema, self.mono)
    }
}

#[derive(Clone, Debug)]
pub struct CtPropagated {
    program: CoreProgram,
    diagnostics: DiagnosticBag,
    sema: SemanticTables,
    mono: MonomorphizationSummary,
    ct: CtPropagationTables,
}

impl CtPropagated {
    pub fn new(
        program: CoreProgram,
        diagnostics: DiagnosticBag,
        sema: SemanticTables,
        mono: MonomorphizationSummary,
        ct: CtPropagationTables,
    ) -> Self {
        Self {
            program,
            diagnostics,
            sema,
            mono,
            ct,
        }
    }

    pub fn program(&self) -> &CoreProgram {
        &self.program
    }

    pub fn diagnostics(&self) -> &DiagnosticBag {
        &self.diagnostics
    }

    pub fn sema(&self) -> &SemanticTables {
        &self.sema
    }

    pub fn mono(&self) -> &MonomorphizationSummary {
        &self.mono
    }

    pub fn ct(&self) -> &CtPropagationTables {
        &self.ct
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        CoreProgram,
        DiagnosticBag,
        SemanticTables,
        MonomorphizationSummary,
        CtPropagationTables,
    ) {
        (
            self.program,
            self.diagnostics,
            self.sema,
            self.mono,
            self.ct,
        )
    }

    pub fn into_bta_classified(self, bta: BtaTables) -> BtaClassified {
        let (program, diagnostics, sema, mono, ct) = self.into_parts();
        BtaClassified::new(program, diagnostics, sema, mono, ct, bta)
    }
}

#[derive(Clone, Debug)]
pub struct BtaClassified {
    program: CoreProgram,
    diagnostics: DiagnosticBag,
    sema: SemanticTables,
    mono: MonomorphizationSummary,
    ct: CtPropagationTables,
    bta: BtaTables,
}

impl BtaClassified {
    pub fn new(
        program: CoreProgram,
        diagnostics: DiagnosticBag,
        sema: SemanticTables,
        mono: MonomorphizationSummary,
        ct: CtPropagationTables,
        bta: BtaTables,
    ) -> Self {
        Self {
            program,
            diagnostics,
            sema,
            mono,
            ct,
            bta,
        }
    }

    pub fn program(&self) -> &CoreProgram {
        &self.program
    }

    pub(crate) fn program_mut(&mut self) -> &mut CoreProgram {
        &mut self.program
    }

    pub fn diagnostics(&self) -> &DiagnosticBag {
        &self.diagnostics
    }

    pub fn sema(&self) -> &SemanticTables {
        &self.sema
    }

    pub fn mono(&self) -> &MonomorphizationSummary {
        &self.mono
    }

    pub fn ct(&self) -> &CtPropagationTables {
        &self.ct
    }

    pub fn bta(&self) -> &BtaTables {
        &self.bta
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        CoreProgram,
        DiagnosticBag,
        SemanticTables,
        MonomorphizationSummary,
        CtPropagationTables,
        BtaTables,
    ) {
        (
            self.program,
            self.diagnostics,
            self.sema,
            self.mono,
            self.ct,
            self.bta,
        )
    }

    pub fn into_residualized(self, residual: ResidualTables) -> Residualized {
        let (program, diagnostics, sema, mono, ct, bta) = self.into_parts();
        Residualized::new(program, diagnostics, sema, mono, ct, bta, residual)
    }
}

#[derive(Clone, Debug)]
pub struct Residualized {
    program: CoreProgram,
    diagnostics: DiagnosticBag,
    sema: SemanticTables,
    mono: MonomorphizationSummary,
    ct: CtPropagationTables,
    bta: BtaTables,
    residual: ResidualTables,
}

impl Residualized {
    pub fn new(
        program: CoreProgram,
        diagnostics: DiagnosticBag,
        sema: SemanticTables,
        mono: MonomorphizationSummary,
        ct: CtPropagationTables,
        bta: BtaTables,
        residual: ResidualTables,
    ) -> Self {
        Self {
            program,
            diagnostics,
            sema,
            mono,
            ct,
            bta,
            residual,
        }
    }

    pub fn program(&self) -> &CoreProgram {
        &self.program
    }

    pub fn diagnostics(&self) -> &DiagnosticBag {
        &self.diagnostics
    }

    pub(crate) fn program_and_diagnostics_mut(&mut self) -> (&CoreProgram, &mut DiagnosticBag) {
        (&self.program, &mut self.diagnostics)
    }

    pub fn sema(&self) -> &SemanticTables {
        &self.sema
    }

    pub fn mono(&self) -> &MonomorphizationSummary {
        &self.mono
    }

    pub fn ct(&self) -> &CtPropagationTables {
        &self.ct
    }

    pub fn bta(&self) -> &BtaTables {
        &self.bta
    }

    pub fn residual(&self) -> &ResidualTables {
        &self.residual
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        CoreProgram,
        DiagnosticBag,
        SemanticTables,
        MonomorphizationSummary,
        CtPropagationTables,
        BtaTables,
        ResidualTables,
    ) {
        (
            self.program,
            self.diagnostics,
            self.sema,
            self.mono,
            self.ct,
            self.bta,
            self.residual,
        )
    }
}
