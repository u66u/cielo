//! Can this region's storage live on the C stack?
//!
//! A region's slots are freed when the region closes. Stack placement is only
//! sound when nothing that outlives the close can still hold a slot's address.
//! Getting this wrong in the permissive direction produces a use-after-free with
//! no diagnostic, so every question this analysis cannot answer is answered
//! [`Placement::Arena`].
//!
//! # What the proof rests on
//!
//! A region is *control-confined* when every path that enters it leaves through
//! its own `RegionExit` and nothing inside it can capture a continuation. Four
//! invariants make that checkable on today's CFG, and each names the feature
//! that breaks it:
//!
//! * **Evidence addresses reach exactly one place.** `HandlerEnter` stores
//!   `&evidence` into the runtime's handler stack and `HandlerExit` pops it, so
//!   the only escape route is control leaving the region without popping. A
//!   future lowering that passes evidence as a call argument (evidence-passing
//!   style, `docs/v1/decisions.md`) adds a route this analysis does not model.
//! * **No continuation object exists.** `cielo_dispatch_with_evidence` passes a
//!   null continuation, and a residual clause (CIELO-19) is admitted only when
//!   it is tail-resumptive: it *returns* the resumption argument to the perform
//!   site rather than receiving a continuation it could store. So a `Perform`
//!   of the region's own effect still returns to the region or not at all,
//!   whether it was inlined or dispatched. CIELO-42 (multi-shot resumption) is
//!   what breaks this: once a clause receives a real `CieloContinuation`, a
//!   `Perform` inside the extent can be resumed after the close, and [`escapes`]
//!   must stop treating the region's own effect as confined.
//!
//!   CIELO-39 *does* reify a continuation, and deliberately not as an object: a
//!   defunctionalised clause enters one shared copy of the continuation by
//!   `Goto` and leaves it by `Switch`, both inside the frame that opened the
//!   region. The code pointer is a case index, the values it carries are block
//!   parameters, and nothing takes the address of anything. That is why it needs
//!   no [`RegionSlotKind`] arm of its own and why the arm above is unchanged:
//!   the reified continuation is only reachable while the region is open, and
//!   the paths out of it are the same paths inlining would have produced. A
//!   continuation that outlives the frame is the case that needs a slot, and
//!   that is CIELO-42's, not this one's.
//! * **Regions nest with the C frame.** A slot is placed per enclosing
//!   function, so the extent walk never crosses a `CfgFunction` boundary.
//!   Regions that outlive their opening frame need a different substrate.
//! * **The extent is syntactic.** It is the block set reachable from the
//!   `RegionEnter` that stops at `RegionExit`, so an unstructured jump back into
//!   a closed region would be invisible here. `cfg_lower` only ever produces the
//!   bracketed shape -- a resumption jump included, which enters a block the
//!   walk already reaches from the same `RegionEnter` -- and [`region_extent`]
//!   returning no exit is itself treated as escaping.
//!
//! Closure environments (CIELO-25) and an escaping continuation's environment
//! (CIELO-42) get a [`RegionSlotKind`] arm rather than a parallel analysis: the
//! confinement question is the same one, and the match here is exhaustive so a
//! new arm cannot silently inherit evidence's answer.

use std::collections::HashSet;

use cielo_base::{CfgBlockId, CfgRegionId};
use cielo_ir::cfg::{CfgCallConvention, CfgInstruction, CfgProgram, CfgTerminator};
use cielo_ir::region::{Placement, RegionOwner, RegionSlotKind};

/// How many slots ended up where, for reporting the non-escape rate the design
/// doc estimates at ~95%. Read back off a placed CFG rather than returned from
/// [`place`], so a consumer that did not run the pass cannot mistake a
/// zero-filled struct for a measurement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RegionPlacementStats {
    pub stack_slots: u32,
    pub arena_slots: u32,
}

impl RegionPlacementStats {
    pub fn of(cfg: &CfgProgram) -> Self {
        let mut stats = Self::default();
        for region in cfg.regions() {
            for slot in &region.slots {
                match slot.placement {
                    Placement::Stack => stats.stack_slots = stats.stack_slots.saturating_add(1),
                    Placement::Arena => stats.arena_slots = stats.arena_slots.saturating_add(1),
                }
            }
        }
        stats
    }

