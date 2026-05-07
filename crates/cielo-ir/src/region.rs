//! One allocation substrate for everything a lexical scope owns.
//!
//! Each `Handle` opens a region (Müller, Schuster, Starup, Ostermann,
//! Brachthäuser, *From Capabilities to Regions*, OOPSLA 2023): the capability
//! scope and the allocation scope are the same scope, so handler evidence,
//! continuation environments (CIELO-19/42) and closure environments can share
//! one placement decision instead of each growing a bespoke record.
//!
//! A region owns *slots*, not values. A slot is storage of a fixed C type whose
//! lifetime is the region's, and it deliberately has no [`CfgValueId`]: a
//! `CfgValueId` is a `CieloValue`, and evidence is not one. Keeping slots out of
//! the value space is what lets the ARC pass ignore regions entirely rather than
//! learn a second ownership discipline.
//!
//! [`Placement`] defaults to [`Placement::Arena`] so that a CFG which never
//! reached the escape analysis is heap-backed rather than silently dangling.

use cielo_base::{CfgHandlerId, CfgRegionId, EffectLabelId};

/// What opened the region. The owner is what makes the scope observable to the
/// backend; a region with no owner would have no place to emit open and close.
///
/// A handler owner carries the effect it discharges. Reading that off a slot
/// instead would tie the answer to slot order, which stops being one slot as
/// soon as continuation environments join the region.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RegionOwner {
    Handler {
        handler: CfgHandlerId,
        effect: EffectLabelId,
    },
}

/// What a slot holds. One arm per kind of record that lives and dies with a
/// scope; the escape analysis matches exhaustively so a new arm cannot inherit
/// evidence's placement rule by accident.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RegionSlotKind {
    HandlerEvidence { effect: EffectLabelId },
}

/// Where the backend must put a slot's storage.
///
/// `Arena` is the default because it is the answer that is always correct: a
/// region whose placement was never computed still compiles to working code,
/// just slower. `Stack` is only ever installed by an analysis that proved the
/// slot's address cannot outlive the region.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Placement {
    Stack,
    #[default]
    Arena,
}

#[derive(Clone, Copy, Debug)]
pub struct RegionSlot {
    pub kind: RegionSlotKind,
    pub placement: Placement,
}

#[derive(Clone, Debug)]
pub struct CfgRegion {
    pub id: CfgRegionId,
    pub owner: RegionOwner,
    pub slots: Vec<RegionSlot>,
}

impl CfgRegion {
    /// True when nothing in the region needs the arena, which is what lets the
    /// backend skip emitting the open/close pair altogether.
    pub fn is_fully_stack(&self) -> bool {
        self.slots
            .iter()
            .all(|slot| slot.placement == Placement::Stack)
    }
}
