use cielo::common::ids::SourceId;
use cielo::common::symbols::Interner;
use cielo::ir::core::{ExprKind, Literal};
use cielo::{Compiler, CompilerConfig};

#[test]
fn cteval_folds_pure_call_with_literal_arguments() {
    let src = r#"
fn add2(x: Int) -> Int {
  x + 2
}

fn main() -> Int {
  add2(40)
}
"#;

    let compiler = Compiler::new(CompilerConfig::default());
    let mut interner = Interner::new();
    let core = compiler.parse_and_lower_to_core(src, SourceId::from_u32(0), &mut interner);
    let staged = compiler.run_v1_ct_eval(core);

    let call_exprs = staged
        .program()
        .exprs()
        .iter()
        .enumerate()
        .filter_map(|(idx, expr)| {
            matches!(expr.kind, ExprKind::PureCall { .. })
                .then_some(cielo::common::ids::ExprId::new(idx))
        })
        .collect::<Vec<_>>();
    assert!(
        !call_exprs.is_empty(),
        "fixture must contain at least one pure call expression"
    );

    assert!(
        call_exprs
            .iter()
            .any(|expr_id| staged.ct().ct_cache.get(expr_id) == Some(&Literal::Int(42))),
        "ct evaluator should fold known-arg pure call to literal 42"
    );
}

#[test]
fn cteval_folds_pure_call_through_let_chain_in_callee() {
    let src = r#"
fn bump_twice(x: Int) -> Int {
  let y = x + 1;
  y + 1
}

fn main() -> Int {
  bump_twice(40)
}
"#;

    let compiler = Compiler::new(CompilerConfig::default());
    let mut interner = Interner::new();
    let core = compiler.parse_and_lower_to_core(src, SourceId::from_u32(0), &mut interner);
    let staged = compiler.run_v1_ct_eval(core);

    let folded_values = staged.ct().ct_cache.values().collect::<Vec<_>>();
    assert!(
        folded_values.contains(&&Literal::Int(42)),
        "ct evaluator should execute let-chain body and fold call result"
    );
}

#[test]
fn cteval_keeps_recursive_pure_call_runtime_when_cycle_detected() {
    let src = r#"
fn loop(x: Int) -> Int {
  loop(x)
}

fn main() -> Int {
  loop(1)
}
"#;

    let compiler = Compiler::new(CompilerConfig::default());
    let mut interner = Interner::new();
    let core = compiler.parse_and_lower_to_core(src, SourceId::from_u32(0), &mut interner);
    let staged = compiler.run_v1_ct_eval(core);

    let recursive_calls = staged
        .program()
        .exprs()
        .iter()
        .enumerate()
        .filter_map(|(idx, expr)| {
            let ExprKind::PureCall { callee, .. } = expr.kind else {
                return None;
            };
            (callee.index() == 0).then_some(cielo::common::ids::ExprId::new(idx))
        })
        .collect::<Vec<_>>();
    assert!(
        !recursive_calls.is_empty(),
        "fixture must contain recursive pure-call expressions"
    );

    for expr_id in recursive_calls {
        assert!(
            staged.ct().ct_cache.get(&expr_id).is_none(),
            "recursive call expression e{} should not be folded in v1 evaluator slice",
            expr_id.as_u32()
        );
    }
}
