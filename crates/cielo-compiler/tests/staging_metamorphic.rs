use std::collections::{BTreeMap, HashMap, HashSet};

#[path = "helpers/mod.rs"]
mod helpers;

use cielo_base::{ExprId, FuncId, SourceId, StmtId, VarId};
use cielo_base::Interner;
use cielo_ir::core::{CoreProgram, ExprKind};
use cielo_staging::pipeline::phases::{BranchDecision, Knownness, Reason, Residualized, Stage};
use cielo::{Compiler, CompilerConfig};
use helpers::ir::{first_return_expr, reachable_stmt_count_from_root};

#[derive(Debug, PartialEq, Eq)]
struct StageSignature {
    ct_exprs: usize,
    rt_exprs: usize,
    rt_reasons: BTreeMap<&'static str, usize>,
    knownness_unknown: usize,
    knownness_local: usize,
    knownness_persistable: usize,
    ct_cache_entries: usize,
    branch_live_true: usize,
    branch_live_false: usize,
    branch_unknown: usize,
}

#[test]
fn staging_is_alpha_rename_invariant() {
    let src_a = r#"
fn add_one(x: Int) -> Int {
  let y = x + 1;
  y
}

fn main() -> Int {
  let a = 3;
  add_one(a)
}
"#;
    let src_b = r#"
fn add_one(value: Int) -> Int {
  let tmp = value + 1;
  tmp
}

fn main() -> Int {
  let input = 3;
  add_one(input)
}
"#;

    let compiler = Compiler::new(CompilerConfig::default());
    let mut interner_a = Interner::new();
    let residual_a = compiler.compile_source(src_a, SourceId::from_u32(0), &mut interner_a);
    let mut interner_b = Interner::new();
    let residual_b = compiler.compile_source(src_b, SourceId::from_u32(1), &mut interner_b);

    assert_eq!(
        stage_signature(&residual_a),
        stage_signature(&residual_b),
        "alpha-renaming locals must not change staging classifications"
    );

    let mut emit_interner_a = Interner::new();
    let emitted_a =
        compiler.compile_source_to_c(src_a, SourceId::from_u32(2), &mut emit_interner_a);
    let mut emit_interner_b = Interner::new();
    let emitted_b =
        compiler.compile_source_to_c(src_b, SourceId::from_u32(3), &mut emit_interner_b);
    assert_eq!(
        emitted_a.c_source, emitted_b.c_source,
        "alpha-renaming locals must not change emitted C"
    );
}

#[test]
fn staging_is_discardable_dead_code_insertion_invariant() {
    let src_base = r#"
fn main() -> Int {
  let x = 1 + 2;
  x
}
"#;
    let src_with_dead = r#"
fn main() -> Int {
  let dead = 99;
  let x = 1 + 2;
  x
}
"#;

    let compiler = Compiler::new(CompilerConfig::default());
    let mut interner_base = Interner::new();
    let residual_base =
        compiler.compile_source(src_base, SourceId::from_u32(10), &mut interner_base);
    let mut interner_dead = Interner::new();
    let residual_dead =
        compiler.compile_source(src_with_dead, SourceId::from_u32(11), &mut interner_dead);

    let base_main = main_func_id(residual_base.program(), &interner_base);
    let dead_main = main_func_id(residual_dead.program(), &interner_dead);
    let base_return_expr = first_return_expr(
        residual_base.program(),
        residual_base
            .program()
            .function(base_main)
            .expect("base main")
            .body,
    )
    .expect("base main return expression");
    let dead_return_expr = first_return_expr(
        residual_dead.program(),
        residual_dead
            .program()
            .function(dead_main)
            .expect("dead main")
            .body,
    )
    .expect("dead main return expression");

    assert_eq!(
        residual_base
            .bta()
            .stage_of_expr
            .get(&base_return_expr)
            .copied(),
        residual_dead
            .bta()
            .stage_of_expr
            .get(&dead_return_expr)
            .copied(),
        "inserting discardable dead code should not change stage of the live return expression"
    );

    let mut emit_interner_base = Interner::new();
    let emitted_base =
        compiler.compile_source_to_c(src_base, SourceId::from_u32(12), &mut emit_interner_base);
    let mut emit_interner_dead = Interner::new();
    let emitted_dead = compiler.compile_source_to_c(
        src_with_dead,
        SourceId::from_u32(13),
        &mut emit_interner_dead,
    );
    let emitted_base_main = main_func_id(emitted_base.residual.program(), &emit_interner_base);
    let emitted_dead_main = main_func_id(emitted_dead.residual.program(), &emit_interner_dead);
    let emitted_base_main_body = emitted_base
        .residual
        .program()
        .function(emitted_base_main)
        .expect("emitted base main")
        .body;
    let emitted_dead_main_body = emitted_dead
        .residual
        .program()
        .function(emitted_dead_main)
        .expect("emitted dead main")
        .body;

    assert_eq!(
        reachable_stmt_count_from_root(emitted_base.residual.program(), emitted_base_main_body),
        reachable_stmt_count_from_root(emitted_dead.residual.program(), emitted_dead_main_body),
        "discardable dead-code insertion should not change normalized main statement shape"
    );
    assert_eq!(
        resolved_main_return_int_literal(emitted_base.residual.program(), emitted_base_main_body),
        resolved_main_return_int_literal(emitted_dead.residual.program(), emitted_dead_main_body),
        "discardable dead-code insertion should preserve normalized return value"
    );
}

