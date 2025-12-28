use std::collections::BTreeMap;
use std::fmt::Write;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::Path;

use crate::common::ids::ExprId;
use crate::ir::core::{CoreProgram, ExprKind};
use crate::pipeline::phases::{BtaTables, Reason, Stage};

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StageSnapshotEntry {
    pub stable_id: String,
    pub stage: SnapshotStage,
    pub top_reason: String,
    pub cause_hash: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SnapshotStage {
    Ct,
    Rt,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StageDiff {
    pub stable_id: String,
    pub before: SnapshotStage,
    pub after: SnapshotStage,
    pub before_reason: String,
    pub after_reason: String,
}

pub fn collect_snapshot(program: &CoreProgram, bta: &BtaTables) -> Vec<StageSnapshotEntry> {
    let fingerprints = collect_expr_fingerprints(program);
    let mut entries = Vec::new();
    for (idx, expr) in program.exprs().iter().enumerate() {
        let expr_id = ExprId::new(idx);
        let Some(stage) = bta.stage_of_expr.get(&expr_id).copied() else {
            continue;
        };
        let stage_tag = match stage {
            Stage::Ct => SnapshotStage::Ct,
            Stage::Rt(_) => SnapshotStage::Rt,
        };
        let top_reason = match stage {
            Stage::Ct => String::new(),
            Stage::Rt(reason) => reason_text(reason),
        };
        let stable_id = stable_expr_id(
            expr.span.start,
            expr.span.end,
            &expr.kind,
            fingerprints.get(idx).copied().unwrap_or_default(),
        );
        entries.push(StageSnapshotEntry {
            stable_id,
            stage: stage_tag,
            cause_hash: hash_reason(top_reason.as_str()),
            top_reason,
        });
    }
    entries.sort_by(|lhs, rhs| lhs.stable_id.cmp(&rhs.stable_id));
    entries
}

pub fn diff_snapshots(
    previous: &[StageSnapshotEntry],
    current: &[StageSnapshotEntry],
) -> Vec<StageDiff> {
    let previous_map = as_map(previous);
    let current_map = as_map(current);

    let mut diffs = Vec::new();
    for (stable_id, before) in previous_map {
        let Some(after) = current_map.get(&stable_id) else {
            continue;
        };
        if before.stage == after.stage && before.cause_hash == after.cause_hash {
            continue;
        }
        diffs.push(StageDiff {
            stable_id,
            before: before.stage,
            after: after.stage,
            before_reason: before.top_reason.clone(),
            after_reason: after.top_reason.clone(),
        });
    }
    diffs.sort_by(|lhs, rhs| lhs.stable_id.cmp(&rhs.stable_id));
    diffs
}

pub fn load_snapshot(path: &Path) -> io::Result<Vec<StageSnapshotEntry>> {
    let text = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for line in text.lines() {
        let parts = line.splitn(4, '\t').collect::<Vec<_>>();
        if parts.len() != 4 {
            continue;
        }
        let stage = match parts[1] {
            "ct" => SnapshotStage::Ct,
            "rt" => SnapshotStage::Rt,
            _ => continue,
        };
        let cause_hash = parts[2].parse::<u64>().unwrap_or_default();
        out.push(StageSnapshotEntry {
            stable_id: parts[0].to_owned(),
            stage,
            cause_hash,
            top_reason: parts[3].to_owned(),
        });
    }
    Ok(out)
}

pub fn save_snapshot(path: &Path, entries: &[StageSnapshotEntry]) -> io::Result<()> {
    let mut text = String::new();
    for entry in entries {
        let stage = match entry.stage {
            SnapshotStage::Ct => "ct",
            SnapshotStage::Rt => "rt",
        };
        writeln!(
            text,
            "{}\t{}\t{}\t{}",
            entry.stable_id, stage, entry.cause_hash, entry.top_reason
        )
        .expect("in-memory write should not fail");
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text)
}

fn as_map(entries: &[StageSnapshotEntry]) -> BTreeMap<String, StageSnapshotEntry> {
    entries
        .iter()
        .cloned()
        .map(|entry| (entry.stable_id.clone(), entry))
        .collect()
}

fn stable_expr_id(start: u32, end: u32, kind: &ExprKind, fingerprint: u64) -> String {
    format!(
        "{}-{}:{}:{:016x}",
        start,
        end,
        expr_kind_tag(kind),
        fingerprint
    )
}

fn expr_kind_tag(kind: &ExprKind) -> &'static str {
    match kind {
        ExprKind::Literal(_) => "lit",
        ExprKind::Var(_) => "var",
        ExprKind::Unary { .. } => "un",
        ExprKind::Binary { .. } => "bin",
        ExprKind::PureCall { .. } => "call",
        ExprKind::MakeStruct { .. } => "mk_struct",
        ExprKind::MakeEnum { .. } => "mk_enum",
        ExprKind::Error(_) => "err",
    }
}

fn hash_reason(reason: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    reason.hash(&mut hasher);
    hasher.finish()
}

fn reason_text(reason: Reason) -> String {
    match reason {
        Reason::UnclassifiedRuntime => "unclassified-runtime".to_owned(),
        Reason::Parameter { func, index } => format!("param-f{}-{}", func.as_u32(), index),
        Reason::DependsOnVar(var) => format!("depends-v{}", var.as_u32()),
        Reason::EffectNotDischarged(effect) => format!("effect-e{}", effect.as_u32()),
        Reason::HandlerIsRuntime(handler) => format!("handler-h{}", handler.as_u32()),
        Reason::BranchOnRuntime(expr) => format!("branch-e{}", expr.as_u32()),
        Reason::NotPersistable(ty) => format!("non-persistable-t{}", ty.as_u32()),
        Reason::UserForcedRuntime => "forced-runtime".to_owned(),
        Reason::CtOnlyWithRuntimeArgs(func) => format!("ct-only-f{}", func.as_u32()),
    }
}

fn collect_expr_fingerprints(program: &CoreProgram) -> Vec<u64> {
    let mut memo = vec![None; program.exprs().len()];
    let mut visiting = vec![false; program.exprs().len()];
    for idx in 0..program.exprs().len() {
        let expr_id = ExprId::new(idx);
        let _ = expr_fingerprint(program, expr_id, &mut memo, &mut visiting);
    }
    memo.into_iter().map(|entry| entry.unwrap_or_default()).collect()
}

fn expr_fingerprint(
    program: &CoreProgram,
    expr_id: ExprId,
    memo: &mut [Option<u64>],
    visiting: &mut [bool],
) -> u64 {
    if let Some(value) = memo.get(expr_id.index()).copied().flatten() {
        return value;
    }
    if visiting.get(expr_id.index()).copied().unwrap_or(false) {
        return 0;
    }
    if expr_id.index() >= visiting.len() || expr_id.index() >= memo.len() {
        return 0;
    }
    visiting[expr_id.index()] = true;

    let value = if let Some(expr) = program.expr(expr_id) {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        expr.span.start.hash(&mut hasher);
        expr.span.end.hash(&mut hasher);
        expr_kind_tag(&expr.kind).hash(&mut hasher);
        match &expr.kind {
            ExprKind::Var(var) => var.as_u32().hash(&mut hasher),
            ExprKind::Literal(literal) => hash_literal(literal, &mut hasher),
            ExprKind::Unary { op, expr } => {
                std::mem::discriminant(op).hash(&mut hasher);
                expr_fingerprint(program, *expr, memo, visiting).hash(&mut hasher);
            }
            ExprKind::Binary { op, lhs, rhs } => {
                std::mem::discriminant(op).hash(&mut hasher);
                expr_fingerprint(program, *lhs, memo, visiting).hash(&mut hasher);
                expr_fingerprint(program, *rhs, memo, visiting).hash(&mut hasher);
            }
            ExprKind::PureCall { callee, args } => {
                callee.as_u32().hash(&mut hasher);
                for arg in args {
                    expr_fingerprint(program, *arg, memo, visiting).hash(&mut hasher);
                }
            }
            ExprKind::MakeStruct { ty, fields } => {
                ty.as_u32().hash(&mut hasher);
                for field in fields {
                    expr_fingerprint(program, *field, memo, visiting).hash(&mut hasher);
                }
            }
            ExprKind::MakeEnum {
                ty,
                variant,
                fields,
            } => {
                ty.as_u32().hash(&mut hasher);
                variant.as_u32().hash(&mut hasher);
                for field in fields {
                    expr_fingerprint(program, *field, memo, visiting).hash(&mut hasher);
                }
            }
            ExprKind::Error(error) => {
                error.span.start.hash(&mut hasher);
                error.span.end.hash(&mut hasher);
                error.message.hash(&mut hasher);
            }
        }
        hasher.finish()
    } else {
        0
    };

    visiting[expr_id.index()] = false;
    memo[expr_id.index()] = Some(value);
    value
}

fn hash_literal<H: Hasher>(literal: &crate::ir::core::Literal, hasher: &mut H) {
    match literal {
        crate::ir::core::Literal::Unit => 0u8.hash(hasher),
        crate::ir::core::Literal::Bool(value) => {
            1u8.hash(hasher);
            value.hash(hasher);
        }
        crate::ir::core::Literal::Int(value) => {
            2u8.hash(hasher);
            value.hash(hasher);
        }
        crate::ir::core::Literal::Float(value) => {
            3u8.hash(hasher);
            value.to_bits().hash(hasher);
        }
        crate::ir::core::Literal::Char(value) => {
            4u8.hash(hasher);
            value.hash(hasher);
        }
        crate::ir::core::Literal::String(value) => {
            5u8.hash(hasher);
            value.hash(hasher);
        }
    }
}
