use cielo::common::gc::ArcInsertRule;
use cielo::passes::arc_insert::{ArcDecisionOutcome, ArcDecisionReason};

use crate::helpers::arc::{
    alias_copy_decision, alias_copy_stmt_from_source, call_arg_decision, call_stmt_with_arg_var,
    call_stmts_with_arg_var, has_release_op, has_retain_op, managed_ctor_binding, plan_with_rules,
};

#[test]
fn arc_causal_last_use_move_rule_is_necessary_and_sufficient() {
    let source = r#"
enum Boxed { Wrap(Int) }
fn consume(v: Boxed) -> Int {
  match v {
    Wrap(n) => n,
  }
}
fn main() -> Int {
  let x = Wrap(1);
  consume(x)
}
"#;

    let move_enabled_rules = ArcInsertRule::all();
    let (program_move_on, plan_move_on) = plan_with_rules(source, move_enabled_rules);
    let x_var_move_on = managed_ctor_binding(&program_move_on);
    let call_stmt_move_on = call_stmt_with_arg_var(&program_move_on, x_var_move_on);
    assert!(
        !has_retain_op(&plan_move_on, call_stmt_move_on, x_var_move_on),
        "move-enabled run should not retain managed terminal call arg"
    );
    let move_on_decision = call_arg_decision(&plan_move_on, call_stmt_move_on, x_var_move_on)
        .expect("decision trace for move-enabled call arg");
    assert_eq!(
        move_on_decision.outcome,
        ArcDecisionOutcome::MoveToCallee,
        "move-enabled run should decide MoveToCallee"
    );
    assert_eq!(
        move_on_decision.reason,
        ArcDecisionReason::EligibleMove,
        "move-enabled run should record EligibleMove reason"
    );

    let move_disabled_rules = move_enabled_rules.difference(ArcInsertRule::CALL_ARG_LAST_USE_MOVE);
    let (program_move_off, plan_move_off) = plan_with_rules(source, move_disabled_rules);
    let x_var_move_off = managed_ctor_binding(&program_move_off);
    let call_stmt_move_off = call_stmt_with_arg_var(&program_move_off, x_var_move_off);
    assert!(
        has_retain_op(&plan_move_off, call_stmt_move_off, x_var_move_off),
        "move-disabled run must retain managed call arg at call boundary"
    );
    let move_off_decision = call_arg_decision(&plan_move_off, call_stmt_move_off, x_var_move_off)
        .expect("decision trace for move-disabled call arg");
    assert_eq!(
        move_off_decision.outcome,
        ArcDecisionOutcome::RetainCopy,
        "move-disabled run should fall back to retain/copy"
    );
    assert_eq!(
        move_off_decision.reason,
        ArcDecisionReason::MoveRuleDisabled,
        "move-disabled run should attribute behavior to MoveRuleDisabled"
    );
}

#[test]
fn arc_causal_live_alias_guard_blocks_unsound_move_when_enabled() {
    let source = r#"
enum Boxed { Wrap(Int) }
fn consume(v: Boxed) -> Int {
  match v {
    Wrap(n) => n,
  }
}
fn main() -> Int {
  let a = Wrap(1);
  let b = a;
  let consumed = consume(a);
  match b {
    Wrap(n) => consumed + n,
  }
}
"#;

    let rules_guard_on = ArcInsertRule::all();
    let (program_guard_on, plan_guard_on) = plan_with_rules(source, rules_guard_on);
    let a_var_guard_on = managed_ctor_binding(&program_guard_on);
    let call_stmt_guard_on = call_stmt_with_arg_var(&program_guard_on, a_var_guard_on);
    assert!(
        has_retain_op(&plan_guard_on, call_stmt_guard_on, a_var_guard_on),
        "live-alias guard enabled must block move and retain the call arg"
    );
    let guard_on_decision = call_arg_decision(&plan_guard_on, call_stmt_guard_on, a_var_guard_on)
        .expect("decision trace for guard-enabled run");
    assert_eq!(
        guard_on_decision.outcome,
        ArcDecisionOutcome::RetainCopy,
        "guard-enabled run should keep retain/copy behavior"
    );
    assert_eq!(
        guard_on_decision.reason,
        ArcDecisionReason::LiveAliasOut,
        "guard-enabled run should record LiveAliasOut blocker"
    );

    let rules_guard_off = rules_guard_on.difference(ArcInsertRule::CALL_ARG_ALIAS_LIVE_OUT_GUARD);
    let (program_guard_off, plan_guard_off) = plan_with_rules(source, rules_guard_off);
    let a_var_guard_off = managed_ctor_binding(&program_guard_off);
    let call_stmt_guard_off = call_stmt_with_arg_var(&program_guard_off, a_var_guard_off);
    assert!(
        !has_retain_op(&plan_guard_off, call_stmt_guard_off, a_var_guard_off),
        "live-alias guard disabled should expose unsound move behavior (retain removed)"
    );
    let guard_off_decision =
        call_arg_decision(&plan_guard_off, call_stmt_guard_off, a_var_guard_off)
            .expect("decision trace for guard-disabled run");
    assert_eq!(
        guard_off_decision.outcome,
        ArcDecisionOutcome::MoveToCallee,
        "guard-disabled run should move call arg"
    );
}

