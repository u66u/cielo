use cielo::common::diagnostics::DiagnosticBag;
use cielo::common::ids::{EffectLabelId, FuncId, SourceId, SymbolId, TypeId, VarId};
use cielo::common::span::Span;
use cielo::common::symbols::Interner;
use cielo::ir::core::{
    CoreProgram, CoreTypeRef, ExprKind, ExprNode, FunctionDecl, Literal, MatchArm,
    PrimitiveTypeRef, StmtKind, StmtNode,
};
use cielo::passes::{bta, c_emit, cfg_lower, ct_propagate, linearize, residualize};
use cielo::pipeline::compiler::TargetSpec;
use cielo::pipeline::phases::{
    BranchDecision, BtaClassified, BtaTables, CtPropagationTables, Knownness,
    MonomorphizationSummary, Reason, SemanticTables, Stage, Typed,
};
use cielo::sema::effect::SortedEffectRow;
use cielo::sema::ty::Persistability;
use cielo::{Compiler, CompilerConfig};

#[test]
fn compiles_source_through_default_pipeline() {
    let src = r#"
fn add(a: Int, b: Int) -> Int {
  a + b
}
fn main() -> Int {
  add(1, 2)
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source(src, SourceId::from_u32(0), &mut interner);
    assert_eq!(residual.program().entrypoints().len(), 1);
    assert!(
        !residual.program().functions().is_empty(),
        "default pipeline should emit at least the entrypoint function"
    );
    assert!(
        residual.diagnostics().entries().is_empty(),
        "default pipeline should compile the skeleton without diagnostics"
    );
}

#[test]
fn staging_boundary_rejects_non_concrete_effect_rows() {
    let mut program = CoreProgram::new();
    let span = Span::synthetic();

    let zero = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(0)),
    });
    let ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(zero),
    });
    let main = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(0),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::from_slice(&[EffectLabelId::from_u32(7)]),
        body: ret,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main]);

    let sema = SemanticTables::with_counts(program.exprs().len(), program.stmts().len());
    let typed = Typed::new(program, DiagnosticBag::default(), sema);
    let mut mono_summary = MonomorphizationSummary::default();
    mono_summary.source_to_mono.insert(main, vec![main]);
    let mono = typed.into_monomorphized(mono_summary);

    let panic = std::panic::catch_unwind(|| {
        let _ = ct_propagate::run(mono, TargetSpec::default());
    });
    assert!(
        panic.is_err(),
        "invalid effect rows must panic before staging"
    );
}

#[test]
fn compiles_handle_flow_and_keeps_root_stmt_effects_discharged() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  let x = handle { do Console.print("x"); 7 } with Console {
    | print(s) => 0
  };
  x
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source(src, SourceId::from_u32(0), &mut interner);
    assert_eq!(residual.program().handlers().len(), 1);
    let main_body = residual.program().functions()[0].body;
    assert_eq!(
        residual.sema().effects_of_stmt[main_body.index()],
        SortedEffectRow::empty()
    );
}

