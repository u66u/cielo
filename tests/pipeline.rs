use cielo::common::diagnostics::DiagnosticBag;
use cielo::common::ids::{EffectLabelId, ExprId, FuncId, SourceId, SymbolId, VarId};
use cielo::common::span::Span;
use cielo::common::symbols::Interner;
use cielo::ir::core::{
    CoreProgram, CoreTypeRef, ExprKind, ExprNode, FunctionDecl, Literal, MatchArm,
    PrimitiveTypeRef, StmtKind, StmtNode,
};
use cielo::passes::residualize;
use cielo::pipeline::phases::{
    BranchDecision, BtaClassified, BtaTables, CtPropagationTables, MonomorphizationSummary,
    SemanticTables, Stage,
};
use cielo::pipeline::provenance::runtime_provenance_lines;
use cielo::sema::effect::SortedEffectRow;
use cielo::{Compiler, CompilerConfig};

#[test]
fn compiles_source_through_v0_skeleton_pipeline() {
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
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);
    assert_eq!(residual.program().functions().len(), 2);
    assert_eq!(residual.program().entrypoints().len(), 1);
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
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);
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
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);
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
    let residual = compiler.compile_source_v0(src, SourceId::from_u32(0), &mut interner);

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
    let classified = BtaClassified::new(
        program,
        DiagnosticBag::default(),
        sema,
        MonomorphizationSummary::default(),
        ct,
        BtaTables::default(),
    );
    let residual = residualize::run(classified);

    assert!(matches!(
        residual.program().expr(sum).map(|expr| &expr.kind),
        Some(ExprKind::Literal(Literal::Int(3)))
    ));
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
fn v1_example_snapshot_matches_expected_registration_and_staging_counts() {
    let src = include_str!("../examples/v1_test.cielo");
    let mut interner = Interner::new();
    let compiler = Compiler::new(CompilerConfig::default());

    let core = compiler.parse_and_lower_to_core(src, SourceId::from_u32(0), &mut interner);
    assert_eq!(core.program().effects().len(), 2);
    assert_eq!(interner.resolve(core.program().effects()[0].name), Some("Console"));
    assert_eq!(interner.resolve(core.program().effects()[1].name), Some("LocalState"));
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
    assert_eq!(core.program().effects()[0].operations[0].param_types.len(), 1);
    assert_eq!(core.program().effects()[1].operations[0].param_types.len(), 0);

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
    let typed_exprs = residual
        .sema()
        .type_of_expr
        .iter()
        .filter(|entry| entry.is_some())
        .count();
    assert_eq!(typed_exprs, 39);
    assert_eq!(residual.sema().type_of_expr.len(), 39);

    let effectful_stmts = residual
        .sema()
        .effects_of_stmt
        .iter()
        .filter(|row| !row.is_empty())
        .count();
    assert_eq!(effectful_stmts, 6);
    assert_eq!(residual.sema().effects_of_stmt.len(), 34);

    let ct_count = residual
        .bta()
        .stage_of_expr
        .values()
        .filter(|stage| matches!(stage, Stage::Ct))
        .count();
    let rt_count = residual
        .bta()
        .stage_of_expr
        .values()
        .filter(|stage| matches!(stage, Stage::Rt(_)))
        .count();
    assert_eq!(ct_count, 15);
    assert_eq!(rt_count, 24);
    assert_eq!(residual.diagnostics().entries().len(), 0);

    let e8 = runtime_provenance_lines(residual.program(), residual.bta(), ExprId::new(8), 5);
    assert_eq!(
        e8,
        vec![
            " 1. e8: runtime classification has not been refined yet".to_owned(),
            " 2. v0: parameter #1 of f1 is runtime".to_owned(),
        ]
    );
    let e10 = runtime_provenance_lines(residual.program(), residual.bta(), ExprId::new(10), 5);
    assert_eq!(
        e10,
        vec![
            " 1. e10: runtime classification has not been refined yet".to_owned(),
            " 2. e8: runtime classification has not been refined yet".to_owned(),
            " 3. v0: parameter #1 of f1 is runtime".to_owned(),
        ]
    );
    let e11 = runtime_provenance_lines(residual.program(), residual.bta(), ExprId::new(11), 5);
    assert_eq!(
        e11,
        vec![
            " 1. e11: runtime classification has not been refined yet".to_owned(),
            " 2. v1: parameter #1 of f2 is runtime".to_owned(),
        ]
    );
}
