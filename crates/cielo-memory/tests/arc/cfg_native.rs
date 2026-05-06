use cielo_base::Interner;
use cielo_base::{SourceId, SymbolId};
use cielo_ir::cfg::{
    CfgArcOpKind, CfgExpr, CfgInstruction, CfgProgram, CfgProjectionMode, CfgTerminator,
};
use cielo_ir::constants::ConstantTable;
use cielo_ir::core::Literal;
use cielo_memory::MemoryPreset;
use cielo_memory::{ArcConfig, ArcFeatures};
use cielo_runtime::{cfg_lower, linearize};
use cielo_test_support::{PassConfig, PassHarness};

use crate::helpers::core::emit_pipeline;

fn compile(source: &str) -> cielo_test_support::CompiledC {
    let mut interner = Interner::new();
    PassHarness::new(PassConfig::default()).compile_source_to_c(
        source,
        SourceId::from_u32(0),
        &mut interner,
    )
}

fn compile_with_preset(source: &str, preset: MemoryPreset) -> cielo_test_support::CompiledC {
    let mut interner = Interner::new();
    PassHarness::new(PassConfig::default().with_memory_preset(preset)).compile_source_to_c(
        source,
        SourceId::from_u32(0),
        &mut interner,
    )
}

fn compile_without_normalize(source: &str) -> cielo_test_support::CompiledC {
    let mut interner = Interner::new();
    let compiler = PassHarness::new(PassConfig::default());
    let core = compiler.parse_and_lower_to_core(source, SourceId::from_u32(0), &mut interner);
    let mut residual = compiler.stage_core(core);
    let sema = residual.sema().clone();
    let linear = {
        let (program, diagnostics) = residual.program_and_diagnostics_mut();
        linearize::run(program, &sema, diagnostics)
    };
    let cfg = cfg_lower::run(&linear);
    let emitted = emit_pipeline(residual, linear, cfg, &interner, Default::default());
    cielo_test_support::CompiledC {
        residual: emitted.residual,
        linear: emitted.linear,
        cfg: emitted.cfg,
        memory: emitted.memory,
        c_source: emitted.c_source,
    }
}

/// The runtime header defines these helpers and calls them itself, so counting
/// over the whole file measures the header, not the program.
fn emitted_bodies(c_source: &str) -> &str {
    let start = c_source
        .find("\nstatic CieloValue cielo_fn_")
        .expect("emitted C must define at least one function");
    &c_source[start..]
}

fn arc_op_count(compiled: &cielo_test_support::CompiledC, kind: CfgArcOpKind) -> usize {
    let mut count = 0;
    for block in compiled.cfg.blocks() {
        count += block.entry_arc.iter().filter(|op| op.kind == kind).count();
        count += block
            .terminator_arc
            .pre
            .iter()
            .chain(&block.terminator_arc.post)
            .filter(|op| op.kind == kind)
            .count();
        for instruction in &block.instructions {
            let instruction = compiled.cfg.instruction(*instruction).unwrap();
            count += instruction
                .arc
                .pre
                .iter()
                .chain(&instruction.arc.post)
                .filter(|op| op.kind == kind)
                .count();
        }
    }
    count
}

#[test]
fn cfg_arc_moves_last_use_call_arguments() {
    let compiled = compile(
        r#"
enum Boxed { Wrap(Int) }
fn consume(v: Boxed) -> Int { match v { | Wrap(n) => n | _ => 0 } }
fn main() -> Int { let value = Wrap(1); consume(value) }
"#,
    );
    assert_eq!(
        arc_op_count(&compiled, CfgArcOpKind::Retain),
        0,
        "a unique last-use argument should sink without a retain"
    );
    assert!(
        compiled
            .memory
            .reference_counting()
            .expect("ARC test must select reference counting")
            .arc
            .eliminated_move_pairs
            > 0
    );
}

#[test]
fn cfg_arc_raw_materializes_pairs_that_optimized_sinks() {
    let source = r#"
enum Boxed { Wrap(Int) }
fn consume(v: Boxed) -> Int { match v { | Wrap(n) => n | _ => 0 } }
fn main() -> Int { let value = Wrap(1); consume(value) }
"#;
    let raw = compile_with_preset(source, MemoryPreset::ArcRaw);
    let optimized = compile_with_preset(source, MemoryPreset::ArcOptimized);

    assert!(arc_op_count(&raw, CfgArcOpKind::Retain) > 0);
    assert!(arc_op_count(&raw, CfgArcOpKind::Release) > 0);
    assert_eq!(arc_op_count(&optimized, CfgArcOpKind::Retain), 0);
    let raw_stats = raw
        .memory
        .reference_counting()
        .expect("raw profile must select reference counting")
        .arc;
    let optimized_stats = optimized
        .memory
        .reference_counting()
        .expect("optimized profile must select reference counting")
        .arc;
    assert!(optimized_stats.eliminated_move_pairs > raw_stats.eliminated_move_pairs);
}