#[test]
fn compiles_adt_constructor_flow_without_diagnostics() {
    let src = r#"
enum Option { Some(Int), None }
struct Pair { a: Int, b: Int }
fn main() -> Int {
  let x = Some(1);
  let y = Pair(1, 2);
  0
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source(src, SourceId::from_u32(0), &mut interner);
    assert_eq!(residual.program().structs().len(), 1);
    assert_eq!(residual.program().enums().len(), 1);
    assert!(residual.diagnostics().entries().is_empty());
}

#[test]
fn residualize_erases_declared_effect_annotations_and_keeps_call_summaries() {
    let src = r#"
effect Console { fn print(s: String) -> () }
fn ping() -> Int with Console {
  do Console.print("x");
  7
}
fn main() -> Int {
  let y = ping();
  y
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source(src, SourceId::from_u32(0), &mut interner);

    for function in residual.program().functions() {
        assert!(
            function.declared_effects.is_empty(),
            "residual function effects should be erased"
        );
    }

    let ping_id = residual
        .program()
        .functions()
        .iter()
        .enumerate()
        .find_map(|(idx, function)| {
            (interner.resolve(function.name) == Some("ping")).then_some(idx)
        })
        .expect("ping function must exist");
    let ping_effects = residual
        .residual()
        .function_effect_summary
        .get(&cielo::common::ids::FuncId::new(ping_id))
        .expect("ping summary must exist");
    assert!(ping_effects.contains(EffectLabelId::from_u32(0)));

    let main_body = residual
        .program()
        .functions()
        .iter()
        .find(|f| interner.resolve(f.name) == Some("main"))
        .expect("main")
        .body;
    let mut stack = vec![main_body];
    let mut saw_call = false;
    while let Some(stmt_id) = stack.pop() {
        let stmt = residual.program().stmt(stmt_id).expect("reachable stmt");
        match &stmt.kind {
            StmtKind::Call { effects, .. } => {
                assert!(effects.contains(EffectLabelId::from_u32(0)));
                saw_call = true;
            }
            StmtKind::Let { next, .. }
            | StmtKind::Resume { next, .. }
            | StmtKind::Perform { next, .. } => stack.push(*next),
            StmtKind::Val { value, next, .. } => {
                stack.push(*value);
                stack.push(*next);
            }
            StmtKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                stack.push(*then_branch);
                stack.push(*else_branch);
            }
            StmtKind::Match { arms, default, .. } => {
                for arm in arms {
                    stack.push(arm.body);
                }
                if let Some(default_stmt) = default {
                    stack.push(*default_stmt);
                }
            }
            StmtKind::Handle { body, next, .. } | StmtKind::Stage { body, next, .. } => {
                stack.push(*body);
                if let Some(next_stmt) = next {
                    stack.push(*next_stmt);
                }
            }
            StmtKind::Return(_) | StmtKind::Hole { .. } | StmtKind::Error(_) => {}
        }
    }
    assert!(saw_call, "expected at least one call in main flow");
}

#[test]
fn residualize_replaces_cached_ct_expr_with_literal() {
    let mut program = CoreProgram::new();
    let span = Span::synthetic();

    let one = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(1)),
    });
    let two = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(2)),
    });
    let sum = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo::ir::core::BinaryOp::Add,
            lhs: one,
            rhs: two,
        },
    });
    let result_var = VarId::from_u32(0);
    let result_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(result_var),
    });
    let ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(result_expr),
    });
    let body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Let {
            binding: result_var,
            value: sum,
            next: ret,
        },
    });
    let main_name = SymbolId::from_u32(1);
    let main_id = program.add_function(FunctionDecl {
        name: main_name,
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let sema = SemanticTables::with_counts(program.exprs().len(), program.stmts().len());
    let mut ct = CtPropagationTables::default();
    ct.ct_cache.insert(sum, Literal::Int(3));
    let mut bta_tables = BtaTables::default();
    bta_tables.stage_of_expr.insert(sum, Stage::Ct);
    bta_tables
        .knownness_of_expr
        .insert(sum, Knownness::KnownPersistable);
    let classified = BtaClassified::new(
        program,
        DiagnosticBag::default(),
        sema,
        MonomorphizationSummary::default(),
        ct,
        bta_tables,
    );
    let residual = residualize::run(classified);

    assert!(matches!(
        residual.program().expr(sum).map(|expr| &expr.kind),
        Some(ExprKind::Literal(Literal::Int(3)))
    ));
}

#[test]
fn ct_propagate_collects_stable_eval_stats_oracle() {
    let mut program = CoreProgram::new();
    let span = Span::synthetic();

    let lit_one = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(1)),
    });
    let lit_two = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(2)),
    });
    let folded_add = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo::ir::core::BinaryOp::Add,
            lhs: lit_one,
            rhs: lit_two,
        },
    });
    let runtime_var = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(VarId::from_u32(0)),
    });
    let missing_input_add = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo::ir::core::BinaryOp::Add,
            lhs: runtime_var,
            rhs: lit_two,
        },
    });
    let unsupported_call = program.push_expr(ExprNode {
        span,
        kind: ExprKind::PureCall {
            callee: FuncId::from_u32(0),
            args: vec![lit_one],
        },
    });
    let folded_neg = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Unary {
            op: cielo::ir::core::UnaryOp::Neg,
            expr: lit_two,
        },
    });

    let ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(folded_add),
    });
    let main_name = SymbolId::from_u32(100);
    let main_id = program.add_function(FunctionDecl {
        name: main_name,
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: ret,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let sema = SemanticTables::with_counts(program.exprs().len(), program.stmts().len());
    let mono = cielo::pipeline::phases::Monomorphized::new(
        program,
        DiagnosticBag::default(),
        sema,
        MonomorphizationSummary::default(),
    );
    let ct = ct_propagate::run(mono, CompilerConfig::default().target);
    let stats = ct.ct().eval_stats;

    assert_eq!(
        ct.ct().ct_cache.get(&folded_add),
        Some(&Literal::Int(3)),
        "foldable binary expression should be materialized in ct cache"
    );
    assert_eq!(
        ct.ct().ct_cache.get(&folded_neg),
        Some(&Literal::Int(-2)),
        "foldable unary expression should be materialized in ct cache"
    );
    assert!(
        !ct.ct().ct_cache.contains_key(&missing_input_add),
        "binary expression with runtime var operand should remain unresolved"
    );
    assert!(
        !ct.ct().ct_cache.contains_key(&unsupported_call),
        "unsupported expression shapes should not enter ct cache"
    );

    assert_eq!(
        stats.iterations, 2,
        "program should converge in exactly two fixpoint iterations"
    );
    assert_eq!(
        stats.eval_attempts, 10,
        "eval attempts should include first-pass full walk and second-pass unresolved nodes"
    );
    assert_eq!(
        stats.cache_hits, 4,
        "second-pass cache hits should match folded nodes"
    );
    assert_eq!(
        stats.cache_inserts, 4,
        "ct cache inserts should equal literal+unary+binary folds"
    );
    assert_eq!(
        stats.folded_literals, 2,
        "literal folds should count both literals"
    );
    assert_eq!(
        stats.folded_unary, 1,
        "unary fold count should include negation"
    );
    assert_eq!(
        stats.folded_binary, 1,
        "binary fold count should include add"
    );
    assert_eq!(
        stats.miss_missing_inputs, 2,
        "missing-input misses should repeat until fixpoint convergence"
    );
    assert_eq!(
        stats.miss_unsupported, 4,
        "unsupported-shape misses should be tracked across fixpoint iterations"
    );
}

#[test]
fn ct_propagate_tracks_host_float_folds_explicitly() {
    let mut program = CoreProgram::new();
    let span = Span::synthetic();

    let float_lit = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Float(1.5)),
    });
    let neg_float = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Unary {
            op: cielo::ir::core::UnaryOp::Neg,
            expr: float_lit,
        },
    });
    let ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(neg_float),
    });
    let main_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(101),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Float),
        declared_effects: SortedEffectRow::empty(),
        body: ret,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let sema = SemanticTables::with_counts(program.exprs().len(), program.stmts().len());
    let mono = cielo::pipeline::phases::Monomorphized::new(
        program,
        DiagnosticBag::default(),
        sema,
        MonomorphizationSummary::default(),
    );
    let ct = ct_propagate::run(mono, CompilerConfig::default().target);
    let stats = ct.ct().eval_stats;

    assert_eq!(
        ct.ct().ct_cache.get(&neg_float),
        Some(&Literal::Float(-1.5)),
        "unary neg on float literals should still fold"
    );
    assert_eq!(
        stats.folded_unary, 1,
        "float unary fold should contribute to unary fold totals"
    );
    assert_eq!(
        stats.folded_float_host, 1,
        "host-semantics float folds should be counted explicitly"
    );
}

#[test]
fn ct_propagate_folds_float_binary_surface_and_counts_host_float_folds() {
    let mut program = CoreProgram::new();
    let span = Span::synthetic();

    let lhs = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Float(1.25)),
    });
    let rhs = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Float(0.5)),
    });
    let add = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo::ir::core::BinaryOp::Add,
            lhs,
            rhs,
        },
    });
    let sub = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo::ir::core::BinaryOp::Sub,
            lhs,
            rhs,
        },
    });
    let mul = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo::ir::core::BinaryOp::Mul,
            lhs,
            rhs,
        },
    });
    let div = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo::ir::core::BinaryOp::Div,
            lhs,
            rhs,
        },
    });
    let rem = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo::ir::core::BinaryOp::Mod,
            lhs,
            rhs,
        },
    });
    let lt = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo::ir::core::BinaryOp::Lt,
            lhs,
            rhs,
        },
    });
    let ge = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo::ir::core::BinaryOp::Ge,
            lhs,
            rhs,
        },
    });
    let eq = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo::ir::core::BinaryOp::Eq,
            lhs,
            rhs,
        },
    });
    let ne = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo::ir::core::BinaryOp::Ne,
            lhs,
            rhs,
        },
    });
    let ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(ne),
    });
    let main_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(102),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Bool),
        declared_effects: SortedEffectRow::empty(),
        body: ret,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let sema = SemanticTables::with_counts(program.exprs().len(), program.stmts().len());
    let mono = cielo::pipeline::phases::Monomorphized::new(
        program,
        DiagnosticBag::default(),
        sema,
        MonomorphizationSummary::default(),
    );
    let ct = ct_propagate::run(mono, CompilerConfig::default().target);
    let stats = ct.ct().eval_stats;

    assert_eq!(ct.ct().ct_cache.get(&add), Some(&Literal::Float(1.75)));
    assert_eq!(ct.ct().ct_cache.get(&sub), Some(&Literal::Float(0.75)));
    assert_eq!(ct.ct().ct_cache.get(&mul), Some(&Literal::Float(0.625)));
    assert_eq!(ct.ct().ct_cache.get(&div), Some(&Literal::Float(2.5)));
    assert_eq!(ct.ct().ct_cache.get(&rem), Some(&Literal::Float(0.25)));
    assert_eq!(ct.ct().ct_cache.get(&lt), Some(&Literal::Bool(false)));
    assert_eq!(ct.ct().ct_cache.get(&ge), Some(&Literal::Bool(true)));
    assert_eq!(ct.ct().ct_cache.get(&eq), Some(&Literal::Bool(false)));
    assert_eq!(ct.ct().ct_cache.get(&ne), Some(&Literal::Bool(true)));

    assert_eq!(stats.folded_binary, 9, "all float binary forms should fold");
    assert_eq!(
        stats.folded_float_host, 9,
        "all folded float binary forms should be tracked as host-float folds"
    );
}

