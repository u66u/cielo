use std::collections::HashSet;

use cielo::common::diagnostics::DiagnosticBag;
use cielo::common::ids::{FuncId, SourceId, StmtId, SymbolId, VarId};
use cielo::common::span::Span;
use cielo::common::symbols::Interner;
use cielo::ir::core::{
    CoreProgram, CoreTypeRef, ExprKind, ExprNode, FunctionDecl, Literal, PrimitiveTypeRef,
    StmtKind, StmtNode,
};
use cielo::passes::normalize;
use cielo::pipeline::phases::{
    BtaTables, CtPropagationTables, MonomorphizationSummary, ResidualTables, Residualized,
    SemanticTables,
};
use cielo::sema::effect::SortedEffectRow;
use cielo::{Compiler, CompilerConfig};

#[test]
fn normalize_shrink_is_monotone_and_idempotent() {
    let src = r#"
fn helper() -> Int {
  7
}

fn main() -> Int {
  let dead = 1;
  let x = helper();
  x
}
"#;
    let compiler = Compiler::new(CompilerConfig::default());
    let mut interner = Interner::new();
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);

    let before_count = reachable_stmt_count(residual.program());
    let once = normalize::run(residual.clone());
    let once_count = reachable_stmt_count(once.program());
    let twice = normalize::run(once.clone());
    let twice_count = reachable_stmt_count(twice.program());

    assert!(
        once_count <= before_count,
        "normalize shrink phases should not increase reachable statement count ({once_count} > {before_count})"
    );
    assert_eq!(
        once_count, twice_count,
        "normalize should reach an idempotent statement-count fixpoint after one full sandwich run"
    );
    assert_eq!(
        format!("{:?}", once.program()),
        format!("{:?}", twice.program()),
        "running normalize twice should leave core program shape unchanged"
    );
}

#[test]
fn normalize_inlines_pure_call_expr_and_prunes_constant_if() {
    let span = Span::synthetic();
    let mut program = CoreProgram::new();

    let cond_true = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Bool(true)),
    });
    let cond_ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(cond_true),
    });
    let cond_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(10),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Bool),
        declared_effects: SortedEffectRow::empty(),
        body: cond_ret,
        ct_only: false,
        span,
    });

    let one = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(1)),
    });
    let two = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(2)),
    });
    let cond_var = VarId::from_u32(0);
    let cond_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(cond_var),
    });
    let then_ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(one),
    });
    let else_ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(two),
    });
    let branch = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::If {
            cond: cond_expr,
            then_branch: then_ret,
            else_branch: else_ret,
        },
    });
    let main_body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Call {
            result: cond_var,
            callee: cond_id,
            args: Vec::new(),
            effects: SortedEffectRow::empty(),
            next: branch,
        },
    });
    let main_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(11),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: main_body,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let normalized = normalize::run(residualized(program));
    let normalized_main = normalized
        .program()
        .function(main_id)
        .expect("main should exist")
        .body;

    assert!(
        !contains_if_stmt(normalized.program(), normalized_main),
        "after inlining the pure condition helper, normalize should prune the constant if branch"
    );
    assert!(
        !contains_call_to(normalized.program(), normalized_main, cond_id),
        "helper call should be removed after inline+shrink in normalize"
    );
}

#[test]
fn normalize_keeps_recursive_callee_callsites() {
    let span = Span::synthetic();
    let mut program = CoreProgram::new();

    let loop_placeholder_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(0)),
    });
    let loop_placeholder_body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(loop_placeholder_expr),
    });
    let loop_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(20),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: loop_placeholder_body,
        ct_only: false,
        span,
    });

    let loop_call_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::PureCall {
            callee: loop_id,
            args: Vec::new(),
        },
    });
    let loop_recursive_body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(loop_call_expr),
    });
    program.function_mut(loop_id).expect("loop").body = loop_recursive_body;

    let main_result = VarId::from_u32(1);
    let main_result_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(main_result),
    });
    let main_ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(main_result_expr),
    });
    let main_body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Call {
            result: main_result,
            callee: loop_id,
            args: Vec::new(),
            effects: SortedEffectRow::empty(),
            next: main_ret,
        },
    });
    let main_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(21),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: main_body,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let normalized = normalize::run(residualized(program));
    let normalized_main = normalized
        .program()
        .function(main_id)
        .expect("main should exist")
        .body;
    assert!(
        contains_call_to(normalized.program(), normalized_main, loop_id),
        "normalize must keep callsites for recursive callees"
    );
}

fn residualized(program: CoreProgram) -> Residualized {
    let sema = SemanticTables::with_counts(program.exprs().len(), program.stmts().len());
    Residualized::new(
        program,
        DiagnosticBag::default(),
        sema,
        MonomorphizationSummary::default(),
        CtPropagationTables::default(),
        BtaTables::default(),
        ResidualTables::default(),
    )
}

fn reachable_stmt_count(program: &CoreProgram) -> usize {
    let mut seen_stmts = HashSet::new();
    let mut stack = program
        .functions()
        .iter()
        .map(|function| function.body)
        .collect::<Vec<_>>();
    while let Some(stmt_id) = stack.pop() {
        if !seen_stmts.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        stack.extend(stmt.child_stmts());
    }
    seen_stmts.len()
}

fn contains_if_stmt(program: &CoreProgram, root: StmtId) -> bool {
    let mut seen_stmts = HashSet::new();
    let mut stack = vec![root];
    while let Some(stmt_id) = stack.pop() {
        if !seen_stmts.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if matches!(stmt.kind, StmtKind::If { .. }) {
            return true;
        }
        stack.extend(stmt.child_stmts());
    }
    false
}

fn contains_call_to(program: &CoreProgram, root: StmtId, target: FuncId) -> bool {
    let mut seen_stmts = HashSet::new();
    let mut stack = vec![root];
    while let Some(stmt_id) = stack.pop() {
        if !seen_stmts.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        if matches!(stmt.kind, StmtKind::Call { callee, .. } if callee == target) {
            return true;
        }
        stack.extend(stmt.child_stmts());
    }
    false
}
