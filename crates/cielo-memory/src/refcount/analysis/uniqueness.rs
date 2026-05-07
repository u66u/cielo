//! Is this the last user of this value?
//!
//! Destructive field takes, multi-shot resume (CIELO-19/42), cursor inference
//! (CIELO-24) and Perceus-style reuse all ask that question. This is the single
//! query they share, and it gives the Perceus two-level answer: static where
//! provable, runtime test where not.
//!
//! Liveness cannot answer it. Liveness proves *dead here*, which is what the
//! ARC pass used to infer `Move` from, and CIELO-3 is the bug that came out of
//! the confusion: two `take(a)` calls on one value returned 1 instead of 2.
//!
//! Two properties of today's CFG make the static half sound, and neither
//! survives v2 untouched:
//!
//! * Managed values are written once and read syntactically. Core IR has no
//!   `Stmt::Var`/`Get`/`Put` (`docs/v2/plan_notes.md`), so a syntactic use
//!   count of one bounds the number of live references to one. Indirect stores
//!   break that, and `Origin` then has to come from a graph/mutation analysis
//!   instead of from `collect_definitions`. The matches over `CfgExpr`,
//!   `CfgInstruction` and `CfgTerminator` below are exhaustive on purpose: a
//!   new IR node fails to compile here until someone classifies it.
//! * A constructor the constant table pools is immortal static storage; one it
//!   does not pool is a fresh `cielo_make_ctor` with refcount 1. Both halves
//!   read the same `ConstantTable` the C backend reads, so they cannot drift.
//!
//! Regions (CIELO-38) landed without changing anything here, and the reason is
//! worth recording. Non-escape on its own is *not* stronger than unique: a
//! value can be confined to a scope and still be aliased twice inside it, so
//! seeding `Origin::Fresh` from an escape verdict would be unsound. What is
//! stronger is *region-allocated and non-escaping* -- storage the region owns
//! outright, whose refcount no one can observe after the region closes. That
//! combination is a genuine route to `Fresh`, and it needs a definition this
//! analysis can see it through.
//!
//! `crate::region` deliberately gives region slots no `CfgValueId` (evidence is
//! a `CieloEvidence`, not a `CieloValue`), so today no definition is
//! region-allocated and the route has nothing to fire on. CIELO-19 changes that
//! the moment a continuation environment becomes a value: `collect_definitions`
//! grows an arm for it, and `solve_origins` answers `Fresh` when
//! `region::place` proved its region confined. Call sites keep asking `at`
//! either way.

use std::collections::HashMap;

use cielo_base::ids::{CfgExprId, CfgValueId, SymbolId};
use cielo_ir::cfg::{CfgExpr, CfgInstruction, CfgProgram, CfgTerminator, ctor_literal_key};
use cielo_ir::constants::ConstantTable;

use crate::refcount::analysis::cfg_liveness::CfgUseSite;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Uniqueness {
    /// Statically proven sole owner. The runtime `rc == 1` test can be dropped.
    Unique,
    /// Statically proven to have other owners; a destructive take is illegal
    /// whatever the runtime count says.
    Shared,
    /// Not proven either way. Needs the runtime `rc == 1 && !immortal` test.
    Unknown,
}

/// Where the reference a value holds came from. `Unknown` is the top of the
/// lattice: joins absorb it, and every unclassified definition starts there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Origin {
    Fresh,
    Pooled,
    Unknown,
}

impl Origin {
    fn join(self, other: Self) -> Self {
        if self == other { self } else { Self::Unknown }
    }
}

#[derive(Clone, Copy, Default)]
struct ValueUses {
    count: u32,
    site: Option<CfgUseSite>,
}

/// A definition of a value. Edge arguments carry an expression; results of
/// calls, effect operations and match projections carry nothing this analysis
/// can look through.
#[derive(Clone, Copy)]
enum Definition {
    Expr(CfgExprId),
    Opaque,
}

pub struct UniquenessQuery {
    origins: Vec<Origin>,
    uses: Vec<ValueUses>,
}

impl UniquenessQuery {
    pub fn analyze(cfg: &CfgProgram, constants: &ConstantTable) -> Self {
        let uses = collect_uses(cfg);
        let definitions = collect_definitions(cfg);
        let origins = solve_origins(cfg, constants, &definitions, &uses);
        Self { origins, uses }
    }