#[test]
fn ct_propagate_folds_non_numeric_equality_and_leaves_mixed_types_unresolved() {
    let mut program = CoreProgram::new();
    let span = Span::synthetic();

    let bool_true = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Bool(true)),
    });
    let bool_false = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Bool(false)),
    });
    let char_a = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Char('a')),
    });
    let char_b = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Char('b')),
    });
    let string_x = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::String("x".to_owned())),
    });
    let string_y = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::String("y".to_owned())),
    });
    let unit_lhs = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Unit),
    });
    let unit_rhs = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Unit),
    });
    let int_one = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(1)),
    });
    let bool_eq = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo::ir::core::BinaryOp::Eq,
            lhs: bool_true,
            rhs: bool_false,
        },
    });
    let char_ge = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo::ir::core::BinaryOp::Ge,
            lhs: char_b,
            rhs: char_a,
        },
    });
    let string_ne = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo::ir::core::BinaryOp::Ne,
            lhs: string_x,
            rhs: string_y,
        },
    });
    let unit_eq = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo::ir::core::BinaryOp::Eq,
            lhs: unit_lhs,
            rhs: unit_rhs,
        },
    });
    let mixed_eq = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo::ir::core::BinaryOp::Eq,
            lhs: int_one,
            rhs: bool_true,
        },
    });
    let ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(unit_eq),
    });
    let main_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(103),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Bool),
        declared_effects: SortedEffectRow::empty(),
        body: ret,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let sema = SemanticTables::with_counts(program.exprs().len(), program.stmts().len());
    let mono = cielo::pipeline::phases::Monomorphized::new(
        program,
        DiagnosticBag::default(),
        sema,
        MonomorphizationSummary::default(),
    );
    let ct = ct_propagate::run(mono, CompilerConfig::default().target);
    let stats = ct.ct().eval_stats;

    assert_eq!(ct.ct().ct_cache.get(&bool_eq), Some(&Literal::Bool(false)));
    assert_eq!(ct.ct().ct_cache.get(&char_ge), Some(&Literal::Bool(true)));
    assert_eq!(ct.ct().ct_cache.get(&string_ne), Some(&Literal::Bool(true)));
    assert_eq!(ct.ct().ct_cache.get(&unit_eq), Some(&Literal::Bool(true)));
    assert!(
        !ct.ct().ct_cache.contains_key(&mixed_eq),
        "mixed-type equality should remain unresolved in ct cache"
    );
    assert_eq!(
        stats.folded_float_host, 0,
        "non-float folds should not increment host-float counters"
    );
    assert!(
        stats.miss_unsupported >= 1,
        "mixed-type equality should produce unsupported misses"
    );
}

#[test]
fn ct_propagate_skips_non_finite_host_float_folds() {
    let mut program = CoreProgram::new();
    let span = Span::synthetic();

    let finite_lhs = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Float(1.0)),
    });
    let finite_rhs = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Float(2.0)),
    });
    let finite_add = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo::ir::core::BinaryOp::Add,
            lhs: finite_lhs,
            rhs: finite_rhs,
        },
    });

    let huge = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Float(f64::MAX)),
    });
    let overflow_mul = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo::ir::core::BinaryOp::Mul,
            lhs: huge,
            rhs: huge,
        },
    });
    let inf = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Float(f64::INFINITY)),
    });
    let neg_inf = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Unary {
            op: cielo::ir::core::UnaryOp::Neg,
            expr: inf,
        },
    });
    let nan = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Float(f64::NAN)),
    });
    let nan_eq = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo::ir::core::BinaryOp::Eq,
            lhs: nan,
            rhs: nan,
        },
    });
    let ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(finite_add),
    });
    let main_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(104),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Float),
        declared_effects: SortedEffectRow::empty(),
        body: ret,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let sema = SemanticTables::with_counts(program.exprs().len(), program.stmts().len());
    let mono = cielo::pipeline::phases::Monomorphized::new(
        program,
        DiagnosticBag::default(),
        sema,
        MonomorphizationSummary::default(),
    );
    let ct = ct_propagate::run(mono, CompilerConfig::default().target);
    let stats = ct.ct().eval_stats;

    assert_eq!(
        ct.ct().ct_cache.get(&finite_add),
        Some(&Literal::Float(3.0))
    );
    assert!(
        !ct.ct().ct_cache.contains_key(&overflow_mul),
        "float arithmetic that overflows to non-finite must remain unresolved"
    );
    assert!(
        !ct.ct().ct_cache.contains_key(&neg_inf),
        "unary negation on non-finite inputs must remain unresolved"
    );
    assert!(
        !ct.ct().ct_cache.contains_key(&nan_eq),
        "equality on non-finite float inputs must remain unresolved"
    );
    assert_eq!(
        stats.folded_float_host, 1,
        "only finite host-float folds should be tracked"
    );
    assert!(
        stats.miss_unsupported >= 3,
        "non-finite host-float operations should count as unsupported misses"
    );
}

#[test]
fn residualize_skips_runtime_forced_cached_expr_even_when_literal_is_available() {
    let mut program = CoreProgram::new();
    let span = Span::synthetic();

    let one = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(1)),
    });
    let two = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(2)),
    });
    let sum = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo::ir::core::BinaryOp::Add,
            lhs: one,
            rhs: two,
        },
    });
    let result_var = VarId::from_u32(0);
    let result_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(result_var),
    });
    let ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(result_expr),
    });
    let body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Let {
            binding: result_var,
            value: sum,
            next: ret,
        },
    });
    let main_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(1),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let sema = SemanticTables::with_counts(program.exprs().len(), program.stmts().len());
    let mut ct = CtPropagationTables::default();
    ct.ct_cache.insert(sum, Literal::Int(3));
    let mut bta_tables = BtaTables::default();
    bta_tables
        .stage_of_expr
        .insert(sum, Stage::Rt(Reason::UserForcedRuntime));
    bta_tables
        .knownness_of_expr
        .insert(sum, Knownness::KnownPersistable);
    let classified = BtaClassified::new(
        program,
        DiagnosticBag::default(),
        sema,
        MonomorphizationSummary::default(),
        ct,
        bta_tables,
    );
    let residual = residualize::run(classified);

    assert!(matches!(
        residual.program().expr(sum).map(|expr| &expr.kind),
        Some(ExprKind::Binary { .. })
    ));
}