#[test]
fn cfg_arc_retains_non_last_call_arguments() {
    let compiled = compile(
        r#"
enum Boxed { Wrap(Int) }
fn consume(v: Boxed) -> Int { match v { | Wrap(n) => n | _ => 0 } }
fn main() -> Int {
  let value = Wrap(1);
  let first = consume(value);
  first + consume(value)
}
"#,
    );
    assert!(arc_op_count(&compiled, CfgArcOpKind::Retain) >= 1);
}

#[test]
fn cfg_arc_accounts_for_duplicate_sink_arguments() {
    let compiled = compile_without_normalize(
        r#"
enum Boxed { Wrap(Int) }
fn score(value: Boxed) -> Int { match value { | Wrap(n) => n | _ => 0 } }
fn choose(a: Boxed, b: Boxed) -> Int { score(a) + score(b) }
fn main() -> Int { @runtime { let value = Wrap(1); choose(value, value) } }
"#,
    );
    assert!(
        arc_op_count(&compiled, CfgArcOpKind::Retain) >= 1,
        "two owned parameters require two references"
    );
}

#[test]
fn cfg_match_moves_fields_out_of_dead_parent() {
    let compiled = compile_without_normalize(
        r#"
enum Leaf { N(Int) }
enum Boxed { Wrap(Leaf) }
fn unbox(value: Boxed) -> Leaf {
  match value { | Wrap(leaf) => leaf | _ => N(0) }
}
fn main() -> Int {
  let leaf = unbox(Wrap(N(42)));
  match leaf { | N(value) => value | _ => 0 }
}
"#,
    );
    assert!(
        compiled.cfg.blocks().iter().any(|block| {
            matches!(&block.terminator, CfgTerminator::Match { arms, .. }
                if arms.iter().any(|arm| arm.projections.contains(&CfgProjectionMode::Move)))
        }),
        "expected moved projection in {:#?}",
        compiled.cfg
    );
    assert!(compiled.c_source.contains("cielo_ctor_take_field"));
}

/// `one` is a block parameter joined from two fresh allocations and read only
/// by the match, so the uniqueness query discharges the runtime `rc == 1` test
/// the take would otherwise perform.
#[test]
fn cfg_match_takes_a_statically_unique_parent_without_a_runtime_check() {
    let compiled = compile(
        r#"
enum Leaf { N(Int) }
enum Boxed { Wrap(Leaf), Empty(Int) }
fn peel(l: Leaf) -> Int { match l { | N(v) => v | _ => 0 } }
fn main() -> Int {
  let seed = @runtime { 1 + 2 };
  let one = if seed > 2 { Wrap(N(seed)) } else { Wrap(N(0)) };
  match one { | Wrap(leaf) => peel(leaf) | _ => 0 }
}
"#,
    );
    let stats = compiled
        .memory
        .reference_counting()
        .expect("ARC test must select reference counting")
        .arc;
    assert!(
        stats.static_unique_takes > 0,
        "expected a statically unique take, got {stats:?}"
    );
    assert!(compiled.cfg.blocks().iter().any(
        |block| matches!(&block.terminator, CfgTerminator::Match { arms, .. }
                if arms.iter().any(|arm| arm.projections.contains(&CfgProjectionMode::MoveUnique)))
    ),);
    let bodies = emitted_bodies(compiled.c_source.as_str());
    assert_eq!(
        bodies.matches("cielo_ctor_take_field_unique(").count(),
        stats.static_unique_takes as usize,
        "unchecked takes in the emitted bodies must match the plan: {bodies}"
    );
}

/// `leaf` comes back from a call, so nothing local bounds its reference count
/// and the runtime test has to stay. `Wrap(N(42))` is also all-literal, which
/// is exactly the pooled-immortal parent the check exists for.
#[test]
fn cfg_match_keeps_the_runtime_check_for_a_call_result() {
    let compiled = compile_without_normalize(
        r#"
enum Leaf { N(Int) }
enum Boxed { Wrap(Leaf) }
fn unbox(value: Boxed) -> Leaf {
  match value { | Wrap(leaf) => leaf | _ => N(0) }
}
fn main() -> Int {
  let leaf = unbox(Wrap(N(42)));
  match leaf { | N(value) => value | _ => 0 }
}
"#,
    );
    assert_eq!(
        compiled
            .memory
            .reference_counting()
            .expect("ARC test must select reference counting")
            .arc
            .static_unique_takes,
        0
    );
    let bodies = emitted_bodies(compiled.c_source.as_str());
    assert!(bodies.contains("cielo_ctor_take_field("));
    assert!(!bodies.contains("cielo_ctor_take_field_unique("));
}

