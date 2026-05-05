use std::collections::BTreeMap;
use std::fmt::Write;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::Path;

use crate::pipeline::phases::{BtaTables, Stage};
use cielo_base::ExprId;
use cielo_ir::core::{CoreProgram, ExprKind};
use cielo_ir::walk::fingerprint_exprs;

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

impl SnapshotStage {
    pub fn from_stage(stage: Stage) -> Self {
        match stage {
            Stage::Ct => Self::Ct,
            Stage::Rt(_) => Self::Rt,
        }
    }

    pub fn from_snapshot_token(token: &str) -> Option<Self> {
        match token {
            "ct" => Some(Self::Ct),
            "rt" => Some(Self::Rt),
            _ => None,
        }
    }

    pub const fn snapshot_token(self) -> &'static str {
        match self {
            Self::Ct => "ct",
            Self::Rt => "rt",
        }
    }

    pub const fn display_text(self) -> &'static str {
        match self {
            Self::Ct => "CT",
            Self::Rt => "RT",
        }
    }
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
    let fingerprints = fingerprint_exprs(program);
    let mut entries = Vec::new();
    for (idx, expr) in program.exprs().iter().enumerate() {
        let expr_id = ExprId::new(idx);
        let Some(stage) = bta.stage_of_expr.get(&expr_id).copied() else {
            continue;
        };
        let stage_tag = SnapshotStage::from_stage(stage);
        let top_reason = match stage {
            Stage::Ct => String::new(),
            Stage::Rt(reason) => reason.stable_tag(),
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
        let Some(stage) = SnapshotStage::from_snapshot_token(parts[1]) else {
            continue;
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
        writeln!(
            text,
            "{}\t{}\t{}\t{}",
            entry.stable_id,
            entry.stage.snapshot_token(),
            entry.cause_hash,
            entry.top_reason
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
    format!("{}-{}:{}:{:016x}", start, end, kind.tag(), fingerprint)
}

fn hash_reason(reason: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    reason.hash(&mut hasher);
    hasher.finish()
}