#[test]
fn residualize_recomputes_function_effect_summary_from_rewritten_ir() {
    let mut program = CoreProgram::new();
    let span = Span::synthetic();
    let effect = EffectLabelId::from_u32(0);

    let cond = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Bool(false)),
    });
    let zero = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(0)),
    });
    let one = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(1)),
    });
    let call_result_var = VarId::from_u32(10);
    let call_result_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(call_result_var),
    });

    let callee_then_ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(zero),
    });
    let callee_then = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Perform {
            result: None,
            effect,
            operation: SymbolId::from_u32(701),
            args: Vec::new(),
            next: callee_then_ret,
        },
    });
    let callee_else = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(one),
    });
    let callee_body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::If {
            cond,
            then_branch: callee_then,
            else_branch: callee_else,
        },
    });
    let callee_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(700),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::singleton(effect),
        body: callee_body,
        ct_only: false,
        span,
    });

    let main_ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(call_result_expr),
    });
    let main_body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Call {
            result: call_result_var,
            callee: callee_id,
            args: Vec::new(),
            effects: SortedEffectRow::singleton(effect),
            next: main_ret,
        },
    });
    let main_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(702),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: main_body,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let mut sema = SemanticTables::with_counts(program.exprs().len(), program.stmts().len());
    sema.effects_of_stmt[callee_body.index()] = SortedEffectRow::singleton(effect);
    let classified = BtaClassified::new(
        program,
        DiagnosticBag::default(),
        sema,
        MonomorphizationSummary::default(),
        CtPropagationTables::default(),
        BtaTables::default(),
    );
    let residual = residualize::run(classified);

    assert_eq!(
        residual.residual().function_effect_summary.get(&callee_id),
        Some(&SortedEffectRow::empty()),
        "callee summary should follow pruned residual body, not stale pre-residual stmt effects"
    );
    let call_effects = match residual.program().stmt(main_body).map(|stmt| &stmt.kind) {
        Some(StmtKind::Call { effects, .. }) => effects,
        other => panic!("expected main body to remain a call, got {other:?}"),
    };
    assert!(
        call_effects.is_empty(),
        "call effect row should be rewritten from recomputed residual function summary"
    );
}

#[test]
fn bta_marks_ct_expr_runtime_when_type_is_non_persistable() {
    let mut program = CoreProgram::new();
    let span = Span::synthetic();

    let one = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(3)),
    });
    let two = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(4)),
    });
    let fake_ct_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo::ir::core::BinaryOp::Add,
            lhs: one,
            rhs: two,
        },
    });
    let ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(fake_ct_expr),
    });
    let main_name = SymbolId::from_u32(1);
    let main_id = program.add_function(FunctionDecl {
        name: main_name,
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: ret,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let mut sema = SemanticTables::with_counts(program.exprs().len(), program.stmts().len());
    sema.type_of_expr[fake_ct_expr.index()] = Some(TypeId::new(0));
    sema.persistability_of_type = vec![Persistability::NonPersistable];

    let mut ct = CtPropagationTables::default();
    let _ = ct.ct_cache.insert(fake_ct_expr, Literal::Int(7));
    let ct_state = cielo::pipeline::phases::CtPropagated::new(
        program,
        DiagnosticBag::default(),
        sema,
        MonomorphizationSummary::default(),
        ct,
    );
    let classified = bta::run(ct_state);

    assert!(matches!(
        classified.bta().stage_of_expr.get(&fake_ct_expr),
        Some(Stage::Rt(cielo::pipeline::phases::Reason::NotPersistable(ty)))
            if *ty == TypeId::new(0)
    ));
    assert!(matches!(
        classified.bta().knownness_of_expr.get(&fake_ct_expr),
        Some(Knownness::KnownLocal) | Some(Knownness::KnownPersistable)
    ));
    assert!(
        classified
            .diagnostics()
            .entries()
            .iter()
            .any(|diag| diag.code == "BTA_NOT_PERSISTABLE_BOUNDARY"),
        "non-persistable CT value crossing must emit a dedicated staging diagnostic"
    );

    let residual = residualize::run(classified);
    assert!(matches!(
        residual.program().expr(fake_ct_expr).map(|expr| &expr.kind),
        Some(ExprKind::Binary { .. })
    ));
}

#[test]
fn bta_not_persistable_diagnostic_points_to_boundary_stmt_span() {
    let mut program = CoreProgram::new();
    let source = SourceId::from_u32(17);
    let expr_span = Span::new(source, 5, 11);
    let stmt_span = Span::new(source, 40, 52);
    let fn_span = Span::new(source, 60, 80);

    let one = program.push_expr(ExprNode {
        span: expr_span,
        kind: ExprKind::Literal(Literal::Int(3)),
    });
    let two = program.push_expr(ExprNode {
        span: expr_span,
        kind: ExprKind::Literal(Literal::Int(4)),
    });
    let fake_ct_expr = program.push_expr(ExprNode {
        span: expr_span,
        kind: ExprKind::Binary {
            op: cielo::ir::core::BinaryOp::Add,
            lhs: one,
            rhs: two,
        },
    });
    let ret = program.push_stmt(StmtNode {
        span: stmt_span,
        kind: StmtKind::Return(fake_ct_expr),
    });
    let main_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(1),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: ret,
        ct_only: false,
        span: fn_span,
    });
    program.set_entrypoints([main_id]);

    let mut sema = SemanticTables::with_counts(program.exprs().len(), program.stmts().len());
    sema.type_of_expr[fake_ct_expr.index()] = Some(TypeId::new(0));
    sema.persistability_of_type = vec![Persistability::NonPersistable];

    let mut ct = CtPropagationTables::default();
    let _ = ct.ct_cache.insert(fake_ct_expr, Literal::Int(7));
    let ct_state = cielo::pipeline::phases::CtPropagated::new(
        program,
        DiagnosticBag::default(),
        sema,
        MonomorphizationSummary::default(),
        ct,
    );
    let classified = bta::run(ct_state);
    let diag = classified
        .diagnostics()
        .entries()
        .iter()
        .find(|diag| diag.code == "BTA_NOT_PERSISTABLE_BOUNDARY")
        .expect("boundary diagnostic");

    assert_eq!(
        diag.span, stmt_span,
        "persistability boundary diagnostics should point at the runtime-use statement span"
    );
    assert!(
        diag.message.contains("statement s"),
        "diagnostic message should identify the boundary statement id"
    );
    assert!(
        diag.message.contains("return-value"),
        "diagnostic message should include the boundary crossing role"
    );
}

