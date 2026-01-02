use std::fmt::Write;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};

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

pub fn sidecar_path(snapshot_path: &Path) -> PathBuf {
    snapshot_path.with_extension("ctquery.tsv")
}

pub fn load_snapshot(path: &Path) -> io::Result<CtQueryCacheSnapshot> {
    let text = std::fs::read_to_string(path)?;
    let mut snapshot = CtQueryCacheSnapshot::default();
    let mut deps = Vec::new();
    let mut cache = DenseMap::default();

    for line in text.lines() {
        let parts = line.splitn(3, '\t').collect::<Vec<_>>();
        if parts.len() != 3 {
            continue;
        }
        match parts[0] {
            "meta" if parts[1] == "program_fingerprint" => {
                snapshot.program_fingerprint = parts[2].parse::<u64>().unwrap_or_default();
            }
            "key" => apply_cache_key_field(&mut snapshot.cache_key, parts[1], parts[2]),
            "dep" => deps.push(CtFileDep {
                path: parts[1].to_owned(),
                content_hash: parts[2].to_owned(),
            }),
            "expr" => {
                let Ok(idx) = parts[1].parse::<usize>() else {
                    continue;
                };
                let Some(literal) = decode_literal(parts[2]) else {
                    continue;
                };
                cache.insert(ExprId::new(idx), literal);
            }
            _ => {}
        }
    }
    deps.sort_by(|lhs, rhs| {
        lhs.path
            .cmp(&rhs.path)
            .then(lhs.content_hash.cmp(&rhs.content_hash))
    });
    snapshot.file_deps = deps;
    snapshot.ct_cache = cache;
    Ok(snapshot)
}

pub fn save_snapshot(path: &Path, snapshot: &CtQueryCacheSnapshot) -> io::Result<()> {
    let mut text = String::new();
    writeln!(
        text,
        "meta\tprogram_fingerprint\t{}",
        snapshot.program_fingerprint
    )
    .expect("in-memory write should not fail");
    write_cache_key(&mut text, &snapshot.cache_key);

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

    let mut entries = snapshot
        .ct_cache
        .iter()
        .map(|(expr_id, literal)| (expr_id.index(), encode_literal(literal)))
        .collect::<Vec<_>>();
    entries.sort_by_key(|(idx, _)| *idx);
    for (idx, literal) in entries {
        writeln!(text, "expr\t{}\t{}", idx, literal).expect("in-memory write should not fail");
    }

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text)
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

fn write_cache_key(out: &mut String, cache_key: &CtCacheKey) {
    writeln!(
        out,
        "key\ttarget_word_size_bits\t{}",
        cache_key.target_word_size_bits
    )
    .expect("in-memory write should not fail");
    writeln!(
        out,
        "key\ttarget_endianness\t{}",
        cache_key.target_endianness
    )
    .expect("in-memory write should not fail");
    writeln!(
        out,
        "key\ttarget_pointer_alignment\t{}",
        cache_key.target_pointer_alignment
    )
    .expect("in-memory write should not fail");
    writeln!(out, "key\tevaluator_policy\t{}", cache_key.evaluator_policy)
        .expect("in-memory write should not fail");
    writeln!(out, "key\tcompiler_version\t{}", cache_key.compiler_version)
        .expect("in-memory write should not fail");
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

fn encode_literal(literal: &Literal) -> String {
    match literal {
        Literal::Unit => "u".to_owned(),
        Literal::Bool(value) => format!("b:{}", if *value { 1 } else { 0 }),
        Literal::Int(value) => format!("i:{value}"),
        Literal::Float(value) => format!("f:{}", value.to_bits()),
        Literal::Char(value) => format!("c:{}", *value as u32),
        Literal::String(value) => format!("s:{}", encode_hex(value.as_bytes())),
    }
}

fn decode_literal(text: &str) -> Option<Literal> {
    if text == "u" {
        return Some(Literal::Unit);
    }
    let mut parts = text.splitn(2, ':');
    let tag = parts.next()?;
    let payload = parts.next()?;
    match tag {
        "b" => match payload {
            "0" => Some(Literal::Bool(false)),
            "1" => Some(Literal::Bool(true)),
            _ => None,
        },
        "i" => payload.parse::<i64>().ok().map(Literal::Int),
        "f" => payload
            .parse::<u64>()
            .ok()
            .map(f64::from_bits)
            .map(Literal::Float),
        "c" => payload
            .parse::<u32>()
            .ok()
            .and_then(char::from_u32)
            .map(Literal::Char),
        "s" => decode_hex(payload)
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .map(Literal::String),
        _ => None,
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        write!(out, "{:02x}", byte).expect("in-memory write should not fail");
    }
    out
}

fn decode_hex(encoded: &str) -> Option<Vec<u8>> {
    if !encoded.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(encoded.len() / 2);
    let bytes = encoded.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() {
        let hi = decode_nibble(bytes[idx])?;
        let lo = decode_nibble(bytes[idx + 1])?;
        out.push((hi << 4) | lo);
        idx += 2;
    }
    Some(out)
}

fn decode_nibble(ch: u8) -> Option<u8> {
    match ch {
        b'0'..=b'9' => Some(ch - b'0'),
        b'a'..=b'f' => Some(10 + (ch - b'a')),
        b'A'..=b'F' => Some(10 + (ch - b'A')),
        _ => None,
    }
}