#[test]
fn arc_causal_live_alias_guard_has_no_effect_when_no_alias_is_live_out() {
    let source = r#"
enum Boxed { Wrap(Int) }
fn consume(v: Boxed) -> Int {
  match v {
    Wrap(n) => n,
  }
}
fn main() -> Int {
  let x = Wrap(1);
  consume(x)
}
"#;

    let rules_guard_on = ArcInsertRule::all();
    let (program_guard_on, plan_guard_on) = plan_with_rules(source, rules_guard_on);
    let x_var_guard_on = managed_ctor_binding(&program_guard_on);
    let call_stmt_guard_on = call_stmt_with_arg_var(&program_guard_on, x_var_guard_on);
    let decision_guard_on = call_arg_decision(&plan_guard_on, call_stmt_guard_on, x_var_guard_on)
        .expect("decision trace for guard-enabled no-alias run");

    let rules_guard_off = rules_guard_on.difference(ArcInsertRule::CALL_ARG_ALIAS_LIVE_OUT_GUARD);
    let (program_guard_off, plan_guard_off) = plan_with_rules(source, rules_guard_off);
    let x_var_guard_off = managed_ctor_binding(&program_guard_off);
    let call_stmt_guard_off = call_stmt_with_arg_var(&program_guard_off, x_var_guard_off);
    let decision_guard_off =
        call_arg_decision(&plan_guard_off, call_stmt_guard_off, x_var_guard_off)
            .expect("decision trace for guard-disabled no-alias run");

    assert_eq!(
        decision_guard_on.outcome,
        ArcDecisionOutcome::MoveToCallee,
        "with no live alias out, guard-on should still move"
    );
    assert_eq!(
        decision_guard_off.outcome,
        ArcDecisionOutcome::MoveToCallee,
        "with no live alias out, guard-off should match guard-on behavior"
    );
    assert_eq!(
        has_retain_op(&plan_guard_on, call_stmt_guard_on, x_var_guard_on),
        has_retain_op(&plan_guard_off, call_stmt_guard_off, x_var_guard_off),
        "guard toggle should not change retain planning when no live alias exists"
    );
}

