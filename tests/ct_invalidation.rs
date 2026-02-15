use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use cielo::pipeline::ct_invalidation::{
    CtDepSnapshot, CtInvalidationReason, diff, load_snapshot, save_snapshot, sidecar_path,
};
use cielo::pipeline::phases::{CtCacheKey, CtFileDep};

fn key(word_size: u8, endianness: &str, alignment: u8, policy: &str, version: &str) -> CtCacheKey {
    CtCacheKey {
        target_word_size_bits: word_size,
        target_endianness: endianness.to_owned(),
        target_pointer_alignment: alignment,
        evaluator_policy: policy.to_owned(),
        compiler_version: version.to_owned(),
    }
}

#[test]
fn diff_reports_cache_key_and_dependency_invalidation_reasons() {
    let previous = CtDepSnapshot {
        cache_key: key(64, "little", 8, "v1", "1.0.0"),
        file_deps: vec![
            CtFileDep {
                path: "/tmp/a".to_owned(),
                content_hash: "1111".to_owned(),
            },
            CtFileDep {
                path: "/tmp/b".to_owned(),
                content_hash: "2222".to_owned(),
            },
        ],
    };
    let current = CtDepSnapshot {
        cache_key: key(32, "big", 16, "v2", "2.0.0"),
        file_deps: vec![
            CtFileDep {
                path: "/tmp/a".to_owned(),
                content_hash: "9999".to_owned(),
            },
            CtFileDep {
                path: "/tmp/c".to_owned(),
                content_hash: "3333".to_owned(),
            },
        ],
    };

    let reasons = diff(&previous, &current);
    assert!(
        reasons.iter().any(|reason| matches!(
            reason,
            CtInvalidationReason::TargetWordSizeChanged {
                before: 64,
                after: 32
            }
        )),
        "word-size changes must be reported"
    );
    assert!(
        reasons.iter().any(|reason| matches!(
            reason,
            CtInvalidationReason::TargetEndiannessChanged { before, after }
                if before == "little" && after == "big"
        )),
        "endianness changes must be reported"
    );
    assert!(
        reasons.iter().any(|reason| matches!(
            reason,
            CtInvalidationReason::TargetAlignmentChanged {
                before: 8,
                after: 16
            }
        )),
        "pointer-alignment changes must be reported"
    );
    assert!(
        reasons.iter().any(|reason| matches!(
            reason,
            CtInvalidationReason::EvaluatorPolicyChanged { before, after }
                if before == "v1" && after == "v2"
        )),
        "evaluator-policy changes must be reported"
    );
    assert!(
        reasons.iter().any(|reason| matches!(
            reason,
            CtInvalidationReason::CompilerVersionChanged { before, after }
                if before == "1.0.0" && after == "2.0.0"
        )),
        "compiler-version changes must be reported"
    );
    assert!(
        reasons.iter().any(|reason| matches!(
            reason,
            CtInvalidationReason::FileChanged {
                path,
                before_hash,
                after_hash
            } if path == "/tmp/a" && before_hash == "1111" && after_hash == "9999"
        )),
        "dependency hash changes must be reported"
    );
    assert!(
        reasons.iter().any(|reason| matches!(
            reason,
            CtInvalidationReason::FileRemoved { path, content_hash }
                if path == "/tmp/b" && content_hash == "2222"
        )),
        "removed dependencies must be reported"
    );
    assert!(
        reasons.iter().any(|reason| matches!(
            reason,
            CtInvalidationReason::FileAdded { path, content_hash }
                if path == "/tmp/c" && content_hash == "3333"
        )),
        "added dependencies must be reported"
    );
}

#[test]
fn snapshot_roundtrip_is_stable_and_sorted() {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("cielo_ctdeps_{stamp}.tsv"));
    let sidecar = sidecar_path(path.as_path());
    assert!(
        sidecar.to_string_lossy().ends_with(".ctdeps.bin"),
        "sidecar naming should stay stable for tooling integration"
    );

    let snapshot = CtDepSnapshot {
        cache_key: key(64, "little", 8, "v1-int-wrap-litnorm", "1.2.0"),
        file_deps: vec![
            CtFileDep {
                path: "/tmp/z".to_owned(),
                content_hash: "ffff".to_owned(),
            },
            CtFileDep {
                path: "/tmp/a".to_owned(),
                content_hash: "aaaa".to_owned(),
            },
        ],
    };

    save_snapshot(path.as_path(), &snapshot).expect("save snapshot");
    let loaded = load_snapshot(path.as_path()).expect("load snapshot");
    fs::remove_file(path.as_path()).expect("cleanup snapshot");

    assert_eq!(loaded.cache_key, snapshot.cache_key);
    assert_eq!(loaded.file_deps.len(), 2);
    assert_eq!(loaded.file_deps[0].path, "/tmp/a");
    assert_eq!(loaded.file_deps[1].path, "/tmp/z");
}
