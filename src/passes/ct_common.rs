use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::common::densemap::DenseMap;
use crate::common::ids::{EffectLabelId, ExprId};
use crate::ir::core::{BinaryOp, CoreProgram, ExprKind, Literal, OpCategory, UnaryOp};
use crate::pipeline::compiler::{Endianness, TargetSpec};
use crate::pipeline::phases::{BranchDecision, CtCacheKey, CtEvalStats, CtFileDep, SemanticTables};
use crate::sema::effect::EffectFlags;

pub(super) const EVALUATOR_POLICY: &str = "v1-int-wrap-litnorm";

pub(super) fn assert_pre_staging_effects_concrete(program: &CoreProgram) {
    let effect_count = program.effects().len();
    let assert_effect = |effect: EffectLabelId, context: &str| {
        assert!(
            effect.is_valid() && effect.index() < effect_count,
            "compiler bug: unresolved/non-concrete effect at pre-staging boundary: {context} references e{} but only {effect_count} effect declarations exist",
            effect.as_u32()
        );
    };

    for (func_idx, function) in program.functions().iter().enumerate() {
        for effect in function.declared_effects.iter() {
            assert_effect(
                effect,
                format!("function f{func_idx} declared_effects").as_str(),
            );
        }
    }

    for (handler_idx, handler) in program.handlers().iter().enumerate() {
        assert_effect(
            handler.effect,
            format!("handler h{handler_idx} effect").as_str(),
        );
    }

    for (stmt_idx, stmt) in program.stmts().iter().enumerate() {
        match &stmt.kind {
            crate::ir::core::StmtKind::Call { effects, .. } => {
                for effect in effects.iter() {
                    assert_effect(effect, format!("stmt s{stmt_idx} call effect row").as_str());
                }
            }
            crate::ir::core::StmtKind::Perform { effect, .. } => {
                assert_effect(*effect, format!("stmt s{stmt_idx} perform effect").as_str());
            }
            _ => {}
        }
    }
}

pub(super) fn rebuild_branch_decisions(
    ct_cache: &DenseMap<ExprId, Literal>,
) -> DenseMap<ExprId, BranchDecision> {
    ct_cache
        .iter()
        .filter_map(|(expr_id, literal)| match literal {
            Literal::Bool(true) => Some((expr_id, BranchDecision::LiveTrue)),
            Literal::Bool(false) => Some((expr_id, BranchDecision::LiveFalse)),
            _ => None,
        })
        .collect()
}
