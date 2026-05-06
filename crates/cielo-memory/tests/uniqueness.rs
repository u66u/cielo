use cielo_base::SymbolId;
use cielo_ir::cfg::{CfgExpr, CfgInstruction, CfgProgram, CfgTerminator};
use cielo_ir::constants::{
    ConstantEmbedStrategy, ConstantEntry, ConstantKey, ConstantTable, CtorFieldKey, CtorLiteralKey,
};
use cielo_ir::core::Literal;
use cielo_memory::refcount::analysis::cfg_liveness::CfgUseSite;
use cielo_memory::refcount::analysis::uniqueness::{Uniqueness, UniquenessQuery};

fn ty() -> SymbolId {
    SymbolId::new(1)
}

fn variant() -> SymbolId {
    SymbolId::new(2)
}

fn pooled_table() -> ConstantTable {
    ConstantTable {
        entries: vec![ConstantEntry {
            key: ConstantKey::Ctor(CtorLiteralKey {
                ty: ty(),
                variant: variant(),
                fields: vec![CtorFieldKey::Int(1)],
            }),
            strategy: ConstantEmbedStrategy::Pooled,
            estimated_size_bytes: 0,
        }],
        entry_cap_bytes: 0,
        unit_cap_bytes: 0,
        total_size_bytes: 0,
    }
}

/// `let v = Ctor(opaque); match v { .. }` is the whole static case: the
/// allocation is fresh and this match is its only reader.
#[test]
fn a_fresh_allocation_read_once_is_unique_at_that_read() {
    let mut cfg = CfgProgram::default();
    let opaque = cfg.push_value(None);
    let boxed = cfg.push_value(None);
    let opaque_expr = cfg.push_expr(CfgExpr::Value(opaque), None);
    let ctor = cfg.push_expr(
        CfgExpr::MakeEnum {
            ty: ty(),
            variant: variant(),
            fields: vec![opaque_expr],
        },
        None,
    );
    let boxed_expr = cfg.push_expr(CfgExpr::Value(boxed), None);

    let exit = cfg.push_block(Vec::new(), None);
    cfg.set_terminator(exit, CfgTerminator::Unreachable);
    let block = cfg.push_block(vec![opaque], None);
    cfg.push_instruction(
        block,
        CfgInstruction::Let {
            result: boxed,
            value: ctor,
        },
        None,
    );
    cfg.set_terminator(
        block,
        CfgTerminator::Match {
            scrutinee: boxed_expr,
            arms: Vec::new(),
            default: exit,
        },
    );

    let query = UniquenessQuery::analyze(&cfg, &ConstantTable::default());
    assert_eq!(
        query.at(boxed, CfgUseSite::Terminator(block)),
        Uniqueness::Unique
    );
}

/// The same allocation read at two sites: at neither is the count known, so
/// the runtime test stays. This is the CIELO-3 shape.
#[test]
fn a_fresh_allocation_read_twice_is_unknown() {
    let mut cfg = CfgProgram::default();
    let opaque = cfg.push_value(None);
    let boxed = cfg.push_value(None);
    let seen = cfg.push_value(None);
    let opaque_expr = cfg.push_expr(CfgExpr::Value(opaque), None);
    let ctor = cfg.push_expr(
        CfgExpr::MakeEnum {
            ty: ty(),
            variant: variant(),
            fields: vec![opaque_expr],
        },
        None,
    );
    let first = cfg.push_expr(CfgExpr::Value(boxed), None);
    let second = cfg.push_expr(CfgExpr::Value(boxed), None);

    let exit = cfg.push_block(Vec::new(), None);
    cfg.set_terminator(exit, CfgTerminator::Unreachable);
    let block = cfg.push_block(vec![opaque], None);
    cfg.push_instruction(
        block,
        CfgInstruction::Let {
            result: boxed,
            value: ctor,
        },
        None,
    );
    cfg.push_instruction(
        block,
        CfgInstruction::Eval {
            result: seen,
            value: first,
        },
        None,
    );
    cfg.set_terminator(
        block,
        CfgTerminator::Match {
            scrutinee: second,
            arms: Vec::new(),
            default: exit,
        },
    );

    let query = UniquenessQuery::analyze(&cfg, &ConstantTable::default());
    assert_eq!(
        query.at(boxed, CfgUseSite::Terminator(block)),
        Uniqueness::Unknown
    );
}

/// A pooled constructor is emitted as immortal static storage. It is aliased by
/// construction, so the answer is `Shared` at every point, not `Unknown`.
#[test]
fn a_pooled_constructor_is_shared_everywhere() {
    let mut cfg = CfgProgram::default();
    let boxed = cfg.push_value(None);
    let literal = cfg.push_expr(CfgExpr::Literal(Literal::Int(1)), None);
    let ctor = cfg.push_expr(
        CfgExpr::MakeEnum {
            ty: ty(),
            variant: variant(),
            fields: vec![literal],
        },
        None,
    );
    let boxed_expr = cfg.push_expr(CfgExpr::Value(boxed), None);

    let exit = cfg.push_block(Vec::new(), None);
    cfg.set_terminator(exit, CfgTerminator::Unreachable);
    let block = cfg.push_block(Vec::new(), None);
    cfg.push_instruction(
        block,
        CfgInstruction::Let {
            result: boxed,
            value: ctor,
        },
        None,
    );
    cfg.set_terminator(
        block,
        CfgTerminator::Match {
            scrutinee: boxed_expr,
            arms: Vec::new(),
            default: exit,
        },
    );

    assert_eq!(
        UniquenessQuery::analyze(&cfg, &pooled_table()).at(boxed, CfgUseSite::Terminator(block)),
        Uniqueness::Shared
    );
    // The same constructor is a fresh allocation when the table does not pool
    // it: the backend then emits `cielo_make_ctor`.
    assert_eq!(
        UniquenessQuery::analyze(&cfg, &ConstantTable::default())
            .at(boxed, CfgUseSite::Terminator(block)),
        Uniqueness::Unique
    );
}