    pub fn at(&self, value: CfgValueId, point: CfgUseSite) -> Uniqueness {
        match self.origins.get(value.index()).copied() {
            Some(Origin::Pooled) => Uniqueness::Shared,
            Some(Origin::Fresh) => {
                let uses = self.uses[value.index()];
                // A single syntactic read of a freshly allocated value is the
                // only reader that will ever exist, so at that read the
                // allocation's count is still the one it was born with.
                if uses.count == 1 && uses.site == Some(point) {
                    Uniqueness::Unique
                } else {
                    Uniqueness::Unknown
                }
            }
            Some(Origin::Unknown) | None => Uniqueness::Unknown,
        }
    }
}

fn collect_uses(cfg: &CfgProgram) -> Vec<ValueUses> {
    let mut uses = vec![ValueUses::default(); cfg.values().len()];
    for block in cfg.blocks() {
        for instruction in &block.instructions {
            let Some(node) = cfg.instruction(*instruction) else {
                continue;
            };
            let site = CfgUseSite::Instruction(*instruction);
            match &node.kind {
                CfgInstruction::Let { value, .. } | CfgInstruction::Eval { value, .. } => {
                    record_expr_uses(cfg, *value, site, &mut uses)
                }
                CfgInstruction::HandlerEnter { .. }
                | CfgInstruction::HandlerExit { .. }
                | CfgInstruction::RegionEnter { .. }
                | CfgInstruction::RegionExit { .. }
                | CfgInstruction::StageEnter { .. }
                | CfgInstruction::StageExit { .. }
                | CfgInstruction::Hole
                | CfgInstruction::Error => {}
            }
        }

        let site = CfgUseSite::Terminator(block.id);
        match &block.terminator {
            CfgTerminator::Return(value)
            | CfgTerminator::Branch { cond: value, .. }
            | CfgTerminator::Match {
                scrutinee: value, ..
            }
            | CfgTerminator::Switch {
                selector: value, ..
            } => record_expr_uses(cfg, *value, site, &mut uses),
            CfgTerminator::Goto { args, .. }
            | CfgTerminator::Call { args, .. }
            | CfgTerminator::Perform { args, .. } => {
                for arg in args {
                    record_expr_uses(cfg, *arg, site, &mut uses);
                }
            }
            CfgTerminator::Unreachable => {}
        }
    }
    uses
}

fn record_expr_uses(
    cfg: &CfgProgram,
    expression: CfgExprId,
    site: CfgUseSite,
    uses: &mut [ValueUses],
) {
    let Some(node) = cfg.expr(expression) else {
        return;
    };
    match &node.kind {
        CfgExpr::Value(value) => {
            let Some(entry) = uses.get_mut(value.index()) else {
                return;
            };
            entry.count = entry.count.saturating_add(1);
            entry.site = Some(site);
        }
        CfgExpr::Unary { expr, .. } | CfgExpr::Field { base: expr, .. } => {
            record_expr_uses(cfg, *expr, site, uses)
        }
        CfgExpr::Binary { lhs, rhs, .. } => {
            record_expr_uses(cfg, *lhs, site, uses);
            record_expr_uses(cfg, *rhs, site, uses);
        }
        CfgExpr::PureCall { args, .. }
        | CfgExpr::BuiltinCall { args, .. }
        | CfgExpr::MakeStruct { fields: args, .. }
        | CfgExpr::MakeEnum { fields: args, .. } => {
            for arg in args {
                record_expr_uses(cfg, *arg, site, uses);
            }
        }
        CfgExpr::Literal(_) | CfgExpr::Error => {}
    }
}

/// A successor's parameters are defined by the edge that reaches them: match
/// binders, call results and effect-operation results all arrive that way. Only
/// `Goto` hands over an expression this analysis can look through, and a
/// parameter with no matching argument is opaque rather than undefined, so an
/// arity mismatch degrades instead of silently dropping a definition.
fn collect_definitions(cfg: &CfgProgram) -> HashMap<CfgValueId, Vec<Definition>> {
    let mut definitions: HashMap<CfgValueId, Vec<Definition>> = HashMap::new();
    for block in cfg.blocks() {
        for instruction in &block.instructions {
            let Some(node) = cfg.instruction(*instruction) else {
                continue;
            };
            match &node.kind {
                CfgInstruction::Let { result, value } => {
                    definitions
                        .entry(*result)
                        .or_default()
                        .push(Definition::Expr(*value));
                }
                // `Eval` borrows its operand, and whether its result owns a
                // reference depends on the expression kind (`expr_produces_owned`
                // in the ARC pass). Too weak a definition to look through.
                CfgInstruction::Eval { result, .. } => {
                    definitions
                        .entry(*result)
                        .or_default()
                        .push(Definition::Opaque);
                }
                CfgInstruction::HandlerEnter { .. }
                | CfgInstruction::HandlerExit { .. }
                | CfgInstruction::RegionEnter { .. }
                | CfgInstruction::RegionExit { .. }
                | CfgInstruction::StageEnter { .. }
                | CfgInstruction::StageExit { .. }
                | CfgInstruction::Hole
                | CfgInstruction::Error => {}
            }
        }

        let passed = match &block.terminator {
            CfgTerminator::Goto { args, .. } => args.as_slice(),
            CfgTerminator::Return(_)
            | CfgTerminator::Branch { .. }
            | CfgTerminator::Match { .. }
            | CfgTerminator::Switch { .. }
            | CfgTerminator::Call { .. }
            | CfgTerminator::Perform { .. }
            | CfgTerminator::Unreachable => &[],
        };
        for target in block.terminator.successors() {
            let Some(target) = cfg.block(target) else {
                continue;
            };
            for (index, param) in target.params.iter().enumerate() {
                let definition = passed
                    .get(index)
                    .map_or(Definition::Opaque, |arg| Definition::Expr(*arg));
                definitions.entry(*param).or_default().push(definition);
            }
        }
    }
    definitions
}

