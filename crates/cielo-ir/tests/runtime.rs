use cielo_base::{CfgBlockId, CfgInstId, Span};
use cielo_ir::cfg::CfgProgram;
use cielo_ir::constants::ConstantTable;
use cielo_ir::ownership::OwnershipClass;
use cielo_ir::runtime::{RuntimeProgram, RuntimeSourceMap, RuntimeValueFacts};

#[test]
fn runtime_value_facts_are_keyed_by_cfg_value() {
    let mut cfg = CfgProgram::default();
    let trivial = cfg.push_value(None);
    let managed = cfg.push_value(None);
    let values = RuntimeValueFacts::new(vec![OwnershipClass::Trivial, OwnershipClass::Managed]);

    assert_eq!(values.ownership(trivial), OwnershipClass::Trivial);
    assert!(values.is_managed(managed));
}

#[test]
fn source_map_uses_synthetic_spans_for_unknown_sites() {
    let sources = RuntimeSourceMap::default();

    assert_eq!(
        sources.block_span(CfgBlockId::from_u32(9)),
        Span::synthetic()
    );
    assert_eq!(
        sources.instruction_span(CfgInstId::from_u32(9)),
        Span::synthetic()
    );
}

#[test]
fn runtime_program_requires_one_fact_per_cfg_value() {
    let mut cfg = CfgProgram::default();
    cfg.push_value(None);

    let runtime = RuntimeProgram::new(
        cfg,
        RuntimeValueFacts::new(vec![OwnershipClass::Managed]),
        RuntimeSourceMap::default(),
        ConstantTable::default(),
        Default::default(),
    );

    assert_eq!(runtime.values.len(), 1);
}

#[test]
#[should_panic(expected = "runtime value facts must cover every CFG value")]
fn runtime_program_rejects_incomplete_value_facts() {
    let mut cfg = CfgProgram::default();
    cfg.push_value(None);

    let _ = RuntimeProgram::new(
        cfg,
        RuntimeValueFacts::default(),
        RuntimeSourceMap::default(),
        ConstantTable::default(),
        Default::default(),
    );
}