#[test]
fn bta_not_persistable_diagnostic_reports_call_arg_boundary_role() {
    let mut program = CoreProgram::new();
    let source = SourceId::from_u32(18);
    let span = Span::new(source, 10, 20);

    let one = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(1)),
    });
    let two = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(2)),
    });
    let non_persistable_ct_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo::ir::core::BinaryOp::Add,
            lhs: one,
            rhs: two,
        },
    });

    let helper_ret_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(0)),
    });
    let helper_ret_stmt = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(helper_ret_expr),
    });
    let helper_fn = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(2),
        params: vec![VarId::from_u32(0)],
        param_types: vec![CoreTypeRef::Primitive(PrimitiveTypeRef::Int)],
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: helper_ret_stmt,
        ct_only: false,
        span,
    });

    let ret_var = VarId::from_u32(1);
    let ret_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(ret_var),
    });
    let ret_stmt = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(ret_expr),
    });
    let call_stmt = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Call {
            result: ret_var,
            callee: helper_fn,
            args: vec![non_persistable_ct_expr],
            effects: SortedEffectRow::empty(),
            next: ret_stmt,
        },
    });

    let main_fn = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(1),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: call_stmt,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_fn]);

    let mut sema = SemanticTables::with_counts(program.exprs().len(), program.stmts().len());
    sema.type_of_expr[non_persistable_ct_expr.index()] = Some(TypeId::new(0));
    sema.persistability_of_type = vec![Persistability::NonPersistable];

    let mut ct = CtPropagationTables::default();
    let _ = ct.ct_cache.insert(non_persistable_ct_expr, Literal::Int(3));
    let classified = bta::run(cielo::pipeline::phases::CtPropagated::new(
        program,
        DiagnosticBag::default(),
        sema,
        MonomorphizationSummary::default(),
        ct,
    ));
    let diag = classified
        .diagnostics()
        .entries()
        .iter()
        .find(|diag| diag.code == "BTA_NOT_PERSISTABLE_BOUNDARY")
        .expect("boundary diagnostic");

    assert!(
        diag.message.contains("call-arg#0"),
        "diagnostic should report exact call-argument boundary role"
    );
}

#[test]
fn bta_does_not_flag_non_persistable_ct_expr_used_only_inside_comptime_stage() {
    let mut program = CoreProgram::new();
    let source = SourceId::from_u32(19);
    let span = Span::new(source, 10, 20);

    let one = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(1)),
    });
    let two = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(2)),
    });
    let non_persistable_ct_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Binary {
            op: cielo::ir::core::BinaryOp::Add,
            lhs: one,
            rhs: two,
        },
    });

    let stage_var = VarId::from_u32(55);
    let stage_var_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(stage_var),
    });
    let stage_ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(stage_var_expr),
    });
    let stage_let = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Let {
            binding: stage_var,
            value: non_persistable_ct_expr,
            next: stage_ret,
        },
    });

    let zero = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(0)),
    });
    let outer_ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(zero),
    });
    let root = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Stage {
            stage: cielo::ir::core::StageDirective::Comptime,
            body: stage_let,
            next: Some(outer_ret),
        },
    });
    let main_fn = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(1),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: root,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_fn]);

    let mut sema = SemanticTables::with_counts(program.exprs().len(), program.stmts().len());
    sema.type_of_expr[non_persistable_ct_expr.index()] = Some(TypeId::new(0));
    sema.persistability_of_type = vec![Persistability::NonPersistable];

    let mut ct = CtPropagationTables::default();
    let _ = ct.ct_cache.insert(non_persistable_ct_expr, Literal::Int(3));
    let classified = bta::run(cielo::pipeline::phases::CtPropagated::new(
        program,
        DiagnosticBag::default(),
        sema,
        MonomorphizationSummary::default(),
        ct,
    ));

    assert!(
        matches!(
            classified.bta().stage_of_expr.get(&non_persistable_ct_expr),
            Some(Stage::Ct)
        ),
        "non-persistable ct expression confined to @comptime stage should stay CT"
    );
    assert!(
        classified
            .diagnostics()
            .entries()
            .iter()
            .all(|diag| diag.code != "BTA_NOT_PERSISTABLE_BOUNDARY"),
        "no boundary diagnostic should be emitted when the expression never crosses into runtime"
    );
}

#[test]
fn residualize_prunes_if_using_ct_branch_decision() {
    let mut program = CoreProgram::new();
    let span = Span::synthetic();

    let cond = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Bool(true)),
    });
    let then_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(1)),
    });
    let else_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(2)),
    });
    let then_stmt = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(then_expr),
    });
    let else_stmt = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(else_expr),
    });
    let root_if = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::If {
            cond,
            then_branch: then_stmt,
            else_branch: else_stmt,
        },
    });

    let main_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(2),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: root_if,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let sema = SemanticTables::with_counts(program.exprs().len(), program.stmts().len());
    let mut ct = CtPropagationTables::default();
    ct.ct_cache.insert(cond, Literal::Bool(true));
    ct.branch_decisions.insert(cond, BranchDecision::LiveTrue);
    let classified = BtaClassified::new(
        program,
        DiagnosticBag::default(),
        sema,
        MonomorphizationSummary::default(),
        ct,
        BtaTables::default(),
    );
    let residual = residualize::run(classified);

    let body = residual
        .program()
        .function(FuncId::new(0))
        .expect("main")
        .body;
    assert_eq!(body, then_stmt, "if root should be rewired to live branch");
}

#[test]
fn residualize_prunes_match_with_known_variant_scrutinee() {
    let mut program = CoreProgram::new();
    let span = Span::synthetic();
    let enum_name = SymbolId::from_u32(11);
    let some_variant = SymbolId::from_u32(12);
    let none_variant = SymbolId::from_u32(13);

    let scrutinee = program.push_expr(ExprNode {
        span,
        kind: ExprKind::MakeEnum {
            ty: enum_name,
            variant: some_variant,
            fields: Vec::new(),
        },
    });
    let one = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(1)),
    });
    let zero = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(0)),
    });
    let some_body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(one),
    });
    let none_body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(zero),
    });
    let root_match = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Match {
            scrutinee,
            arms: vec![
                MatchArm {
                    tag: some_variant,
                    binders: Vec::new(),
                    body: some_body,
                    span,
                },
                MatchArm {
                    tag: none_variant,
                    binders: Vec::new(),
                    body: none_body,
                    span,
                },
            ],
            default: None,
        },
    });

    let main_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(14),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: root_match,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let sema = SemanticTables::with_counts(program.exprs().len(), program.stmts().len());
    let classified = BtaClassified::new(
        program,
        DiagnosticBag::default(),
        sema,
        MonomorphizationSummary::default(),
        CtPropagationTables::default(),
        BtaTables::default(),
    );
    let residual = residualize::run(classified);

    let body = residual
        .program()
        .function(FuncId::new(0))
        .expect("main")
        .body;
    assert_eq!(
        body, some_body,
        "match root should be rewired to arm selected by known variant"
    );
}

