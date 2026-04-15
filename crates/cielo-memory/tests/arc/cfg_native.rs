use cielo_base::Interner;
use cielo_base::SourceId;
use cielo_ir::cfg::{CfgArcOpKind, CfgProjectionMode, CfgTerminator};
use cielo_memory::MemoryPreset;
use cielo_runtime::{cfg_lower, linearize};
use cielo_test_support::{Compiler, CompilerConfig};

use crate::helpers::core::emit_pipeline;

fn compile(source: &str) -> cielo_test_support::CompiledC {
    let mut interner = Interner::new();
    Compiler::new(CompilerConfig::default()).compile_source_to_c(
        source,
        SourceId::from_u32(0),
        &mut interner,
    )
}

fn compile_with_preset(source: &str, preset: MemoryPreset) -> cielo_test_support::CompiledC {
    let mut interner = Interner::new();
    Compiler::new(CompilerConfig::default().with_memory_preset(preset)).compile_source_to_c(
        source,
        SourceId::from_u32(0),
        &mut interner,
    )
}

fn compile_without_normalize(source: &str) -> cielo_test_support::CompiledC {
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let core = compiler.parse_and_lower_to_core(source, SourceId::from_u32(0), &mut interner);
    let mut residual = compiler.run_v1_core_pipeline(core);
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
    assert!(compiled.memory.arc.eliminated_move_pairs > 0);
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
    assert!(optimized.memory.arc.eliminated_move_pairs > raw.memory.arc.eliminated_move_pairs);
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