    pub fn total(self) -> u32 {
        self.stack_slots.saturating_add(self.arena_slots)
    }
}

/// Replaces every slot's placement with the strongest one this analysis can
/// justify. Called from `cfg_lower::run`, not from a memory strategy: an
/// unmanaged build still has to free its arenas, and a strategy that forgot to
/// ask would silently get [`Placement::Arena`] everywhere.
pub fn place(cfg: &mut CfgProgram) {
    let ids = cfg
        .regions()
        .iter()
        .map(|region| region.id)
        .collect::<Vec<_>>();
    for id in ids {
        let confined = !escapes(cfg, id);
        let Some(region) = cfg.region_mut(id) else {
            continue;
        };
        for slot in &mut region.slots {
            slot.placement = match slot.kind {
                RegionSlotKind::HandlerEvidence { .. } if confined => Placement::Stack,
                RegionSlotKind::HandlerEvidence { .. } => Placement::Arena,
            };
        }
    }
}

/// True when something inside `region` can hold a slot address past the close.
fn escapes(cfg: &CfgProgram, region: CfgRegionId) -> bool {
    let Some(extent) = region_extent(cfg, region) else {
        return true;
    };
    let own_effect = cfg.region(region).map(|region| match region.owner {
        RegionOwner::Handler { effect, .. } => effect,
    });
    extent.iter().any(|block| {
        let Some(block) = cfg.block(*block) else {
            return true;
        };
        match &block.terminator {
            // Returning from inside the region skips the close: the handler
            // frame is still on the runtime stack when the C frame dies.
            CfgTerminator::Return(_) => true,
            // A control call may suspend, and nothing here bounds when -- or
            // whether -- it resumes relative to the close.
            CfgTerminator::Call { convention, .. } => *convention == CfgCallConvention::Control,
            // An effect this region does not handle unwinds past it. Its own
            // effect is confined only while no continuation *object* exists to
            // resume it after the close; see the module doc.
            CfgTerminator::Perform { effect, .. } => Some(*effect) != own_effect,
            // A trap ends the process, so no dangling address is ever read.
            CfgTerminator::Unreachable => false,
            // `Switch` is a defunctionalised continuation's dispatch, and its
            // targets are blocks of this same function that the extent walk
            // already reaches. It moves control, never an address.
            CfgTerminator::Goto { .. }
            | CfgTerminator::Branch { .. }
            | CfgTerminator::Match { .. }
            | CfgTerminator::Switch { .. } => false,
        }
    })
}

/// The blocks between `RegionEnter` and `RegionExit`. Blocks holding the exit
/// are outside the extent -- the close runs there, so an address is already
/// dead by their terminator.
///
/// `None` when the region is not bracketed by exactly one entry with at least
/// one reachable exit, which callers must read as escaping.
fn region_extent(cfg: &CfgProgram, region: CfgRegionId) -> Option<HashSet<CfgBlockId>> {
    let mut entry = None;
    for block in cfg.blocks() {
        if block_has(
            cfg,
            block.id,
            |kind| matches!(kind, CfgInstruction::RegionEnter { region: found } if *found == region),
        ) {
            if entry.is_some() {
                return None;
            }
            entry = Some(block.id);
        }
    }

    let mut extent = HashSet::new();
    let mut saw_exit = false;
    let mut stack = vec![entry?];
    while let Some(block_id) = stack.pop() {
        if block_has(
            cfg,
            block_id,
            |kind| matches!(kind, CfgInstruction::RegionExit { region: found } if *found == region),
        ) {
            saw_exit = true;
            continue;
        }
        if !extent.insert(block_id) {
            continue;
        }
        stack.extend(cfg.block(block_id)?.terminator.successors());
    }
    saw_exit.then_some(extent)
}

fn block_has(
    cfg: &CfgProgram,
    block: CfgBlockId,
    predicate: impl Fn(&CfgInstruction) -> bool,
) -> bool {
    let Some(block) = cfg.block(block) else {
        return false;
    };
    block.instructions.iter().any(|instruction| {
        cfg.instruction(*instruction)
            .is_some_and(|node| predicate(&node.kind))
    })
}