#[test]
fn residualize_prunes_match_with_binders_by_materializing_let_bindings() {
    let mut program = CoreProgram::new();
    let span = Span::synthetic();
    let enum_name = SymbolId::from_u32(21);
    let some_variant = SymbolId::from_u32(22);

    let field_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(41)),
    });
    let scrutinee = program.push_expr(ExprNode {
        span,
        kind: ExprKind::MakeEnum {
            ty: enum_name,
            variant: some_variant,
            fields: vec![field_expr],
        },
    });
    let binder = VarId::from_u32(77);
    let binder_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(binder),
    });
    let arm_body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(binder_expr),
    });
    let root_match = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Match {
            scrutinee,
            arms: vec![MatchArm {
                tag: some_variant,
                binders: vec![binder],
                body: arm_body,
                span,
            }],
            default: None,
        },
    });

    let main_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(23),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: root_match,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let sema = SemanticTables::with_counts(program.exprs().len(), program.stmts().len());
    let classified = BtaClassified::new(
        program,
        DiagnosticBag::default(),
        sema,
        MonomorphizationSummary::default(),
        CtPropagationTables::default(),
        BtaTables::default(),
    );
    let residual = residualize::run(classified);

    let body = residual
        .program()
        .function(FuncId::new(0))
        .expect("main")
        .body;
    let stmt = residual.program().stmt(body).expect("rewritten root stmt");
    match &stmt.kind {
        StmtKind::Let {
            binding,
            value,
            next,
        } => {
            assert_eq!(*binding, binder);
            assert_eq!(*value, field_expr);
            assert_eq!(*next, arm_body);
        }
        other => panic!("expected let-materialized match pruning, got {other:?}"),
    }
}

#[test]
fn residualize_preserves_function_effect_summary_when_match_pruning_rewrites_root_stmt() {
    let mut program = CoreProgram::new();
    let span = Span::synthetic();
    let effect = EffectLabelId::from_u32(0);
    let enum_name = SymbolId::from_u32(700);
    let some_variant = SymbolId::from_u32(701);
    let op_name = SymbolId::from_u32(702);

    let payload = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(1)),
    });
    let scrutinee = program.push_expr(ExprNode {
        span,
        kind: ExprKind::MakeEnum {
            ty: enum_name,
            variant: some_variant,
            fields: vec![payload],
        },
    });
    let binder = VarId::from_u32(703);
    let one = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(1)),
    });
    let perform_next = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(one),
    });
    let effectful_arm = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Perform {
            result: None,
            effect,
            operation: op_name,
            args: Vec::new(),
            next: perform_next,
        },
    });
    let callee_match_root = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Match {
            scrutinee,
            arms: vec![MatchArm {
                tag: some_variant,
                binders: vec![binder],
                body: effectful_arm,
                span,
            }],
            default: None,
        },
    });
    let callee_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(704),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::singleton(effect),
        body: callee_match_root,
        ct_only: false,
        span,
    });

    let call_result = VarId::from_u32(705);
    let call_result_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(call_result),
    });
    let main_ret = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(call_result_expr),
    });
    let main_call = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Call {
            result: call_result,
            callee: callee_id,
            args: Vec::new(),
            effects: SortedEffectRow::empty(),
            next: main_ret,
        },
    });
    let main_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(706),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: main_call,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let mut sema = SemanticTables::with_counts(program.exprs().len(), program.stmts().len());
    sema.effects_of_stmt[callee_match_root.index()] = SortedEffectRow::singleton(effect);
    let classified = BtaClassified::new(
        program,
        DiagnosticBag::default(),
        sema,
        MonomorphizationSummary::default(),
        CtPropagationTables::default(),
        BtaTables::default(),
    );
    let residual = residualize::run(classified);

    let rewritten_callee_root = residual
        .program()
        .function(callee_id)
        .expect("callee function")
        .body;
    assert_ne!(
        rewritten_callee_root, callee_match_root,
        "match pruning with binders should allocate a new callee root stmt"
    );
    assert!(
        residual
            .residual()
            .function_effect_summary
            .get(&callee_id)
            .is_some_and(|row| row.contains(effect)),
        "function effect summary should retain pre-residualized root effects"
    );

    let main = residual.program().function(main_id).expect("main function");
    let call_stmt = residual
        .program()
        .stmt(main.body)
        .expect("main body call after residualization");
    let StmtKind::Call { effects, .. } = &call_stmt.kind else {
        panic!("expected main body to remain a call");
    };
    assert!(
        effects.contains(effect),
        "callsite effect rows should be rewritten from preserved function summaries"
    );
}

#[test]
fn residualize_prunes_if_through_let_alias_chain() {
    let mut program = CoreProgram::new();
    let span = Span::synthetic();
    let cond_var0 = VarId::from_u32(200);
    let cond_var1 = VarId::from_u32(201);

    let cond_lit = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Bool(true)),
    });
    let cond_var0_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(cond_var0),
    });
    let cond_var1_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(cond_var1),
    });
    let then_lit = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(1)),
    });
    let else_lit = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(0)),
    });

    let then_stmt = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(then_lit),
    });
    let else_stmt = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(else_lit),
    });
    let if_stmt = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::If {
            cond: cond_var1_expr,
            then_branch: then_stmt,
            else_branch: else_stmt,
        },
    });
    let alias1_stmt = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Let {
            binding: cond_var1,
            value: cond_var0_expr,
            next: if_stmt,
        },
    });
    let root = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Let {
            binding: cond_var0,
            value: cond_lit,
            next: alias1_stmt,
        },
    });

    let main_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(24),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: root,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let sema = SemanticTables::with_counts(program.exprs().len(), program.stmts().len());
    let classified = BtaClassified::new(
        program,
        DiagnosticBag::default(),
        sema,
        MonomorphizationSummary::default(),
        CtPropagationTables::default(),
        BtaTables::default(),
    );
    let residual = residualize::run(classified);

    let root_stmt = residual.program().stmt(root).expect("root let");
    let next = match root_stmt.kind {
        StmtKind::Let { next, .. } => next,
        ref other => panic!("expected root let after residualization, got {other:?}"),
    };
    let alias_stmt = residual.program().stmt(next).expect("alias let");
    match alias_stmt.kind {
        StmtKind::Let { next, .. } => assert_eq!(
            next, then_stmt,
            "if should be pruned to then branch through alias chain"
        ),
        ref other => panic!("expected alias let after residualization, got {other:?}"),
    }
}

