use crate::ty::{PrimitiveType, TypeKind};
use cielo_ir::core::CoreTypeRef;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OwnershipClass {
    #[default]
    Trivial,
    /// A value requiring a strategy-specific managed representation.
    Managed,
    BorrowedView,
}

impl OwnershipClass {
    /// Compatibility name for the pre-database ARC implementation.  New
    /// analyses should use `Managed`; the semantic layer does not choose a
    /// collector.
    #[allow(non_upper_case_globals)]
    pub const RcManaged: Self = Self::Managed;

    pub fn merge(self, other: Self) -> Self {
        use OwnershipClass::{BorrowedView, Managed, Trivial};
        match (self, other) {
            (Managed, _) | (_, Managed) => Managed,
            (BorrowedView, _) | (_, BorrowedView) => BorrowedView,
            (Trivial, Trivial) => Trivial,
        }
    }
}

pub fn classify_type_kind(kind: &TypeKind) -> OwnershipClass {
    match kind {
        TypeKind::Primitive(primitive) => match primitive {
            PrimitiveType::String => OwnershipClass::BorrowedView,
            PrimitiveType::Unit
            | PrimitiveType::Bool
            | PrimitiveType::Int
            | PrimitiveType::Float
            | PrimitiveType::Char => OwnershipClass::Trivial,
        },
        TypeKind::Struct { .. } | TypeKind::Enum { .. } => OwnershipClass::Managed,
        TypeKind::Function(_) | TypeKind::TypeParam(_) | TypeKind::Error => {
            OwnershipClass::BorrowedView
        }
    }
}

pub fn classify_core_type_ref(ty: &CoreTypeRef) -> OwnershipClass {
    match ty {
        CoreTypeRef::Unit => OwnershipClass::Trivial,
        CoreTypeRef::Primitive(primitive) => match primitive {
            cielo_ir::core::PrimitiveTypeRef::String => OwnershipClass::BorrowedView,
            cielo_ir::core::PrimitiveTypeRef::Bool
            | cielo_ir::core::PrimitiveTypeRef::Int
            | cielo_ir::core::PrimitiveTypeRef::Float
            | cielo_ir::core::PrimitiveTypeRef::Char => OwnershipClass::Trivial,
        },
        CoreTypeRef::Named(_) => OwnershipClass::Managed,
        CoreTypeRef::Unknown => OwnershipClass::BorrowedView,
    }
}