#[test]
fn arc_causal_multiple_call_arg_uses_block_last_use_move() {
    let control_source = r#"
enum Boxed { Wrap(Int) }
fn consume(v: Boxed) -> Int {
  match v {
    Wrap(n) => n,
  }
}
fn use2(a: Boxed, b: Boxed) -> Int {
  consume(a) + consume(b)
}
fn main() -> Int {
  let x = Wrap(1);
  let y = Wrap(2);
  use2(x, y)
}
"#;
    let variant_source = r#"
enum Boxed { Wrap(Int) }
fn consume(v: Boxed) -> Int {
  match v {
    Wrap(n) => n,
  }
}
fn use2(a: Boxed, b: Boxed) -> Int {
  consume(a) + consume(b)
}
fn main() -> Int {
  let x = Wrap(1);
  use2(x, x)
}
"#;

    let (program_control, plan_control) = plan_with_rules(control_source, ArcInsertRule::all());
    let x_var_control = managed_ctor_binding(&program_control);
    let call_stmt_control = call_stmt_with_arg_var(&program_control, x_var_control);
    let control_decision = call_arg_decision(&plan_control, call_stmt_control, x_var_control)
        .expect("decision trace for single-use call arg");
    assert_eq!(
        control_decision.outcome,
        ArcDecisionOutcome::MoveToCallee,
        "single call-arg use should stay move-eligible"
    );
    assert_eq!(
        control_decision.reason,
        ArcDecisionReason::EligibleMove,
        "single call-arg use should be attributed to EligibleMove"
    );

    let (program_variant, plan_variant) = plan_with_rules(variant_source, ArcInsertRule::all());
    let x_var_variant = managed_ctor_binding(&program_variant);
    let call_stmt_variant = call_stmt_with_arg_var(&program_variant, x_var_variant);
    let variant_decision = call_arg_decision(&plan_variant, call_stmt_variant, x_var_variant)
        .expect("decision trace for duplicate call-arg uses");
    assert!(
        has_retain_op(&plan_variant, call_stmt_variant, x_var_variant),
        "duplicate call-arg uses must force retain/copy fallback at call boundary"
    );
    assert_eq!(
        variant_decision.outcome,
        ArcDecisionOutcome::RetainCopy,
        "duplicate call-arg uses should disable move"
    );
    assert_eq!(
        variant_decision.reason,
        ArcDecisionReason::MultipleCallArgUses,
        "duplicate call-arg uses should be blocked by MultipleCallArgUses"
    );
}

#[test]
fn arc_causal_non_call_read_in_same_stmt_blocks_last_use_move() {
    let control_source = r#"
enum Boxed { Wrap(Int) }
enum PairBoxed { Both(Int, Boxed) }
fn consume(v: Boxed) -> Int {
  match v {
    Wrap(n) => n,
  }
}
fn head(p: PairBoxed) -> Int {
  match p {
    Both(n, extra) => n,
  }
}
fn main() -> Int {
  let x = Wrap(1);
  let p = Both(consume(x), Wrap(2));
  head(p)
}
"#;
    let variant_source = r#"
enum Boxed { Wrap(Int) }
enum PairBoxed { Both(Int, Boxed) }
fn consume(v: Boxed) -> Int {
  match v {
    Wrap(n) => n,
  }
}
fn head(p: PairBoxed) -> Int {
  match p {
    Both(n, extra) => n,
  }
}
fn main() -> Int {
  let x = Wrap(1);
  let p = Both(consume(x), x);
  head(p)
}
"#;

    let (program_control, plan_control) = plan_with_rules(control_source, ArcInsertRule::all());
    let x_var_control = managed_ctor_binding(&program_control);
    let call_stmt_control = call_stmt_with_arg_var(&program_control, x_var_control);
    let control_decision = call_arg_decision(&plan_control, call_stmt_control, x_var_control)
        .expect("decision trace for control call arg");
    assert_eq!(
        control_decision.outcome,
        ArcDecisionOutcome::MoveToCallee,
        "single call-use with no extra same-stmt reads should remain move-eligible"
    );
    assert!(
        !has_retain_op(&plan_control, call_stmt_control, x_var_control),
        "control run should not retain move-eligible call arg"
    );

    let (program_variant, plan_variant) = plan_with_rules(variant_source, ArcInsertRule::all());
    let x_var_variant = managed_ctor_binding(&program_variant);
    let call_stmt_variant = call_stmt_with_arg_var(&program_variant, x_var_variant);
    let variant_decision = call_arg_decision(&plan_variant, call_stmt_variant, x_var_variant)
        .expect("decision trace for mixed-read call arg");
    assert!(
        has_retain_op(&plan_variant, call_stmt_variant, x_var_variant),
        "mixed same-stmt read must force retain/copy fallback"
    );
    assert_eq!(
        variant_decision.outcome,
        ArcDecisionOutcome::RetainCopy,
        "extra non-call read should disable move"
    );
    assert_eq!(
        variant_decision.reason,
        ArcDecisionReason::MultipleStmtUses,
        "extra non-call read should be attributed to MultipleStmtUses"
    );
}

