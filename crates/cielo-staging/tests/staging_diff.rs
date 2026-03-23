use cielo_base::{ExprId, SourceId, Span, SymbolId};
use cielo_ir::core::{
    BinaryOp, CoreProgram, CoreTypeRef, ExprKind, ExprNode, FunctionDecl, Literal,
    PrimitiveTypeRef, StmtKind, StmtNode,
};
use cielo_ir::effect::SortedEffectRow;
use cielo_staging::pipeline::phases::{BtaTables, Stage};
use cielo_staging::pipeline::staging_diff::{
    SnapshotStage, StageSnapshotEntry, collect_snapshot, diff_snapshots, load_snapshot,
    save_snapshot,
};
use std::collections::BTreeSet;
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

#[test]
fn collect_snapshot_stable_ids_ignore_expr_index_churn() {
    fn build_program(reverse_literals: bool) -> (CoreProgram, BtaTables) {
        let mut program = CoreProgram::new();
        let source = SourceId::from_u32(33);
        let one_span = Span::new(source, 0, 1);
        let two_span = Span::new(source, 2, 3);
        let add_span = Span::new(source, 4, 5);

        let (one, two) = if reverse_literals {
            let two = program.push_expr(ExprNode {
                span: two_span,
                kind: ExprKind::Literal(Literal::Int(2)),
            });
            let one = program.push_expr(ExprNode {
                span: one_span,
                kind: ExprKind::Literal(Literal::Int(1)),
            });
            (one, two)
        } else {
            let one = program.push_expr(ExprNode {
                span: one_span,
                kind: ExprKind::Literal(Literal::Int(1)),
            });
            let two = program.push_expr(ExprNode {
                span: two_span,
                kind: ExprKind::Literal(Literal::Int(2)),
            });
            (one, two)
        };

        let add = program.push_expr(ExprNode {
            span: add_span,
            kind: ExprKind::Binary {
                op: BinaryOp::Add,
                lhs: one,
                rhs: two,
            },
        });
        let ret = program.push_stmt(StmtNode {
            span: add_span,
            kind: StmtKind::Return(add),
        });
        let main_id = program.add_function(FunctionDecl {
            name: SymbolId::from_u32(1),
            params: Vec::new(),
            param_types: Vec::new(),
            return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
            declared_effects: SortedEffectRow::empty(),
            body: ret,
            ct_only: false,
            span: add_span,
        });
        program.set_entrypoints([main_id]);

        let mut bta = BtaTables::default();
        for idx in 0..program.exprs().len() {
            bta.stage_of_expr.insert(ExprId::new(idx), Stage::Ct);
        }
        (program, bta)
    }

    let (program_a, bta_a) = build_program(false);
    let (program_b, bta_b) = build_program(true);
    let ids_a = collect_snapshot(&program_a, &bta_a)
        .into_iter()
        .map(|entry| entry.stable_id)
        .collect::<BTreeSet<_>>();
    let ids_b = collect_snapshot(&program_b, &bta_b)
        .into_iter()
        .map(|entry| entry.stable_id)
        .collect::<BTreeSet<_>>();

    assert_eq!(
        ids_a, ids_b,
        "stable snapshot IDs should remain equal when expr insertion order changes"
    );
    assert!(
        ids_a.iter().all(|id| !id.starts_with('e')),
        "stable IDs should no longer embed fragile ExprId indexes"
    );
}
