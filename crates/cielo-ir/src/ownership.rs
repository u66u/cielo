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

/// What a node does with one of its operands.
///
/// `Owned` operands are stored or handed onwards -- a constructor field, a call
/// argument, a block argument, a returned value -- so each one needs a reference
/// of its own. `Read` operands are only inspected to compute a result, so the
/// parent can borrow the caller's reference.
///
/// Reference counting is the only consumer; every other traversal projects roles
/// away with `child_exprs()`. The roles are declared next to each node kind so
/// that a walker cannot silently disagree with the node about ownership.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum OperandRole {
    Owned,
    Read,
}
