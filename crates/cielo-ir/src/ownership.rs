//! Collector-neutral value ownership vocabulary.

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OwnershipClass {
    #[default]
    Trivial,
    /// A value that requires the selected memory strategy.
    Managed,
    /// A non-owning view whose lifetime is controlled elsewhere.
    BorrowedView,
}

impl OwnershipClass {
    pub fn merge(self, other: Self) -> Self {
        use OwnershipClass::{BorrowedView, Managed, Trivial};
        match (self, other) {
            (Managed, _) | (_, Managed) => Managed,
            (BorrowedView, _) | (_, BorrowedView) => BorrowedView,
            (Trivial, Trivial) => Trivial,
        }
    }

    pub fn is_managed(self) -> bool {
        matches!(self, Self::Managed)
    }
}
