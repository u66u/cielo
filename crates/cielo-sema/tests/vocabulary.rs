use cielo_base::SymbolId;
use cielo_ir::core::{CoreTypeRef, PrimitiveTypeRef};
use cielo_ir::ownership::OwnershipClass;
use cielo_sema::ownership::classify_core_type_ref;
use cielo_sema::ty::{Persistability, PrimitiveType, TypeKind, TypeStore};

#[test]
fn primitive_and_named_values_have_explicit_ownership_classes() {
    assert_eq!(
        classify_core_type_ref(&CoreTypeRef::Primitive(PrimitiveTypeRef::Int)),
        OwnershipClass::Trivial
    );
    assert_eq!(
        classify_core_type_ref(&CoreTypeRef::Named(SymbolId::from_u32(0))),
        OwnershipClass::Managed
    );
}

#[test]
fn type_store_computes_persistability_at_the_type_boundary() {
    let mut store = TypeStore::new();
    let int = store.intern(TypeKind::Primitive(PrimitiveType::Int));
    let function = store.intern(TypeKind::Function(cielo_sema::ty::FunctionType {
        params: vec![int],
        ret: int,
        effects: cielo_ir::effect::SortedEffectRow::empty(),
    }));
    assert_eq!(store.persistability(int), Persistability::Trivial);
    assert_eq!(
        store.persistability(function),
        Persistability::NonPersistable
    );
}
