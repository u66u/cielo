use crate::common::ids::{SymbolId, TypeId};
use crate::sema::effect::SortedEffectRow;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Persistability {
    Trivial,
    Serializable,
    NonPersistable,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PrimitiveType {
    Unit,
    Bool,
    Int,
    Float,
    Char,
    String,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StructField {
    pub name: SymbolId,
    pub ty: TypeId,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EnumVariant {
    pub name: SymbolId,
    pub fields: Vec<TypeId>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FunctionType {
    pub params: Vec<TypeId>,
    pub ret: TypeId,
    pub effects: SortedEffectRow,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TypeKind {
    Primitive(PrimitiveType),
    Struct {
        name: SymbolId,
        fields: Vec<StructField>,
    },
    Enum {
        name: SymbolId,
        variants: Vec<EnumVariant>,
    },
    Function(FunctionType),
    TypeParam(u16),
    Error,
}

#[derive(Clone, Debug, Default)]
pub struct TypeStore {
    kinds: Vec<TypeKind>,
}

impl TypeStore {
    pub fn new() -> Self {
        Self { kinds: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.kinds.len()
    }

    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }

    pub fn intern(&mut self, kind: TypeKind) -> TypeId {
        let id = TypeId::new(self.kinds.len());
        self.kinds.push(kind);
        id
    }

    pub fn get(&self, id: TypeId) -> Option<&TypeKind> {
        self.kinds.get(id.index())
    }

    pub fn kinds(&self) -> &[TypeKind] {
        &self.kinds
    }

    pub fn persistability(&self, id: TypeId) -> Persistability {
        self.persistability_with_depth(id, 0)
    }

    fn persistability_with_depth(&self, id: TypeId, depth: usize) -> Persistability {
        const MAX_DEPTH: usize = 128;
        if depth > MAX_DEPTH {
            return Persistability::NonPersistable;
        }

        let Some(kind) = self.get(id) else {
            return Persistability::NonPersistable;
        };

        match kind {
            TypeKind::Primitive(_) => Persistability::Trivial,
            TypeKind::TypeParam(_) => Persistability::Serializable,
            TypeKind::Error => Persistability::NonPersistable,
            TypeKind::Function(_) => Persistability::NonPersistable,
            TypeKind::Struct { fields, .. } => {
                fold_persistability(fields.iter().map(|field| field.ty), |ty| {
                    self.persistability_with_depth(ty, depth + 1)
                })
            }
            TypeKind::Enum { variants, .. } => fold_persistability(
                variants
                    .iter()
                    .flat_map(|variant| variant.fields.iter().copied()),
                |ty| self.persistability_with_depth(ty, depth + 1),
            ),
        }
    }
}

fn fold_persistability<I, F>(iter: I, mut classify: F) -> Persistability
where
    I: IntoIterator<Item = TypeId>,
    F: FnMut(TypeId) -> Persistability,
{
    let mut any_serializable = false;
    for ty in iter {
        match classify(ty) {
            Persistability::Trivial => {}
            Persistability::Serializable => any_serializable = true,
            Persistability::NonPersistable => return Persistability::NonPersistable,
        }
    }
    if any_serializable {
        Persistability::Serializable
    } else {
        Persistability::Trivial
    }
}
