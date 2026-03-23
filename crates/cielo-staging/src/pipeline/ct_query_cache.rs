use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::common::densemap::DenseMap;
use crate::common::ids::ExprId;
use crate::ir::core::{CoreProgram, ExprKind, Literal};
use crate::pipeline::phases::{CtCacheKey, CtFileDep};

#[derive(Clone, Debug, Default)]
pub struct CtQueryCacheSnapshot {
    pub cache_key: CtCacheKey,
    pub file_deps: Vec<CtFileDep>,
    pub program_fingerprint: u64,
    pub ct_cache: DenseMap<ExprId, Literal>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CtQueryCacheSnapshotWire {
    cache_key: CtCacheKey,
    file_deps: Vec<CtFileDep>,
    program_fingerprint: u64,
    ct_cache: Vec<CtCacheEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CtCacheEntry {
    expr_index: u32,
    literal: Literal,
}

pub fn sidecar_path(snapshot_path: &Path) -> PathBuf {
    snapshot_path.with_extension("ctquery.bin")
}

pub fn load_snapshot(path: &Path) -> io::Result<CtQueryCacheSnapshot> {
    let bytes = std::fs::read(path)?;
    let wire: CtQueryCacheSnapshotWire =
        bincode::deserialize(&bytes).map_err(deserialize_error(path))?;

    let mut file_deps = wire.file_deps;
    file_deps.sort_by(|lhs, rhs| {
        lhs.path
            .cmp(&rhs.path)
            .then(lhs.content_hash.cmp(&rhs.content_hash))
    });

    let mut ct_cache = DenseMap::default();
    for entry in wire.ct_cache {
        let _ = ct_cache.insert(ExprId::from_u32(entry.expr_index), entry.literal);
    }

    Ok(CtQueryCacheSnapshot {
        cache_key: wire.cache_key,
        file_deps,
        program_fingerprint: wire.program_fingerprint,
        ct_cache,
    })
}

pub fn save_snapshot(path: &Path, snapshot: &CtQueryCacheSnapshot) -> io::Result<()> {
    let mut file_deps = snapshot.file_deps.clone();
    file_deps.sort_by(|lhs, rhs| {
        lhs.path
            .cmp(&rhs.path)
            .then(lhs.content_hash.cmp(&rhs.content_hash))
    });

    let mut ct_cache = snapshot
        .ct_cache
        .iter()
        .map(|(expr_id, literal)| CtCacheEntry {
            expr_index: expr_id.as_u32(),
            literal: literal.clone(),
        })
        .collect::<Vec<_>>();
    ct_cache.sort_by_key(|entry| entry.expr_index);

    let wire = CtQueryCacheSnapshotWire {
        cache_key: snapshot.cache_key.clone(),
        file_deps,
        program_fingerprint: snapshot.program_fingerprint,
        ct_cache,
    };
    let bytes = bincode::serialize(&wire).map_err(serialize_error(path))?;

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)
}

pub fn normalized_file_deps(mut deps: Vec<CtFileDep>) -> Vec<CtFileDep> {
    deps.sort_by(|lhs, rhs| {
        lhs.path
            .cmp(&rhs.path)
            .then(lhs.content_hash.cmp(&rhs.content_hash))
    });
    deps
}

pub fn deps_match(expected: &[CtFileDep], actual: &[CtFileDep]) -> bool {
    normalized_file_deps(expected.to_vec()) == normalized_file_deps(actual.to_vec())
}

pub fn fingerprint_program(program: &CoreProgram) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for expr in program.exprs() {
        expr.span.start.hash(&mut hasher);
        expr.span.end.hash(&mut hasher);
        match &expr.kind {
            ExprKind::Literal(literal) => {
                "lit".hash(&mut hasher);
                hash_literal(literal, &mut hasher);
            }
            ExprKind::Var(var) => {
                "var".hash(&mut hasher);
                var.as_u32().hash(&mut hasher);
            }
            ExprKind::Unary { op, expr } => {
                "un".hash(&mut hasher);
                std::mem::discriminant(op).hash(&mut hasher);
                expr.as_u32().hash(&mut hasher);
            }
            ExprKind::Binary { op, lhs, rhs } => {
                "bin".hash(&mut hasher);
                std::mem::discriminant(op).hash(&mut hasher);
                lhs.as_u32().hash(&mut hasher);
                rhs.as_u32().hash(&mut hasher);
            }
            ExprKind::PureCall { callee, args } => {
                "call".hash(&mut hasher);
                callee.as_u32().hash(&mut hasher);
                for arg in args {
                    arg.as_u32().hash(&mut hasher);
                }
            }
            ExprKind::MakeStruct { ty, fields } => {
                "mk_struct".hash(&mut hasher);
                ty.as_u32().hash(&mut hasher);
                for field in fields {
                    field.as_u32().hash(&mut hasher);
                }
            }
            ExprKind::MakeEnum {
                ty,
                variant,
                fields,
            } => {
                "mk_enum".hash(&mut hasher);
                ty.as_u32().hash(&mut hasher);
                variant.as_u32().hash(&mut hasher);
                for field in fields {
                    field.as_u32().hash(&mut hasher);
                }
            }
            ExprKind::Error(error) => {
                "err".hash(&mut hasher);
                error.span.start.hash(&mut hasher);
                error.span.end.hash(&mut hasher);
                error.message.hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

fn hash_literal<H: Hasher>(literal: &Literal, hasher: &mut H) {
    match literal {
        Literal::Unit => 0u8.hash(hasher),
        Literal::Bool(value) => {
            1u8.hash(hasher);
            value.hash(hasher);
        }
        Literal::Int(value) => {
            2u8.hash(hasher);
            value.hash(hasher);
        }
        Literal::Float(value) => {
            3u8.hash(hasher);
            value.to_bits().hash(hasher);
        }
        Literal::Char(value) => {
            4u8.hash(hasher);
            value.hash(hasher);
        }
        Literal::String(value) => {
            5u8.hash(hasher);
            value.hash(hasher);
        }
    }
}

fn serialize_error(path: &Path) -> impl FnOnce(bincode::Error) -> io::Error + '_ {
    move |err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "failed to serialize query snapshot {}: {err}",
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
                "failed to deserialize query snapshot {}: {err}",
                path.display()
            ),
        )
    }
}
