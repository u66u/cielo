use cielo::pipeline::staging_diff::{
    SnapshotStage, StageSnapshotEntry, diff_snapshots, load_snapshot, save_snapshot,
};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn staging_diff_reports_stage_transitions() {
    let previous = vec![StageSnapshotEntry {
        stable_id: "e1@0-1:lit".to_owned(),
        stage: SnapshotStage::Ct,
        top_reason: String::new(),
        cause_hash: 0,
    }];
    let current = vec![StageSnapshotEntry {
        stable_id: "e1@0-1:lit".to_owned(),
        stage: SnapshotStage::Rt,
        top_reason: "forced-runtime".to_owned(),
        cause_hash: 123,
    }];

    let diff = diff_snapshots(previous.as_slice(), current.as_slice());
    assert_eq!(diff.len(), 1);
    assert_eq!(diff[0].stable_id, "e1@0-1:lit");
    assert_eq!(diff[0].before, SnapshotStage::Ct);
    assert_eq!(diff[0].after, SnapshotStage::Rt);
}

#[test]
fn staging_snapshot_roundtrip_is_stable() {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("cielo_stage_snapshot_{stamp}.tsv"));

    let entries = vec![
        StageSnapshotEntry {
            stable_id: "e1@0-1:lit".to_owned(),
            stage: SnapshotStage::Ct,
            top_reason: String::new(),
            cause_hash: 0,
        },
        StageSnapshotEntry {
            stable_id: "e2@2-3:call".to_owned(),
            stage: SnapshotStage::Rt,
            top_reason: "effect-e0".to_owned(),
            cause_hash: 99,
        },
    ];

    save_snapshot(path.as_path(), entries.as_slice()).expect("save");
    let loaded = load_snapshot(path.as_path()).expect("load");
    fs::remove_file(path).expect("cleanup");
    assert_eq!(loaded, entries);
}
