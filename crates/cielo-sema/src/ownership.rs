use crate::ty::{PrimitiveType, TypeKind};
use cielo_ir::core::CoreTypeRef;
use cielo_ir::ownership::OwnershipClass;

pub fn classify_type_kind(kind: &TypeKind) -> OwnershipClass {
    match kind {
        TypeKind::Primitive(primitive) => match primitive {
            // Refcounted like a constructor: runtime-built strings own a heap
            // block. Pooled literals are immortal, so ARC leaves them alone.
            PrimitiveType::String => OwnershipClass::Managed,
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
            cielo_ir::core::PrimitiveTypeRef::String => OwnershipClass::Managed,
            cielo_ir::core::PrimitiveTypeRef::Bool
            | cielo_ir::core::PrimitiveTypeRef::Int
            | cielo_ir::core::PrimitiveTypeRef::Float
            | cielo_ir::core::PrimitiveTypeRef::Char => OwnershipClass::Trivial,
        },
        CoreTypeRef::Named(_) | CoreTypeRef::Applied { .. } => OwnershipClass::Managed,
        // A type parameter is only seen on an unspecialized signature, whose
        // instances carry the real ownership class.
        CoreTypeRef::Param(_) | CoreTypeRef::Unknown => OwnershipClass::BorrowedView,
    }
}