#[test]
fn arc_causal_non_call_read_before_consume_uses_keepalive_move_compensation() {
    let source = r#"
enum Boxed { Wrap(Int) }
enum PairBoxed { Both(Boxed, Int) }
fn consume(v: Boxed) -> Int {
  match v {
    Wrap(n) => n,
  }
}
fn head(p: PairBoxed) -> Int {
  match p {
    Both(kept, n) => n,
  }
}
fn main() -> Int {
  let x = Wrap(1);
  head(Both(x, consume(x)))
}
"#;

    let (program, plan) = plan_with_rules(source, ArcInsertRule::all());
    let x_var = managed_ctor_binding(&program);
    let call_stmt = call_stmt_with_arg_var(&program, x_var);
    let decision =
        call_arg_decision(&plan, call_stmt, x_var).expect("decision trace for consume(x)");
    assert_eq!(
        decision.outcome,
        ArcDecisionOutcome::MoveToCallee,
        "pre-call non-call reads should still permit move-to-callee"
    );
    assert_eq!(
        decision.reason,
        ArcDecisionReason::EligibleMove,
        "keepalive-compensated pre-call reads should remain EligibleMove"
    );
    assert!(
        has_retain_op(&plan, call_stmt, x_var),
        "pre-call mixed reads should add keepalive retain when move is selected"
    );
    assert!(
        has_release_op(&plan, call_stmt, x_var),
        "keepalive retain for pre-call mixed reads must be balanced by post-release"
    );
}

#[test]
fn arc_causal_not_last_use_blocks_first_call_until_terminal_use() {
    let source = r#"
enum Boxed { Wrap(Int) }
fn consume(v: Boxed) -> Int {
  match v {
    Wrap(n) => n,
  }
}
fn main() -> Int {
  let x = Wrap(1);
  let a = consume(x);
  let b = consume(x);
  a + b
}
"#;

    let (program, plan) = plan_with_rules(source, ArcInsertRule::all());
    let x_var = managed_ctor_binding(&program);
    let call_stmts = call_stmts_with_arg_var(&program, x_var);
    assert_eq!(
        call_stmts.len(),
        2,
        "fixture should contain exactly two call sites consuming x"
    );

    let mut saw_not_last_use = false;
    let mut saw_terminal_move = false;
    for stmt in call_stmts {
        let decision =
            call_arg_decision(&plan, stmt, x_var).expect("decision trace for consume(x) call");
        match (decision.outcome, decision.reason) {
            (ArcDecisionOutcome::RetainCopy, ArcDecisionReason::NotLastUse) => {
                saw_not_last_use = true;
                assert!(
                    has_retain_op(&plan, stmt, x_var),
                    "non-terminal consume(x) should retain x"
                );
            }
            (ArcDecisionOutcome::MoveToCallee, ArcDecisionReason::EligibleMove) => {
                saw_terminal_move = true;
                assert!(
                    !has_retain_op(&plan, stmt, x_var),
                    "terminal consume(x) should move x without retain"
                );
            }
            other => panic!("unexpected decision combination for consume(x): {other:?}"),
        }
    }
    assert!(
        saw_not_last_use,
        "one consume(x) site must be blocked by NotLastUse"
    );
    assert!(
        saw_terminal_move,
        "one consume(x) site must remain EligibleMove"
    );
}

#[test]
fn arc_causal_alias_move_source_rule_is_necessary_and_sufficient() {
    let source = r#"
enum Boxed { Wrap(Int) }
fn main() -> Int {
  let a = Wrap(1);
  let b = a;
  match b {
    Wrap(n) => n,
  }
}
"#;

    let rules_move_on = ArcInsertRule::all();
    let (program_move_on, plan_move_on) = plan_with_rules(source, rules_move_on);
    let source_var_move_on = managed_ctor_binding(&program_move_on);
    let (alias_stmt_move_on, binding_var_move_on) =
        alias_copy_stmt_from_source(&program_move_on, source_var_move_on);
    let decision_move_on = alias_copy_decision(
        &plan_move_on,
        alias_stmt_move_on,
        source_var_move_on,
        binding_var_move_on,
    )
    .expect("decision trace for alias move-source enabled");
    assert_eq!(
        decision_move_on.outcome,
        ArcDecisionOutcome::MoveSource,
        "move-source rule enabled should transfer ownership from source into alias"
    );
    assert_eq!(
        decision_move_on.reason,
        ArcDecisionReason::MoveSourceLastUse,
        "move-source transfer should be attributed to MoveSourceLastUse"
    );
    assert!(
        !has_retain_op(&plan_move_on, alias_stmt_move_on, source_var_move_on),
        "move-source path should not retain copied source"
    );

    let rules_move_off = rules_move_on.difference(ArcInsertRule::ALIAS_COPY_MOVE_SOURCE);
    let (program_move_off, plan_move_off) = plan_with_rules(source, rules_move_off);
    let source_var_move_off = managed_ctor_binding(&program_move_off);
    let (alias_stmt_move_off, binding_var_move_off) =
        alias_copy_stmt_from_source(&program_move_off, source_var_move_off);
    let decision_move_off = alias_copy_decision(
        &plan_move_off,
        alias_stmt_move_off,
        source_var_move_off,
        binding_var_move_off,
    )
    .expect("decision trace for alias move-source disabled");
    assert!(
        has_retain_op(&plan_move_off, alias_stmt_move_off, source_var_move_off),
        "move-source rule disabled must retain/copy source"
    );
    assert_eq!(
        decision_move_off.outcome,
        ArcDecisionOutcome::RetainCopy,
        "move-source rule disabled should fall back to retain/copy"
    );
    assert_eq!(
        decision_move_off.reason,
        ArcDecisionReason::AliasRuleDisabledFallback,
        "move-source rule-disabled fallback reason should be explicit"
    );
}

