use cielo_base::{DiagnosticBag, SourceId, Span, SymbolId};
use cielo_ir::cfg::{
    CfgCallConvention, CfgExpr, CfgInstruction, CfgMatchArm, CfgProgram, CfgProjectionMode,
    CfgTerminator,
};
use cielo_ir::runtime::RuntimeSourceMap;
use cielo_memory::refcount::borrow_hazard::{
    BorrowHazardKind, BorrowHazardSite, analyze, emit_diagnostics,
};

#[test]
fn alias_fanout_is_found_from_cfg_uses() {
    let mut cfg = CfgProgram::default();
    let source = cfg.push_value(None);
    let alias = cfg.push_value(None);
    let source_expr = cfg.push_expr(CfgExpr::Value(source), None);
    let block = cfg.push_block(Vec::new(), None);
    let alias_instruction = cfg.push_instruction(
        block,
        CfgInstruction::Let {
            result: alias,
            value: source_expr,
        },
        None,
    );
    cfg.set_terminator(block, CfgTerminator::Return(source_expr));

    let report = analyze(&cfg, &[true, true]);

    assert_eq!(report.alias_fanout_count, 1);
    assert_eq!(report.alias_fanout_sites, vec![alias_instruction]);
    assert_eq!(report.projection_count, 0);
    assert_eq!(report.call_escape_count, 0);
    assert_eq!(
        report.hotspots[0].site,
        BorrowHazardSite::Instruction(alias_instruction)
    );
}

#[test]
fn unmanaged_aliases_are_not_reported() {
    let mut cfg = CfgProgram::default();
    let source = cfg.push_value(None);
    let alias = cfg.push_value(None);
    let source_expr = cfg.push_expr(CfgExpr::Value(source), None);
    let block = cfg.push_block(Vec::new(), None);
    cfg.push_instruction(
        block,
        CfgInstruction::Let {
            result: alias,
            value: source_expr,
        },
        None,
    );
    cfg.set_terminator(block, CfgTerminator::Return(source_expr));

    let report = analyze(&cfg, &[false, false]);

    assert_eq!(report.alias_fanout_count, 0);
    assert!(report.hotspots.is_empty());
}

#[test]
fn managed_match_binders_are_projection_hazards() {
    let mut cfg = CfgProgram::default();
    let scrutinee = cfg.push_value(None);
    let binder = cfg.push_value(None);
    let scrutinee_expr = cfg.push_expr(CfgExpr::Value(scrutinee), None);
    let binder_expr = cfg.push_expr(CfgExpr::Value(binder), None);
    let arm = cfg.push_block(vec![binder], None);
    cfg.set_terminator(arm, CfgTerminator::Return(binder_expr));
    let fallback_value = cfg.push_expr(CfgExpr::Literal(cielo_ir::core::Literal::Unit), None);
    let fallback = cfg.push_block(Vec::new(), None);
    cfg.set_terminator(fallback, CfgTerminator::Return(fallback_value));
    let entry = cfg.push_block(Vec::new(), None);
    cfg.set_terminator(
        entry,
        CfgTerminator::Match {
            scrutinee: scrutinee_expr,
            arms: vec![CfgMatchArm {
                tag: SymbolId::from_u32(2),
                binders: vec![binder],
                projections: vec![CfgProjectionMode::Borrow],
                target: arm,
            }],
            default: fallback,
        },
    );

    let report = analyze(&cfg, &[true, true]);

    assert_eq!(report.projection_count, 1);
    assert_eq!(report.projection_sites, vec![entry]);
    assert_eq!(report.hotspots[0].site, BorrowHazardSite::Terminator(entry));
}

#[test]
fn shared_values_crossing_calls_are_escape_hazards() {
    let mut cfg = CfgProgram::default();
    let shared = cfg.push_value(None);
    let observed = cfg.push_value(None);
    let result = cfg.push_value(None);
    let shared_expr = cfg.push_expr(CfgExpr::Value(shared), None);
    let entry = cfg.push_block(Vec::new(), None);
    cfg.push_instruction(
        entry,
        CfgInstruction::Eval {
            result: observed,
            value: shared_expr,
        },
        None,
    );
    let result_expr = cfg.push_expr(CfgExpr::Value(result), None);
    let target = cfg.push_block(vec![result], None);
    cfg.set_terminator(target, CfgTerminator::Return(result_expr));
    cfg.set_terminator(
        entry,
        CfgTerminator::Call {
            convention: CfgCallConvention::Direct,
            callee: SymbolId::from_u32(4),
            args: vec![shared_expr],
            result,
            target,
        },
    );

    let report = analyze(&cfg, &[true, true, true]);

    assert_eq!(report.call_escape_count, 1);
    assert_eq!(report.call_escape_sites, vec![entry]);
    assert!(
        report
            .hotspots
            .iter()
            .any(|hotspot| hotspot.kind == BorrowHazardKind::CallEscape)
    );
}

#[test]
fn diagnostics_use_runtime_source_spans() {
    let mut cfg = CfgProgram::default();
    let source = cfg.push_value(None);
    let alias = cfg.push_value(None);
    let source_expr = cfg.push_expr(CfgExpr::Value(source), None);
    let block = cfg.push_block(Vec::new(), None);
    cfg.push_instruction(
        block,
        CfgInstruction::Let {
            result: alias,
            value: source_expr,
        },
        None,
    );
    cfg.set_terminator(block, CfgTerminator::Return(source_expr));

    let span = Span::new(SourceId::from_u32(3), 20, 30);
    let sources = RuntimeSourceMap::new(vec![Span::synthetic()], vec![span]);
    let report = analyze(&cfg, &[true, true]);
    let mut diagnostics = DiagnosticBag::default();
    emit_diagnostics(&sources, &report, &mut diagnostics);

    let warning = diagnostics
        .entries()
        .iter()
        .find(|diagnostic| diagnostic.code == "BORROW_HAZARD_ALIAS_FANOUT")
        .expect("alias warning");
    assert_eq!(warning.span, span);
    assert!(warning.message.contains("alias-fanout-i0"));
    assert!(
        diagnostics
            .entries()
            .iter()
            .any(|diagnostic| diagnostic.code == "BORROW_HAZARD_SUMMARY")
    );
}