/// Every value starts at `Unknown` and can only move off it once all of its
/// definitions are themselves off it. A loop-carried parameter therefore stays
/// `Unknown`: the back edge reads the parameter, which is still `Unknown` when
/// the join runs. Starting from the optimistic end instead would compute a
/// greatest fixpoint and call such a parameter fresh.
fn solve_origins(
    cfg: &CfgProgram,
    constants: &ConstantTable,
    definitions: &HashMap<CfgValueId, Vec<Definition>>,
    uses: &[ValueUses],
) -> Vec<Origin> {
    let mut origins = vec![Origin::Unknown; cfg.values().len()];
    let mut pooled = HashMap::new();
    let mut changed = true;
    while changed {
        changed = false;
        for (value, sources) in definitions {
            let Some(slot) = origins.get(value.index()).copied() else {
                continue;
            };
            if slot != Origin::Unknown {
                continue;
            }
            let mut joined = None;
            for source in sources {
                let origin = match source {
                    Definition::Expr(expr) => {
                        expr_origin(cfg, constants, *expr, &origins, uses, &mut pooled)
                    }
                    Definition::Opaque => Origin::Unknown,
                };
                joined = Some(joined.map_or(origin, |current: Origin| current.join(origin)));
            }
            let joined = joined.unwrap_or(Origin::Unknown);
            if joined != Origin::Unknown {
                origins[value.index()] = joined;
                changed = true;
            }
        }
    }
    origins
}

fn expr_origin(
    cfg: &CfgProgram,
    constants: &ConstantTable,
    expression: CfgExprId,
    origins: &[Origin],
    uses: &[ValueUses],
    pooled: &mut HashMap<CfgExprId, bool>,
) -> Origin {
    let Some(node) = cfg.expr(expression) else {
        return Origin::Unknown;
    };
    match &node.kind {
        // Reading the source once means this definition takes over the whole
        // reference; reading it more than once means somebody else holds one.
        CfgExpr::Value(value) => match uses.get(value.index()) {
            Some(entry) if entry.count == 1 => origins
                .get(value.index())
                .copied()
                .unwrap_or(Origin::Unknown),
            _ => Origin::Unknown,
        },
        CfgExpr::MakeStruct { ty, fields } => ctor_origin(
            cfg,
            constants,
            expression,
            *ty,
            SymbolId::INVALID,
            fields,
            pooled,
        ),
        CfgExpr::MakeEnum {
            ty,
            variant,
            fields,
        } => ctor_origin(cfg, constants, expression, *ty, *variant, fields, pooled),
        // A builtin result is heap-fresh today, but nothing in the signature
        // says so: an interning or caching builtin would return an alias.
        CfgExpr::BuiltinCall { .. }
        | CfgExpr::Literal(_)
        | CfgExpr::Unary { .. }
        | CfgExpr::Binary { .. }
        | CfgExpr::PureCall { .. }
        | CfgExpr::Field { .. }
        | CfgExpr::Error => Origin::Unknown,
    }
}

fn ctor_origin(
    cfg: &CfgProgram,
    constants: &ConstantTable,
    expression: CfgExprId,
    ty: SymbolId,
    variant: SymbolId,
    fields: &[CfgExprId],
    pooled: &mut HashMap<CfgExprId, bool>,
) -> Origin {
    let is_pooled = *pooled.entry(expression).or_insert_with(|| {
        ctor_literal_key(cfg, ty, variant, fields).is_some_and(|key| constants.pools_ctor(&key))
    });
    if is_pooled {
        Origin::Pooled
    } else {
        Origin::Fresh
    }
}
