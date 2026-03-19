use std::collections::HashMap;
use std::ops::Deref;

use cielo_base::EffectLabelId;
use smallvec::SmallVec;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum CapabilityLevel {
    Pure,
    Diverge,
    Alloc,
    LocalState,
    SharedState,
    Io,
    Ffi,
}

impl CapabilityLevel {
    pub const fn is_runtime_only(self) -> bool {
        matches!(self, Self::Io | Self::Ffi | Self::SharedState)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EffectFlags(u16);

impl EffectFlags {
    pub const DISCARDABLE: Self = Self(1 << 0);
    pub const COMMUTATIVE: Self = Self(1 << 1);
    pub const LOCAL_STATE: Self = Self(1 << 2);
    pub const SHARED_STATE: Self = Self(1 << 3);
    pub const OPAQUE_FOR_STAGING: Self = Self(1 << 4);
    pub const CT_ONLY: Self = Self(1 << 5);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

impl std::ops::BitOr for EffectFlags {
    type Output = EffectFlags;

    fn bitor(self, rhs: Self) -> Self::Output {
        EffectFlags(self.0 | rhs.0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct EffectProperties {
    pub level: CapabilityLevel,
    pub flags: EffectFlags,
}

impl EffectProperties {
    pub const fn new(level: CapabilityLevel, flags: EffectFlags) -> Self {
        Self { level, flags }
    }

    pub fn is_ct_eligible(self) -> bool {
        !self.level.is_runtime_only() && !self.flags.contains(EffectFlags::OPAQUE_FOR_STAGING)
    }
}

impl Default for EffectProperties {
    fn default() -> Self {
        Self {
            level: CapabilityLevel::Pure,
            flags: EffectFlags::empty(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct SortedEffectRow(SmallVec<[EffectLabelId; 4]>);

impl SortedEffectRow {
    pub fn empty() -> Self {
        Self(SmallVec::new())
    }

    pub fn singleton(effect: EffectLabelId) -> Self {
        Self(SmallVec::from_slice(&[effect]))
    }

    pub fn from_slice(effects: &[EffectLabelId]) -> Self {
        Self::new(effects.iter().copied())
    }

    pub fn new(effects: impl IntoIterator<Item = EffectLabelId>) -> Self {
        let mut effects: SmallVec<[EffectLabelId; 4]> = effects
            .into_iter()
            .filter(|effect| effect.is_valid())
            .collect();
        effects.sort_unstable();
        effects.dedup();
        Self(effects)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn contains(&self, effect: EffectLabelId) -> bool {
        self.0.binary_search(&effect).is_ok()
    }

    pub fn iter(&self) -> impl Iterator<Item = EffectLabelId> + '_ {
        self.0.iter().copied()
    }

    pub fn as_slice(&self) -> &[EffectLabelId] {
        &self.0
    }

    pub fn union(&self, other: &Self) -> Self {
        let mut merged = self.0.clone();
        for effect in &other.0 {
            if let Err(pos) = merged.binary_search(effect) {
                merged.insert(pos, *effect);
            }
        }
        Self(merged)
    }

    pub fn subtract(&self, other: &Self) -> Self {
        Self(
            self.0
                .iter()
                .copied()
                .filter(|effect| other.0.binary_search(effect).is_err())
                .collect(),
        )
    }
}

impl Deref for SortedEffectRow {
    type Target = [EffectLabelId];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl IntoIterator for SortedEffectRow {
    type Item = EffectLabelId;
    type IntoIter = smallvec::IntoIter<[EffectLabelId; 4]>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

pub fn capability_of_row(
    row: &SortedEffectRow,
    properties: &HashMap<EffectLabelId, EffectProperties>,
) -> CapabilityLevel {
    let mut level = CapabilityLevel::Pure;
    for effect in row.iter() {
        let effect_level = properties
            .get(&effect)
            .map(|props| props.level)
            .unwrap_or(CapabilityLevel::Ffi);
        if effect_level > level {
            level = effect_level;
        }
    }
    level
}

pub fn first_non_thunkable_effect(
    row: &SortedEffectRow,
    properties: &HashMap<EffectLabelId, EffectProperties>,
) -> Option<EffectLabelId> {
    row.iter().find(|effect| {
        let Some(props) = properties.get(effect).copied() else {
            return true;
        };
        !props.is_ct_eligible()
            || props.flags.contains(EffectFlags::SHARED_STATE)
            || props.flags.contains(EffectFlags::OPAQUE_FOR_STAGING)
    })
}

pub fn is_thunkable(
    row: &SortedEffectRow,
    properties: &HashMap<EffectLabelId, EffectProperties>,
) -> bool {
    first_non_thunkable_effect(row, properties).is_none()
        && capability_of_row(row, properties) < CapabilityLevel::Io
}
