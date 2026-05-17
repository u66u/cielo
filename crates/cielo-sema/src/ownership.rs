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
        // A function value is a refcounted closure record, whatever it points
        // at: the environment it owns has to be released with it.
        TypeKind::Struct { .. } | TypeKind::Enum { .. } | TypeKind::Function(_) => {
            OwnershipClass::Managed
        }
        TypeKind::TypeParam(_) | TypeKind::Error => OwnershipClass::BorrowedView,
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
        CoreTypeRef::Named(_) | CoreTypeRef::Applied { .. } | CoreTypeRef::Func { .. } => {
            OwnershipClass::Managed
        }
        // A type parameter is only seen on an unspecialized signature, whose
        // instances carry the real ownership class.
        CoreTypeRef::Param(_) | CoreTypeRef::Unknown => OwnershipClass::BorrowedView,
    }
}