#[test]
fn cfg_arc_verifier_accepts_generated_plan() {
    let compiled = compile(
        r#"
enum Boxed { Wrap(Int) }
fn main() -> Int { let value = Wrap(1); match value { | Wrap(n) => n | _ => 0 } }
"#,
    );
    assert!(
        compiled
            .residual
            .diagnostics()
            .entries()
            .iter()
            .all(|diagnostic| {
                !diagnostic.code.starts_with("CFG_ARC_VERIFY")
                    && !diagnostic.code.starts_with("CFG_VERIFY")
            })
    );
}

/// CIELO-7: a value reachable only below a constructor used to get no ARC ops at
/// all, because the use collector stopped at the top-level node instead of
/// walking into the operands it owns.
#[test]
fn cfg_arc_retains_a_value_nested_below_a_constructor() {
    let mut cfg = CfgProgram::default();
    let empty = cfg.push_expr(
        CfgExpr::MakeStruct {
            ty: SymbolId::from_u32(1),
            fields: Vec::new(),
        },
        None,
    );
    let boxed = cfg.push_value(None);
    let boxed_expr = cfg.push_expr(CfgExpr::Value(boxed), None);
    let wrapper = cfg.push_expr(
        CfgExpr::MakeStruct {
            ty: SymbolId::from_u32(2),
            fields: vec![boxed_expr],
        },
        None,
    );

    let entry = cfg.push_block(Vec::new(), None);
    let define = cfg.push_instruction(
        entry,
        CfgInstruction::Let {
            result: boxed,
            value: empty,
        },
        None,
    );
    cfg.set_terminator(entry, CfgTerminator::Return(wrapper));

    let config = ArcConfig {
        features: ArcFeatures::INSERTION,
    };
    cielo_memory::refcount::passes::cfg_arc::run(
        &mut cfg,
        &[true],
        &ConstantTable::default(),
        &config,
    );

    let terminator_arc = &cfg.block(entry).expect("entry block").terminator_arc;
    assert_eq!(
        terminator_arc
            .pre
            .iter()
            .map(|op| (op.kind, op.value))
            .collect::<Vec<_>>(),
        vec![(CfgArcOpKind::Retain, boxed)],
        "the nested constructor field is consumed and must be retained"
    );
    assert_eq!(
        terminator_arc
            .post
            .iter()
            .map(|op| (op.kind, op.value))
            .collect::<Vec<_>>(),
        vec![(CfgArcOpKind::Release, boxed)]
    );
    let define = cfg.instruction(define).expect("defining instruction");
    assert!(
        define.arc.post.is_empty(),
        "the definition is still live at the terminator"
    );
}

/// Nothing lowers to `Switch` yet, so its ARC traversal is only reachable from
/// a hand-built CFG. A value live on one case and dead on the rest needs an
/// edge block per dead case, which also exercises edge redirection.
#[test]
fn cfg_arc_drops_on_dead_switch_edges() {
    let mut cfg = CfgProgram::default();
    let boxed = cfg.push_value(None);
    let selector_value = cfg.push_value(None);
    let boxed_expr = cfg.push_expr(CfgExpr::Value(boxed), None);
    let selector = cfg.push_expr(CfgExpr::Value(selector_value), None);
    let unit = cfg.push_expr(CfgExpr::Literal(Literal::Unit), None);

    let keep = cfg.push_block(Vec::new(), None);
    cfg.set_terminator(keep, CfgTerminator::Return(boxed_expr));
    let dead = cfg.push_block(Vec::new(), None);
    cfg.set_terminator(dead, CfgTerminator::Return(unit));
    let default = cfg.push_block(Vec::new(), None);
    cfg.set_terminator(default, CfgTerminator::Return(unit));
    let entry = cfg.push_block(Vec::new(), None);
    cfg.set_terminator(
        entry,
        CfgTerminator::Switch {
            selector,
            targets: vec![keep, dead],
            default,
        },
    );

    let config = ArcConfig {
        features: ArcFeatures::INSERTION | ArcFeatures::OPTIMIZATION,
    };
    cielo_memory::refcount::passes::cfg_arc::run(
        &mut cfg,
        &[true, false],
        &Default::default(),
        &config,
    );
    assert_eq!(cfg.validate(), Ok(()));

    let CfgTerminator::Switch {
        targets,
        default: fallback,
        ..
    } = &cfg.block(entry).expect("entry block").terminator
    else {
        panic!("switch terminator must survive ARC insertion");
    };
    assert_eq!(targets[0], keep, "the live case keeps its original target");
    assert_ne!(targets[1], dead, "the dead case is routed through an edge");
    assert_ne!(*fallback, default, "the default is routed through an edge");

    for edge in [targets[1], *fallback] {
        let ops = &cfg.block(edge).expect("edge block").entry_arc;
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].kind, CfgArcOpKind::Release);
        assert_eq!(ops[0].value, boxed);
    }
}
