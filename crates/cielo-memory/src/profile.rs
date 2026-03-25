#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MemoryModel {
    #[default]
    ReferenceCounting,
    Tracing,
    Regions,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RefcountAlgorithm {
    Baseline,
    #[default]
    LastUse,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RefcountProfile {
    pub algorithm: RefcountAlgorithm,
    pub optimization: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TracingCollector {
    #[default]
    MarkSweep,
    SemiSpace,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TracingProfile {
    pub collector: TracingCollector,
    pub optimization: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RegionAlgorithm {
    Lexical,
    #[default]
    ConstraintBased,
    FlowSensitive,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RegionProfile {
    pub algorithm: RegionAlgorithm,
    pub optimization: u8,
}

/// All family profiles are stored together for convenient experiment setup.
/// A strategy-specific Salsa query receives only its own field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct MemoryProfile {
    pub model: MemoryModel,
    /// Compatibility knob for callers of the first migration slice. New
    /// experiments should set the selected family profile instead.
    pub optimization: u8,
    pub refcount: RefcountProfile,
    pub tracing: TracingProfile,
    pub regions: RegionProfile,
}
