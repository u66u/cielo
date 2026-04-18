//! Pure discovery and hashing for files observed by comptime effects.

use std::collections::HashSet;

use cielo_ir::core::{CoreProgram, ExprKind, Literal, StmtKind};
use cielo_ir::effect::{EffectFlags, EffectProperties};
use cielo_sema::SemanticTables;

use crate::pipeline::phases::CtFileDep;

pub fn discover(program: &CoreProgram, sema: &SemanticTables) -> Vec<String> {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();

    for stmt in program.stmts() {
        let StmtKind::Perform { effect, args, .. } = &stmt.kind else {
            continue;
        };
        let is_ct_only =
            sema.effect_properties
                .get(effect)
                .is_some_and(|properties: &EffectProperties| {
                    properties.flags.contains(EffectFlags::CT_ONLY)
                });
        if !is_ct_only {
            continue;
        }
        let Some(first_arg) = args.first() else {
            continue;
        };
        let Some(ExprKind::Literal(Literal::String(path))) =
            program.expr(*first_arg).map(|expr| &expr.kind)
        else {
            continue;
        };

        if seen.insert(path.clone()) {
            paths.push(path.clone());
        }
    }

    paths.sort();
    paths
}

pub fn snapshot(
    paths: impl IntoIterator<Item = String>,
    mut read: impl FnMut(&str) -> Option<Vec<u8>>,
) -> Vec<CtFileDep> {
    paths
        .into_iter()
        .map(|path| {
            let content_hash = read(&path)
                .map(|contents| blake3::hash(&contents).to_hex().to_string())
                .unwrap_or_else(|| "missing".to_owned());
            CtFileDep { path, content_hash }
        })
        .collect()
}