#[test]
fn arc_causal_dead_binding_drop_rule_is_necessary_and_sufficient() {
    let source = r#"
enum Boxed { Wrap(Int) }
fn main() -> Int {
  let a = Wrap(1);
  let b = a;
  0
}
"#;

    let rules_drop_on = ArcInsertRule::all();
    let (program_drop_on, plan_drop_on) = plan_with_rules(source, rules_drop_on);
    let source_var_drop_on = managed_ctor_binding(&program_drop_on);
    let (alias_stmt_drop_on, binding_var_drop_on) =
        alias_copy_stmt_from_source(&program_drop_on, source_var_drop_on);
    let decision_drop_on = alias_copy_decision(
        &plan_drop_on,
        alias_stmt_drop_on,
        source_var_drop_on,
        binding_var_drop_on,
    )
    .expect("decision trace for dead-binding-drop enabled");
    assert_eq!(
        decision_drop_on.outcome,
        ArcDecisionOutcome::DropDeadBinding,
        "dead-binding-drop rule enabled should suppress dead alias churn"
    );
    assert_eq!(
        decision_drop_on.reason,
        ArcDecisionReason::DeadBindingDrop,
        "dead-binding-drop path should record DeadBindingDrop reason"
    );
    assert!(
        !has_retain_op(&plan_drop_on, alias_stmt_drop_on, source_var_drop_on),
        "dead-binding-drop path should not retain source"
    );
    assert!(
        !has_release_op(&plan_drop_on, alias_stmt_drop_on, binding_var_drop_on),
        "dead-binding-drop path should suppress release of dead destination binding"
    );

    let rules_drop_off = rules_drop_on.difference(ArcInsertRule::ALIAS_COPY_DROP_DEAD_BINDING);
    let (program_drop_off, plan_drop_off) = plan_with_rules(source, rules_drop_off);
    let source_var_drop_off = managed_ctor_binding(&program_drop_off);
    let (alias_stmt_drop_off, binding_var_drop_off) =
        alias_copy_stmt_from_source(&program_drop_off, source_var_drop_off);
    let decision_drop_off = alias_copy_decision(
        &plan_drop_off,
        alias_stmt_drop_off,
        source_var_drop_off,
        binding_var_drop_off,
    )
    .expect("decision trace for dead-binding-drop disabled");
    assert!(
        has_retain_op(&plan_drop_off, alias_stmt_drop_off, source_var_drop_off),
        "dead-binding-drop disabled should retain/copy source"
    );
    assert!(
        has_release_op(&plan_drop_off, alias_stmt_drop_off, binding_var_drop_off),
        "dead-binding-drop disabled should release dead destination binding"
    );
    assert_eq!(
        decision_drop_off.outcome,
        ArcDecisionOutcome::RetainCopy,
        "dead-binding-drop disabled should fall back to retain/copy"
    );
    assert_eq!(
        decision_drop_off.reason,
        ArcDecisionReason::AliasRuleDisabledFallback,
        "dead-binding-drop disabled should expose AliasRuleDisabledFallback reason"
    );
}
