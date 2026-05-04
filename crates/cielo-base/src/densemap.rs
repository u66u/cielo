use std::marker::PhantomData;

use crate::ids::{CfgExprId, ExprId, HandlerId, LinearExprId, VarId};

pub trait DenseId: Copy {
    fn from_index(index: usize) -> Self;
    fn index(self) -> usize;
}

macro_rules! impl_dense_id {
    ($name:ident) => {
        impl DenseId for $name {
            fn from_index(index: usize) -> Self {
                $name::new(index)
            }

            fn index(self) -> usize {
                $name::index(self)
            }
        }
    };
}

impl_dense_id!(ExprId);
impl_dense_id!(VarId);
impl_dense_id!(HandlerId);
impl_dense_id!(LinearExprId);
impl_dense_id!(CfgExprId);

#[derive(Clone, Debug)]
pub struct DenseMap<I: DenseId, V> {
    slots: Vec<Option<V>>,
    len: usize,
    marker: PhantomData<I>,
}

impl<I: DenseId, V> Default for DenseMap<I, V> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            len: 0,
            marker: PhantomData,
        }
    }
}

impl<I: DenseId, V> DenseMap<I, V> {
    pub fn insert(&mut self, id: I, value: V) -> Option<V> {
        let index = id.index();
        if self.slots.len() <= index {
            self.slots.resize_with(index + 1, || None);
        }
        let old = self.slots[index].replace(value);
        if old.is_none() {
            self.len += 1;
        }
        old
    }

    pub fn get(&self, id: &I) -> Option<&V> {
        self.slots.get(id.index()).and_then(Option::as_ref)
    }

    pub fn get_mut(&mut self, id: &I) -> Option<&mut V> {
        self.slots.get_mut(id.index()).and_then(Option::as_mut)
    }

    pub fn contains_key(&self, id: &I) -> bool {
        self.get(id).is_some()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.slots.iter().flatten()
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.slots.iter_mut().flatten()
    }

    pub fn iter(&self) -> impl Iterator<Item = (I, &V)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(idx, value)| value.as_ref().map(|value| (I::from_index(idx), value)))
    }

    pub fn keys(&self) -> impl Iterator<Item = I> {
        self.iter().map(|(id, _)| id)
    }
}

impl<I: DenseId, V> FromIterator<(I, V)> for DenseMap<I, V> {
    fn from_iter<T: IntoIterator<Item = (I, V)>>(iter: T) -> Self {
        let mut out = Self::default();
        for (id, value) in iter {
            let _ = out.insert(id, value);
        }
        out
    }
}
