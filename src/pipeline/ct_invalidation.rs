use std::collections::BTreeMap;
use std::fmt::Write;
use std::io;
use std::path::{Path, PathBuf};

use crate::pipeline::phases::{CtCacheKey, CtFileDep};

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct CtDepSnapshot {
    pub cache_key: CtCacheKey,
    pub file_deps: Vec<CtFileDep>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
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

pub fn sidecar_path(snapshot_path: &Path) -> PathBuf {
    snapshot_path.with_extension("ctdeps.tsv")
}

pub fn diff(previous: &CtDepSnapshot, current: &CtDepSnapshot) -> Vec<CtInvalidationReason> {
    let mut reasons = Vec::new();
    diff_cache_key(&previous.cache_key, &current.cache_key, &mut reasons);
    diff_file_deps(&previous.file_deps, &current.file_deps, &mut reasons);
    reasons
}

pub fn load_snapshot(path: &Path) -> io::Result<CtDepSnapshot> {
    let text = std::fs::read_to_string(path)?;
    let mut snapshot = CtDepSnapshot::default();
    let mut file_deps = Vec::new();

    for line in text.lines() {
        let parts = line.splitn(3, '\t').collect::<Vec<_>>();
        if parts.len() != 3 {
            continue;
        }
        match parts[0] {
            "key" => apply_cache_key_field(&mut snapshot.cache_key, parts[1], parts[2]),
            "dep" => file_deps.push(CtFileDep {
                path: parts[1].to_owned(),
                content_hash: parts[2].to_owned(),
            }),
            _ => {}
        }
    }

    file_deps.sort_by(|lhs, rhs| {
        lhs.path
            .cmp(&rhs.path)
            .then(lhs.content_hash.cmp(&rhs.content_hash))
    });
    snapshot.file_deps = file_deps;
    Ok(snapshot)
}

pub fn save_snapshot(path: &Path, snapshot: &CtDepSnapshot) -> io::Result<()> {
    let mut text = String::new();
    writeln!(
        text,
        "key\ttarget_word_size_bits\t{}",
        snapshot.cache_key.target_word_size_bits
    )
    .expect("in-memory write should not fail");
    writeln!(
        text,
        "key\ttarget_endianness\t{}",
        snapshot.cache_key.target_endianness
    )
    .expect("in-memory write should not fail");
    writeln!(
        text,
        "key\ttarget_pointer_alignment\t{}",
        snapshot.cache_key.target_pointer_alignment
    )
    .expect("in-memory write should not fail");
    writeln!(
        text,
        "key\tevaluator_policy\t{}",
        snapshot.cache_key.evaluator_policy
    )
    .expect("in-memory write should not fail");
    writeln!(
        text,
        "key\tcompiler_version\t{}",
        snapshot.cache_key.compiler_version
    )
    .expect("in-memory write should not fail");

    let mut deps = snapshot.file_deps.clone();
    deps.sort_by(|lhs, rhs| {
        lhs.path
            .cmp(&rhs.path)
            .then(lhs.content_hash.cmp(&rhs.content_hash))
    });
    for dep in deps {
        writeln!(text, "dep\t{}\t{}", dep.path, dep.content_hash)
            .expect("in-memory write should not fail");
    }

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text)
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

fn apply_cache_key_field(cache_key: &mut CtCacheKey, field: &str, value: &str) {
    match field {
        "target_word_size_bits" => {
            cache_key.target_word_size_bits = value.parse::<u8>().unwrap_or_default();
        }
        "target_endianness" => cache_key.target_endianness = value.to_owned(),
        "target_pointer_alignment" => {
            cache_key.target_pointer_alignment = value.parse::<u8>().unwrap_or_default();
        }
        "evaluator_policy" => cache_key.evaluator_policy = value.to_owned(),
        "compiler_version" => cache_key.compiler_version = value.to_owned(),
        _ => {}
    }
}
