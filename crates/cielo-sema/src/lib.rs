//! Semantic vocabulary and local facts.
//!
//! The typechecker itself remains in the integration crate for now because it
//! still consumes the legacy phase products.  These definitions are already
//! independent and therefore live here, below the algorithms that use them.

pub mod facts;
pub mod ownership;
pub mod ty;
pub mod typecheck;

pub use facts::SemanticTables;
pub use ownership::OwnershipClass;
pub use ty::{Persistability, TypeKind, TypeStore};