#[test]
fn residualize_prunes_match_through_let_alias_chain_with_binder_materialization() {
    let mut program = CoreProgram::new();
    let span = Span::synthetic();
    let enum_name = SymbolId::from_u32(30);
    let some_variant = SymbolId::from_u32(31);
    let scrut_var0 = VarId::from_u32(210);
    let scrut_var1 = VarId::from_u32(211);
    let binder = VarId::from_u32(212);

    let field_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(41)),
    });
    let make_enum = program.push_expr(ExprNode {
        span,
        kind: ExprKind::MakeEnum {
            ty: enum_name,
            variant: some_variant,
            fields: vec![field_expr],
        },
    });
    let scrut_var0_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(scrut_var0),
    });
    let scrut_var1_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(scrut_var1),
    });
    let binder_expr = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Var(binder),
    });
    let fallback = program.push_expr(ExprNode {
        span,
        kind: ExprKind::Literal(Literal::Int(0)),
    });

    let arm_body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(binder_expr),
    });
    let default_body = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Return(fallback),
    });
    let root_match = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Match {
            scrutinee: scrut_var1_expr,
            arms: vec![MatchArm {
                tag: some_variant,
                binders: vec![binder],
                body: arm_body,
                span,
            }],
            default: Some(default_body),
        },
    });
    let alias1_stmt = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Let {
            binding: scrut_var1,
            value: scrut_var0_expr,
            next: root_match,
        },
    });
    let root = program.push_stmt(StmtNode {
        span,
        kind: StmtKind::Let {
            binding: scrut_var0,
            value: make_enum,
            next: alias1_stmt,
        },
    });

    let main_id = program.add_function(FunctionDecl {
        name: SymbolId::from_u32(32),
        params: Vec::new(),
        param_types: Vec::new(),
        return_type: CoreTypeRef::Primitive(PrimitiveTypeRef::Int),
        declared_effects: SortedEffectRow::empty(),
        body: root,
        ct_only: false,
        span,
    });
    program.set_entrypoints([main_id]);

    let sema = SemanticTables::with_counts(program.exprs().len(), program.stmts().len());
    let classified = BtaClassified::new(
        program,
        DiagnosticBag::default(),
        sema,
        MonomorphizationSummary::default(),
        CtPropagationTables::default(),
        BtaTables::default(),
    );
    let residual = residualize::run(classified);

    let root_stmt = residual.program().stmt(root).expect("root let");
    let next = match root_stmt.kind {
        StmtKind::Let { next, .. } => next,
        ref other => panic!("expected root let after residualization, got {other:?}"),
    };
    let alias_stmt = residual.program().stmt(next).expect("alias let");
    let pruned = match alias_stmt.kind {
        StmtKind::Let { next, .. } => next,
        ref other => panic!("expected alias let after residualization, got {other:?}"),
    };
    let pruned_stmt = residual.program().stmt(pruned).expect("match pruned stmt");
    match &pruned_stmt.kind {
        StmtKind::Let {
            binding,
            value,
            next,
        } => {
            assert_eq!(*binding, binder);
            assert_eq!(*value, field_expr);
            assert_eq!(*next, arm_body);
        }
        other => panic!("expected binder materialization after alias match prune, got {other:?}"),
    }
}

#[test]
fn v1_example_contract_oracle_matches_effect_and_staging_intent_split_pipeline() {
    let src = include_str!("../examples/v1_test.cielo");
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());

    let core = compiler.parse_and_lower_to_core(src, SourceId::from_u32(0), &mut interner);
    assert_eq!(core.program().effects().len(), 2);
    assert_eq!(
        interner.resolve(core.program().effects()[0].name),
        Some("Console")
    );
    assert_eq!(
        interner.resolve(core.program().effects()[1].name),
        Some("LocalState")
    );
    assert_eq!(core.program().effects()[0].operations.len(), 1);
    assert_eq!(core.program().effects()[1].operations.len(), 1);
    assert_eq!(
        interner.resolve(core.program().effects()[0].operations[0].name),
        Some("print")
    );
    assert_eq!(
        interner.resolve(core.program().effects()[1].operations[0].name),
        Some("tick")
    );
    assert_eq!(
        core.program().effects()[0].operations[0].param_types.len(),
        1
    );
    assert_eq!(
        core.program().effects()[1].operations[0].param_types.len(),
        0
    );
    let console_effect = core
        .program()
        .effects()
        .iter()
        .find_map(|effect| {
            (interner.resolve(effect.name) == Some("Console")).then_some(effect.label)
        })
        .expect("Console label");
    let local_state_effect = core
        .program()
        .effects()
        .iter()
        .find_map(|effect| {
            (interner.resolve(effect.name) == Some("LocalState")).then_some(effect.label)
        })
        .expect("LocalState label");

    let expected_names = ["seed", "bump", "local_step", "io_step", "main"];
    let observed_names = core
        .program()
        .functions()
        .iter()
        .map(|function| interner.resolve(function.name).unwrap_or("<missing>"))
        .collect::<Vec<_>>();
    assert_eq!(observed_names, expected_names);
    assert!(core.program().functions()[0].declared_effects.is_empty());
    assert!(core.program().functions()[1].declared_effects.is_empty());
    assert_eq!(
        core.program().functions()[2].declared_effects,
        SortedEffectRow::singleton(EffectLabelId::from_u32(1))
    );
    assert_eq!(
        core.program().functions()[3].declared_effects,
        SortedEffectRow::singleton(EffectLabelId::from_u32(0))
    );
    assert!(core.program().functions()[4].declared_effects.is_empty());

    let residual = compiler.run_v0_core_pipeline(core);
    assert_eq!(
        residual.diagnostics().entries().len(),
        0,
        "v1 example should compile without diagnostics"
    );

    let func_id_named = |name: &str| {
        residual
            .program()
            .functions()
            .iter()
            .enumerate()
            .find_map(|(idx, function)| {
                (interner.resolve(function.name) == Some(name)).then_some(FuncId::new(idx))
            })
            .unwrap_or_else(|| panic!("missing function `{name}`"))
    };
    let seed_id = func_id_named("seed");
    let bump_id = func_id_named("bump");
    let local_step_id = func_id_named("local_step");
    let io_step_id = func_id_named("io_step");
    let main_id = func_id_named("main");

    let summary = &residual.residual().function_effect_summary;
    assert!(
        summary.get(&seed_id).is_some_and(SortedEffectRow::is_empty),
        "seed is pure and should keep an empty residual effect summary"
    );
    assert!(
        summary.get(&bump_id).is_some_and(SortedEffectRow::is_empty),
        "bump is pure and should keep an empty residual effect summary"
    );
    assert!(
        summary
            .get(&local_step_id)
            .is_some_and(|row| row.contains(local_state_effect)),
        "local_step should retain LocalState in residual effect summary"
    );
    assert!(
        summary
            .get(&io_step_id)
            .is_some_and(|row| row.contains(console_effect)),
        "io_step should retain Console in residual effect summary"
    );
    assert!(
        summary.get(&main_id).is_some_and(SortedEffectRow::is_empty),
        "main handlers discharge LocalState/Console so main residual summary should be empty"
    );

    let main_body = residual
        .program()
        .function(main_id)
        .expect("main function")
        .body;
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![main_body];
    let mut saw_local_step_call = false;
    let mut saw_io_step_call = false;
    let mut saw_bump_call = false;
    while let Some(stmt_id) = stack.pop() {
        if !seen.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = residual.program().stmt(stmt_id) else {
            continue;
        };
        if let StmtKind::Call {
            callee, effects, ..
        } = &stmt.kind
        {
            if *callee == local_step_id {
                saw_local_step_call = true;
                assert!(
                    effects.contains(local_state_effect),
                    "main->local_step call should carry LocalState effect row"
                );
            }
            if *callee == io_step_id {
                saw_io_step_call = true;
                assert!(
                    effects.contains(console_effect),
                    "main->io_step call should carry Console effect row"
                );
            }
            if *callee == bump_id {
                saw_bump_call = true;
                assert!(
                    effects.is_empty(),
                    "main->bump call should stay pure after discharge path"
                );
            }
        }
        for expr_id in stmt.child_exprs() {
            let Some(expr) = residual.program().expr(expr_id) else {
                continue;
            };
            if let ExprKind::PureCall { callee, .. } = expr.kind
                && callee == bump_id
            {
                saw_bump_call = true;
            }
        }
        stack.extend(stmt.child_stmts());
    }
    assert!(saw_local_step_call, "main should call local_step");
    assert!(saw_io_step_call, "main should call io_step");
    assert!(saw_bump_call, "main should call bump");

    assert!(
        residual
            .bta()
            .stage_of_expr
            .values()
            .any(|stage| matches!(stage, Stage::Rt(Reason::UserForcedRuntime))),
        "@runtime block in example should force at least one expression to runtime"
    );
    assert!(
        residual
            .bta()
            .stage_of_expr
            .values()
            .any(|stage| matches!(stage, Stage::Ct)),
        "example should still contain CT expressions"
    );
    assert!(
        residual
            .bta()
            .stage_of_expr
            .values()
            .any(|stage| matches!(stage, Stage::Rt(_))),
        "example should still contain RT expressions"
    );
    assert!(
        !residual
            .bta()
            .stage_of_expr
            .values()
            .any(|stage| matches!(stage, Stage::Rt(Reason::NotPersistable(_)))),
        "example should not trigger non-persistable boundary staging failures"
    );
}

