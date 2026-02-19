use crate::ir::core::CoreTypeRef;
use crate::sema::ty::{PrimitiveType, TypeKind};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OwnershipClass {
    #[default]
    Trivial,
    RcManaged,
    BorrowedView,
}

impl OwnershipClass {
    pub fn merge(self, other: Self) -> Self {
        use OwnershipClass::{BorrowedView, RcManaged, Trivial};
        match (self, other) {
            (RcManaged, _) | (_, RcManaged) => RcManaged,
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
        TypeKind::Struct { .. } | TypeKind::Enum { .. } => OwnershipClass::RcManaged,
        TypeKind::Function(_) | TypeKind::TypeParam(_) | TypeKind::Error => {
            OwnershipClass::BorrowedView
        }
    }
}

pub fn classify_core_type_ref(ty: &CoreTypeRef) -> OwnershipClass {
    match ty {
        CoreTypeRef::Unit => OwnershipClass::Trivial,
        CoreTypeRef::Primitive(primitive) => match primitive {
            crate::ir::core::PrimitiveTypeRef::String => OwnershipClass::BorrowedView,
            crate::ir::core::PrimitiveTypeRef::Bool
            | crate::ir::core::PrimitiveTypeRef::Int
            | crate::ir::core::PrimitiveTypeRef::Float
            | crate::ir::core::PrimitiveTypeRef::Char => OwnershipClass::Trivial,
        },
        CoreTypeRef::Named(_) => OwnershipClass::RcManaged,
        CoreTypeRef::Unknown => OwnershipClass::BorrowedView,
    }
}
