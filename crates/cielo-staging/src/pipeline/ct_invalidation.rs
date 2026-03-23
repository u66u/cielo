use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::pipeline::phases::{CtCacheKey, CtFileDep};

#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct CtDepSnapshot {
    pub cache_key: CtCacheKey,
    pub file_deps: Vec<CtFileDep>,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum CtInvalidationReason {
    TargetWordSizeChanged {
        before: u8,
        after: u8,
    },
    TargetEndiannessChanged {
        before: String,
        after: String,
    },
    TargetAlignmentChanged {
        before: u8,
        after: u8,
    },
    EvaluatorPolicyChanged {
        before: String,
        after: String,
    },
    CompilerVersionChanged {
        before: String,
        after: String,
    },
    FileAdded {
        path: String,
        content_hash: String,
    },
    FileRemoved {
        path: String,
        content_hash: String,
    },
    FileChanged {
        path: String,
        before_hash: String,
        after_hash: String,
    },
}

impl CtInvalidationReason {
    pub fn describe(&self) -> String {
        match self {
            Self::TargetWordSizeChanged { before, after } => {
                format!("target word size changed: {before} -> {after}")
            }
            Self::TargetEndiannessChanged { before, after } => {
                format!("target endianness changed: {before} -> {after}")
            }
            Self::TargetAlignmentChanged { before, after } => {
                format!("target pointer alignment changed: {before} -> {after}")
            }
            Self::EvaluatorPolicyChanged { before, after } => {
                format!("ct evaluator policy changed: {before} -> {after}")
            }
            Self::CompilerVersionChanged { before, after } => {
                format!("compiler version changed: {before} -> {after}")
            }
            Self::FileAdded { path, content_hash } => {
                format!("dependency added: {path} ({content_hash})")
            }
            Self::FileRemoved { path, content_hash } => {
                format!("dependency removed: {path} ({content_hash})")
            }
            Self::FileChanged {
                path,
                before_hash,
                after_hash,
            } => format!("dependency changed: {path} ({before_hash} -> {after_hash})"),
        }
    }
}

pub fn sidecar_path(snapshot_path: &Path) -> PathBuf {
    snapshot_path.with_extension("ctdeps.bin")
}

pub fn diff(previous: &CtDepSnapshot, current: &CtDepSnapshot) -> Vec<CtInvalidationReason> {
    let mut reasons = Vec::new();
    diff_cache_key(&previous.cache_key, &current.cache_key, &mut reasons);
    diff_file_deps(&previous.file_deps, &current.file_deps, &mut reasons);
    reasons
}

pub fn load_snapshot(path: &Path) -> io::Result<CtDepSnapshot> {
    let bytes = std::fs::read(path)?;
    let mut snapshot: CtDepSnapshot =
        bincode::deserialize(&bytes).map_err(deserialize_error(path))?;

    snapshot.file_deps.sort_by(|lhs, rhs| {
        lhs.path
            .cmp(&rhs.path)
            .then(lhs.content_hash.cmp(&rhs.content_hash))
    });
    Ok(snapshot)
}

pub fn save_snapshot(path: &Path, snapshot: &CtDepSnapshot) -> io::Result<()> {
    let mut to_save = snapshot.clone();
    to_save.file_deps.sort_by(|lhs, rhs| {
        lhs.path
            .cmp(&rhs.path)
            .then(lhs.content_hash.cmp(&rhs.content_hash))
    });
    let bytes = bincode::serialize(&to_save).map_err(serialize_error(path))?;

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)
}

fn diff_cache_key(
    previous: &CtCacheKey,
    current: &CtCacheKey,
    reasons: &mut Vec<CtInvalidationReason>,
) {
    if previous.target_word_size_bits != current.target_word_size_bits {
        reasons.push(CtInvalidationReason::TargetWordSizeChanged {
            before: previous.target_word_size_bits,
            after: current.target_word_size_bits,
        });
    }
    if previous.target_endianness != current.target_endianness {
        reasons.push(CtInvalidationReason::TargetEndiannessChanged {
            before: previous.target_endianness.clone(),
            after: current.target_endianness.clone(),
        });
    }
    if previous.target_pointer_alignment != current.target_pointer_alignment {
        reasons.push(CtInvalidationReason::TargetAlignmentChanged {
            before: previous.target_pointer_alignment,
            after: current.target_pointer_alignment,
        });
    }
    if previous.evaluator_policy != current.evaluator_policy {
        reasons.push(CtInvalidationReason::EvaluatorPolicyChanged {
            before: previous.evaluator_policy.clone(),
            after: current.evaluator_policy.clone(),
        });
    }
    if previous.compiler_version != current.compiler_version {
        reasons.push(CtInvalidationReason::CompilerVersionChanged {
            before: previous.compiler_version.clone(),
            after: current.compiler_version.clone(),
        });
    }
}

fn diff_file_deps(
    previous: &[CtFileDep],
    current: &[CtFileDep],
    reasons: &mut Vec<CtInvalidationReason>,
) {
    let previous_map = previous
        .iter()
        .map(|dep| (dep.path.clone(), dep.content_hash.clone()))
        .collect::<BTreeMap<_, _>>();
    let current_map = current
        .iter()
        .map(|dep| (dep.path.clone(), dep.content_hash.clone()))
        .collect::<BTreeMap<_, _>>();

    for (path, before_hash) in &previous_map {
        match current_map.get(path) {
            Some(after_hash) if before_hash != after_hash => {
                reasons.push(CtInvalidationReason::FileChanged {
                    path: path.clone(),
                    before_hash: before_hash.clone(),
                    after_hash: after_hash.clone(),
                });
            }
            Some(_) => {}
            None => reasons.push(CtInvalidationReason::FileRemoved {
                path: path.clone(),
                content_hash: before_hash.clone(),
            }),
        }
    }
    for (path, content_hash) in &current_map {
        if !previous_map.contains_key(path) {
            reasons.push(CtInvalidationReason::FileAdded {
                path: path.clone(),
                content_hash: content_hash.clone(),
            });
        }
    }
}

fn serialize_error(path: &Path) -> impl FnOnce(bincode::Error) -> io::Error + '_ {
    move |err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "failed to serialize ct dep snapshot {}: {err}",
                path.display()
            ),
        )
    }
}

fn deserialize_error(path: &Path) -> impl FnOnce(bincode::Error) -> io::Error + '_ {
    move |err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "failed to deserialize ct dep snapshot {}: {err}",
                path.display()
            ),
        )
    }
}
