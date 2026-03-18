use std::marker::PhantomData;

use crate::ids::{ExprId, HandlerId, VarId};

pub trait DenseId: Copy {
    fn from_index(index: usize) -> Self;
    fn index(self) -> usize;
}

impl DenseId for ExprId {
    fn from_index(index: usize) -> Self {
        ExprId::new(index)
    }

    fn index(self) -> usize {
        ExprId::index(self)
    }
}

impl DenseId for VarId {
    fn from_index(index: usize) -> Self {
        VarId::new(index)
    }

    fn index(self) -> usize {
        VarId::index(self)
    }
}

impl DenseId for HandlerId {
    fn from_index(index: usize) -> Self {
        HandlerId::new(index)
    }

    fn index(self) -> usize {
        HandlerId::index(self)
    }
}

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
