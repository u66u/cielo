//! Collector-specific classification of CFG values.

use cielo_base::{CfgExprId, CfgValueId};
use cielo_ir::cfg::{CfgExpr, CfgInstruction, CfgProgram, CfgTerminator};
use cielo_ir::runtime::RuntimeValueFacts;

pub fn classify(cfg: &CfgProgram, facts: &RuntimeValueFacts) -> Vec<bool> {
    let mut managed = cfg
        .values()
        .iter()
        .map(|value| facts.is_managed(value.id))
        .collect::<Vec<_>>();

    let mut changed = true;
    while changed {
        changed = false;
        for block in cfg.blocks() {
            for instruction in &block.instructions {
                let Some(instruction) = cfg.instruction(*instruction) else {
                    continue;
                };
                if let CfgInstruction::Let { result, value }
                | CfgInstruction::Eval { result, value } = instruction.kind
                    && expression_is_managed(cfg, value, &managed)
                    && !managed[result.index()]
                {
                    managed[result.index()] = true;
                    changed = true;
                }
            }
            match &block.terminator {
                CfgTerminator::Goto { target, args } => {
                    if let Some(target) = cfg.block(*target) {
                        for (parameter, argument) in target.params.iter().zip(args) {
                            if expression_is_managed(cfg, *argument, &managed)
                                && !managed[parameter.index()]
                            {
                                managed[parameter.index()] = true;
                                changed = true;
                            }
                        }
                    }
                }
                CfgTerminator::Match {
                    scrutinee, arms, ..
                } if expression_is_managed(cfg, *scrutinee, &managed) => {
                    for binder in arms.iter().flat_map(|arm| arm.binders.iter()) {
                        if !managed[binder.index()] {
                            managed[binder.index()] = true;
                            changed = true;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    managed
}

pub fn is_managed(managed: &[bool], value: CfgValueId) -> bool {
    managed.get(value.index()).copied().unwrap_or(false)
}

fn expression_is_managed(cfg: &CfgProgram, expression: CfgExprId, managed: &[bool]) -> bool {
    match &cfg.expr(expression).map(|expression| &expression.kind) {
        Some(CfgExpr::Value(value)) => is_managed(managed, *value),
        Some(CfgExpr::PureCall { .. })
        | Some(CfgExpr::MakeStruct { .. })
        | Some(CfgExpr::MakeEnum { .. }) => true,
        _ => false,
    }
}