#[test]
fn staging_is_equivalent_control_flow_reshape_invariant() {
    let src_direct = r#"
fn main() -> Int {
  let x = 1 + 2;
  x
}
"#;
    let src_branch = r#"
fn main() -> Int {
  let x = if true { 1 + 2 } else { 0 };
  x
}
"#;

    let compiler = Compiler::new(CompilerConfig::default());
    let mut interner_direct = Interner::new();
    let residual_direct =
        compiler.compile_source(src_direct, SourceId::from_u32(20), &mut interner_direct);
    let mut interner_branch = Interner::new();
    let residual_branch =
        compiler.compile_source(src_branch, SourceId::from_u32(21), &mut interner_branch);

    let direct_main = main_func_id(residual_direct.program(), &interner_direct);
    let branch_main = main_func_id(residual_branch.program(), &interner_branch);
    let direct_body = residual_direct
        .program()
        .function(direct_main)
        .expect("direct main")
        .body;
    let branch_body = residual_branch
        .program()
        .function(branch_main)
        .expect("branch main")
        .body;
    let direct_ret_expr =
        first_return_expr(residual_direct.program(), direct_body).expect("direct return expr");
    let branch_ret_expr =
        first_return_expr(residual_branch.program(), branch_body).expect("branch return expr");

    assert_eq!(
        residual_direct
            .bta()
            .stage_of_expr
            .get(&direct_ret_expr)
            .copied(),
        residual_branch
            .bta()
            .stage_of_expr
            .get(&branch_ret_expr)
            .copied(),
        "equivalent control-flow reshaping should preserve stage at the live return expression"
    );
    assert_eq!(
        residual_direct
            .bta()
            .knownness_of_expr
            .get(&direct_ret_expr)
            .copied(),
        residual_branch
            .bta()
            .knownness_of_expr
            .get(&branch_ret_expr)
            .copied(),
        "equivalent control-flow reshaping should preserve knownness at the live return expression"
    );
}

fn stage_signature(residual: &Residualized) -> StageSignature {
    let mut ct_exprs = 0usize;
    let mut rt_exprs = 0usize;
    let mut rt_reasons = BTreeMap::new();
    for stage in residual.bta().stage_of_expr.values() {
        match stage {
            Stage::Ct => ct_exprs += 1,
            Stage::Rt(reason) => {
                rt_exprs += 1;
                let tag = reason_tag(*reason);
                *rt_reasons.entry(tag).or_insert(0) += 1;
            }
        }
    }

    let mut knownness_unknown = 0usize;
    let mut knownness_local = 0usize;
    let mut knownness_persistable = 0usize;
    for knownness in residual.bta().knownness_of_expr.values() {
        match knownness {
            Knownness::Unknown => knownness_unknown += 1,
            Knownness::KnownLocal => knownness_local += 1,
            Knownness::KnownPersistable => knownness_persistable += 1,
        }
    }

    let mut branch_live_true = 0usize;
    let mut branch_live_false = 0usize;
    let mut branch_unknown = 0usize;
    for decision in residual.ct().branch_decisions.values() {
        match decision {
            BranchDecision::LiveTrue => branch_live_true += 1,
            BranchDecision::LiveFalse => branch_live_false += 1,
            BranchDecision::Unknown => branch_unknown += 1,
        }
    }

    StageSignature {
        ct_exprs,
        rt_exprs,
        rt_reasons,
        knownness_unknown,
        knownness_local,
        knownness_persistable,
        ct_cache_entries: residual.ct().ct_cache.len(),
        branch_live_true,
        branch_live_false,
        branch_unknown,
    }
}

fn reason_tag(reason: Reason) -> &'static str {
    match reason {
        Reason::UnclassifiedRuntime => "unclassified-runtime",
        Reason::Parameter { .. } => "parameter",
        Reason::DependsOnVar(_) => "depends-on-var",
        Reason::EffectNotDischarged(_) => "effect-not-discharged",
        Reason::HandlerIsRuntime(_) => "handler-is-runtime",
        Reason::BranchOnRuntime(_) => "branch-on-runtime",
        Reason::NotPersistable(_) => "not-persistable",
        Reason::UserForcedRuntime => "user-forced-runtime",
        Reason::CtOnlyWithRuntimeArgs(_) => "ct-only-with-runtime-args",
    }
}

fn main_func_id(program: &CoreProgram, interner: &Interner) -> FuncId {
    program
        .functions()
        .iter()
        .enumerate()
        .find_map(|(idx, function)| {
            (interner.resolve(function.name) == Some("main")).then_some(FuncId::new(idx))
        })
        .expect("main function should exist")
}

fn resolved_main_return_int_literal(program: &CoreProgram, root: StmtId) -> Option<i64> {
    let mut let_defs = HashMap::<VarId, ExprId>::new();
    let mut stack = vec![root];
    let mut seen = HashSet::new();
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let stmt = program.stmt(stmt_id)?;
        match stmt.kind {
            cielo_ir::core::StmtKind::Let {
                binding,
                value,
                next,
            } => {
                let_defs.insert(binding, value);
                stack.push(next);
            }
            cielo_ir::core::StmtKind::Return(expr) => {
                return resolve_int_literal(program, expr, &let_defs);
            }
            _ => stack.extend(stmt.child_stmts()),
        }
    }
    None
}

fn resolve_int_literal(
    program: &CoreProgram,
    expr_id: ExprId,
    let_defs: &HashMap<VarId, ExprId>,
) -> Option<i64> {
    let mut current = expr_id;
    let mut seen_vars = HashSet::new();
    loop {
        let expr = program.expr(current)?;
        match expr.kind {
            ExprKind::Literal(cielo_ir::core::Literal::Int(value)) => return Some(value),
            ExprKind::Var(var) => {
                if !seen_vars.insert(var) {
                    return None;
                }
                current = *let_defs.get(&var)?;
            }
            _ => return None,
        }
    }
}
