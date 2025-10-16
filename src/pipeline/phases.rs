use std::collections::HashMap;

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
pub enum Reason {
    Parameter { func: FuncId, index: u16 },
    DependsOnVar(VarId),
    EffectNotDischarged(EffectLabelId),
    HandlerIsRuntime(HandlerId),
    BranchOnRuntime(ExprId),
    NotPersistable(TypeId),
    UserForcedRuntime,
    CtOnlyWithRuntimeArgs(FuncId),
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
    pub persistability_of_type: Vec<Persistability>,
    pub effect_properties: HashMap<EffectLabelId, EffectProperties>,
}

impl SemanticTables {
    pub fn with_expr_count(expr_count: usize) -> Self {
        Self {
            type_of_expr: vec![None; expr_count],
            effects_of_expr: vec![SortedEffectRow::empty(); expr_count],
            persistability_of_type: Vec::new(),
            effect_properties: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct MonomorphizationSummary {
    pub source_to_mono: HashMap<FuncId, Vec<FuncId>>,
}

#[derive(Clone, Debug, Default)]
pub struct CtPropagationTables {
    pub ct_cache: HashMap<ExprId, Literal>,
    pub branch_decisions: HashMap<ExprId, BranchDecision>,
    pub file_deps: Vec<(String, String)>,
}

#[derive(Clone, Debug, Default)]
pub struct BtaTables {
    pub stage_of_expr: HashMap<ExprId, Stage>,
    pub stage_of_var: HashMap<VarId, Stage>,
    pub handler_discharge: HashMap<HandlerId, HandlerDischarge>,
}

#[derive(Clone, Debug, Default)]
pub struct ResidualTables {
    pub function_effect_summary: HashMap<FuncId, SortedEffectRow>,
}

#[derive(Clone, Debug)]
pub struct Parsed {
    pub ast: AstProgram,
    pub diagnostics: DiagnosticBag,
}

#[derive(Clone, Debug)]
pub struct CoreBuilt {
    pub program: CoreProgram,
    pub diagnostics: DiagnosticBag,
}

#[derive(Clone, Debug)]
pub struct Typed {
    pub program: CoreProgram,
    pub diagnostics: DiagnosticBag,
    pub sema: SemanticTables,
}

#[derive(Clone, Debug)]
pub struct Monomorphized {
    pub program: CoreProgram,
    pub diagnostics: DiagnosticBag,
    pub sema: SemanticTables,
    pub mono: MonomorphizationSummary,
}

#[derive(Clone, Debug)]
pub struct CtPropagated {
    pub program: CoreProgram,
    pub diagnostics: DiagnosticBag,
    pub sema: SemanticTables,
    pub mono: MonomorphizationSummary,
    pub ct: CtPropagationTables,
}

#[derive(Clone, Debug)]
pub struct BtaClassified {
    pub program: CoreProgram,
    pub diagnostics: DiagnosticBag,
    pub sema: SemanticTables,
    pub mono: MonomorphizationSummary,
    pub ct: CtPropagationTables,
    pub bta: BtaTables,
}

#[derive(Clone, Debug)]
pub struct Residualized {
    pub program: CoreProgram,
    pub diagnostics: DiagnosticBag,
    pub sema: SemanticTables,
    pub mono: MonomorphizationSummary,
    pub ct: CtPropagationTables,
    pub bta: BtaTables,
    pub residual: ResidualTables,
}

impl CoreBuilt {
    pub fn into_typed(self, sema: SemanticTables) -> Typed {
        Typed {
            program: self.program,
            diagnostics: self.diagnostics,
            sema,
        }
    }
}

impl Parsed {
    pub fn into_core_built(
        self,
        program: CoreProgram,
        extra_diagnostics: DiagnosticBag,
    ) -> CoreBuilt {
        let mut diagnostics = self.diagnostics;
        diagnostics.extend(extra_diagnostics);
        CoreBuilt {
            program,
            diagnostics,
        }
    }
}

impl Typed {
    pub fn into_monomorphized(self, mono: MonomorphizationSummary) -> Monomorphized {
        Monomorphized {
            program: self.program,
            diagnostics: self.diagnostics,
            sema: self.sema,
            mono,
        }
    }
}

impl Monomorphized {
    pub fn into_ct_propagated(self, ct: CtPropagationTables) -> CtPropagated {
        CtPropagated {
            program: self.program,
            diagnostics: self.diagnostics,
            sema: self.sema,
            mono: self.mono,
            ct,
        }
    }
}

impl CtPropagated {
    pub fn into_bta_classified(self, bta: BtaTables) -> BtaClassified {
        BtaClassified {
            program: self.program,
            diagnostics: self.diagnostics,
            sema: self.sema,
            mono: self.mono,
            ct: self.ct,
            bta,
        }
    }
}

impl BtaClassified {
    pub fn into_residualized(self, residual: ResidualTables) -> Residualized {
        Residualized {
            program: self.program,
            diagnostics: self.diagnostics,
            sema: self.sema,
            mono: self.mono,
            ct: self.ct,
            bta: self.bta,
            residual,
        }
    }
}
