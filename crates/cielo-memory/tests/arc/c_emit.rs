use crate::helpers::c_emit::assert_arc_trace_comments_align;
use crate::helpers::core::compile_source_to_c_with_config;
use cielo_memory::{ArcFeatures, MemoryPreset};
use cielo_test_support::CompilerConfig;

#[test]
fn arc_c_emitter_trace_comments_align_with_runtime_calls() {
    let src = r#"
enum Boxed { Wrap(Int) }
fn consume(v: Boxed) -> Int {
  let one = 1;
  let two = one + 1;
  let three = two + 1;
  let four = three + 1;
  let five = four + 1;
  match v {
    Wrap(n) => n + five,
  }
}
fn main() -> Int {
  let b = Wrap(1);
  consume(b)
}
"#;

    let mut config = CompilerConfig::default().with_memory_preset(MemoryPreset::ArcRaw);
    config
        .memory
        .reference_counting_mut()
        .expect("ARC preset should select reference counting")
        .features
        .insert(ArcFeatures::EMIT_TRACE_COMMENTS);
    let compiled = compile_source_to_c_with_config(src, config);
    let arc_stats = compiled.memory.arc;
    assert!(
        arc_stats.final_retain_ops + arc_stats.final_release_ops > 0,
        "fixture must produce ARC ops before C emission validation"
    );
    let cfg_arc_ops = compiled
        .cfg
        .blocks()
        .iter()
        .map(|block| {
            block.entry_arc.len()
                + block.terminator_arc.pre.len()
                + block.terminator_arc.post.len()
                + block
                    .instructions
                    .iter()
                    .filter_map(|instruction| compiled.cfg.instruction(*instruction))
                    .map(|instruction| instruction.arc.pre.len() + instruction.arc.post.len())
                    .sum::<usize>()
        })
        .sum::<usize>();
    assert!(
        cfg_arc_ops > 0,
        "CFG should carry ARC ops to emission boundary"
    );
    let trace_comment_count = compiled.c_source.matches("/* arc ").count();
    assert!(
        trace_comment_count > 0,
        "expected ARC trace comments in emitted C when trace flag is enabled"
    );
    assert_arc_trace_comments_align(compiled.c_source.as_str());
}