#[test]
fn fused_comptime_entrypoints_match_split_pipeline_c_output() {
    let src = r#"
effect LocalState { fn tick() -> Int }
fn worker(n: Int) -> Int with LocalState {
  if n == 0 { 0 } else {
    let x = do LocalState.tick();
    x + worker(n - 1)
  }
}
fn main() -> Int {
  handle { worker(3) } with LocalState {
    | tick(resume) => resume(1)
  }
}
"#;

    let compiler = Compiler::new(CompilerConfig::default());

    let mut fused_interner = Interner::new();
    let core = compiler.parse_and_lower_to_core(src, SourceId::from_u32(0), &mut fused_interner);
    let staged = compiler.run_v1_evaluate_classify(core);
    let residual = compiler.run_v1_residualize_specialize(staged);
    let normalized = compiler.run_v1_normalize(residual);
    let fused_emitted = c_emit::run(cfg_lower::run(linearize::run(normalized)), &fused_interner);

    let mut split_interner = Interner::new();
    let split_emitted =
        compiler.compile_source_v0_to_c(src, SourceId::from_u32(1), &mut split_interner);

    assert_eq!(
        fused_emitted.c_source, split_emitted.c_source,
        "fused comptime entrypoints should preserve split-pipeline output"
    );
}

#[test]
fn fused_evaluate_classify_matches_split_stage_tables() {
    let src = r#"
effect LocalState { fn tick() -> Int }
fn worker(n: Int) -> Int with LocalState {
  if n == 0 { 0 } else {
    let x = do LocalState.tick();
    x + worker(n - 1)
  }
}
fn main() -> Int {
  handle { worker(3) } with LocalState {
    | tick(resume) => resume(1)
  }
}
"#;
    let compiler = Compiler::new(CompilerConfig::default());

    let mut fused_interner = Interner::new();
    let core = compiler.parse_and_lower_to_core(src, SourceId::from_u32(0), &mut fused_interner);
    let fused = compiler.run_v1_evaluate_classify(core);

    let mut split_interner = Interner::new();
    let split = compiler.compile_source_v0(src, SourceId::from_u32(1), &mut split_interner);

    assert_eq!(
        fused.ct().ct_cache.len(),
        split.ct().ct_cache.len(),
        "fused and split ct caches should classify the same expression count"
    );
    for (expr_id, fused_value) in fused.ct().ct_cache.iter() {
        let split_value = split
            .ct()
            .ct_cache
            .get(&expr_id)
            .expect("split cache should contain every fused cache entry");
        assert_eq!(
            fused_value,
            split_value,
            "fused/split ct literal mismatch at e{}",
            expr_id.as_u32()
        );
    }

    assert_eq!(
        fused.ct().branch_decisions.len(),
        split.ct().branch_decisions.len(),
        "fused and split branch decisions should have identical coverage"
    );
    for (expr_id, fused_decision) in fused.ct().branch_decisions.iter() {
        let split_decision = split
            .ct()
            .branch_decisions
            .get(&expr_id)
            .expect("split branch decisions should contain every fused entry");
        assert_eq!(
            fused_decision,
            split_decision,
            "fused/split branch decision mismatch at e{}",
            expr_id.as_u32()
        );
    }

    assert_eq!(
        fused.bta().stage_of_expr.len(),
        split.bta().stage_of_expr.len(),
        "fused and split stage tables should classify every expression"
    );
    for (expr_id, fused_stage) in fused.bta().stage_of_expr.iter() {
        let split_stage = split
            .bta()
            .stage_of_expr
            .get(&expr_id)
            .expect("split stage table should contain every fused stage entry");
        assert_eq!(
            fused_stage,
            split_stage,
            "fused/split stage mismatch at e{}",
            expr_id.as_u32()
        );
    }

    assert_eq!(
        fused.bta().knownness_of_expr.len(),
        split.bta().knownness_of_expr.len(),
        "fused and split knownness tables should classify every expression"
    );
    for (expr_id, fused_knownness) in fused.bta().knownness_of_expr.iter() {
        let split_knownness = split
            .bta()
            .knownness_of_expr
            .get(&expr_id)
            .expect("split knownness table should contain every fused knownness entry");
        assert_eq!(
            fused_knownness,
            split_knownness,
            "fused/split knownness mismatch at e{}",
            expr_id.as_u32()
        );
    }
}

#[test]
fn residualize_stats_track_literal_embedding_and_branch_pruning() {
    let src = r#"
enum Option { Some(Int), None }

fn main() -> Int {
  let cond = true;
  let x = if cond { 1 + 2 } else { 0 };
  let y = Some(41);
  let z = match y {
    Some(v) => v,
    None => 0,
  };
  x + z
}
"#;
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());
    let residual = compiler.compile_source(src, SourceId::from_u32(0), &mut interner);
    let stats = residual.residual().residualize_stats;

    assert!(
        stats.embedded_literals > 0,
        "residualize should report at least one ct literal embedding"
    );
    assert!(
        stats.pruned_if_branches >= 1,
        "constant-if pruning should increment residualize branch-prune counters"
    );
    assert!(
        stats.pruned_match_branches >= 1,
        "known-match pruning should increment residualize branch-prune counters"
    );
}