/// A block parameter fed by a fresh allocation on every edge is fresh. This is
/// the shape `let b = if c { Ctor(x) } else { Ctor(y) }` lowers to, and the only
/// one that fires on real source today.
#[test]
fn a_parameter_joined_from_fresh_edges_is_unique() {
    let mut cfg = CfgProgram::default();
    let opaque = cfg.push_value(None);
    let joined = cfg.push_value(None);
    let opaque_expr = cfg.push_expr(CfgExpr::Value(opaque), None);
    let left = cfg.push_expr(
        CfgExpr::MakeEnum {
            ty: ty(),
            variant: variant(),
            fields: vec![opaque_expr],
        },
        None,
    );
    let right = cfg.push_expr(
        CfgExpr::MakeEnum {
            ty: ty(),
            variant: variant(),
            fields: Vec::new(),
        },
        None,
    );
    let joined_expr = cfg.push_expr(CfgExpr::Value(joined), None);
    let cond = cfg.push_expr(CfgExpr::Literal(Literal::Bool(true)), None);

    let exit = cfg.push_block(Vec::new(), None);
    cfg.set_terminator(exit, CfgTerminator::Unreachable);
    let merge = cfg.push_block(vec![joined], None);
    cfg.set_terminator(
        merge,
        CfgTerminator::Match {
            scrutinee: joined_expr,
            arms: Vec::new(),
            default: exit,
        },
    );
    let then_block = cfg.push_block(Vec::new(), None);
    cfg.set_terminator(
        then_block,
        CfgTerminator::Goto {
            target: merge,
            args: vec![left],
        },
    );
    let else_block = cfg.push_block(Vec::new(), None);
    cfg.set_terminator(
        else_block,
        CfgTerminator::Goto {
            target: merge,
            args: vec![right],
        },
    );
    let entry = cfg.push_block(vec![opaque], None);
    cfg.set_terminator(
        entry,
        CfgTerminator::Branch {
            cond,
            then_target: then_block,
            else_target: else_block,
        },
    );

    let query = UniquenessQuery::analyze(&cfg, &ConstantTable::default());
    assert_eq!(
        query.at(joined, CfgUseSite::Terminator(merge)),
        Uniqueness::Unique
    );
}

/// A parameter that flows back into itself must not be called fresh. Solving
/// from the optimistic end of the lattice would do exactly that.
#[test]
fn a_loop_carried_parameter_stays_unknown() {
    let mut cfg = CfgProgram::default();
    let carried = cfg.push_value(None);
    let seed = cfg.push_expr(
        CfgExpr::MakeEnum {
            ty: ty(),
            variant: variant(),
            fields: Vec::new(),
        },
        None,
    );
    let carried_expr = cfg.push_expr(CfgExpr::Value(carried), None);
    let back_arg = cfg.push_expr(CfgExpr::Value(carried), None);

    let exit = cfg.push_block(Vec::new(), None);
    cfg.set_terminator(exit, CfgTerminator::Unreachable);
    let header = cfg.push_block(vec![carried], None);
    cfg.set_terminator(
        header,
        CfgTerminator::Match {
            scrutinee: carried_expr,
            arms: Vec::new(),
            default: exit,
        },
    );
    let latch = cfg.push_block(Vec::new(), None);
    cfg.set_terminator(
        latch,
        CfgTerminator::Goto {
            target: header,
            args: vec![back_arg],
        },
    );
    let entry = cfg.push_block(Vec::new(), None);
    cfg.set_terminator(
        entry,
        CfgTerminator::Goto {
            target: header,
            args: vec![seed],
        },
    );

    let query = UniquenessQuery::analyze(&cfg, &ConstantTable::default());
    assert_eq!(
        query.at(carried, CfgUseSite::Terminator(header)),
        Uniqueness::Unknown
    );
    assert_eq!(
        query.at(carried, CfgUseSite::Terminator(latch)),
        Uniqueness::Unknown
    );
}

/// Call and effect-operation results are the callee's to alias, so the runtime
/// test stays even though the value is read exactly once.
#[test]
fn a_call_result_is_unknown() {
    let mut cfg = CfgProgram::default();
    let result = cfg.push_value(None);
    let result_expr = cfg.push_expr(CfgExpr::Value(result), None);

    let exit = cfg.push_block(Vec::new(), None);
    cfg.set_terminator(exit, CfgTerminator::Unreachable);
    let after = cfg.push_block(vec![result], None);
    cfg.set_terminator(
        after,
        CfgTerminator::Match {
            scrutinee: result_expr,
            arms: Vec::new(),
            default: exit,
        },
    );
    let entry = cfg.push_block(Vec::new(), None);
    cfg.set_terminator(
        entry,
        CfgTerminator::Call {
            convention: cielo_ir::cfg::CfgCallConvention::Pure,
            callee: ty(),
            callee_fn: cielo_base::CfgFuncId::new(0),
            args: Vec::new(),
            result,
            target: after,
        },
    );

    let query = UniquenessQuery::analyze(&cfg, &ConstantTable::default());
    assert_eq!(
        query.at(result, CfgUseSite::Terminator(after)),
        Uniqueness::Unknown
    );
}
