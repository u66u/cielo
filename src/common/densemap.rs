use std::marker::PhantomData;

use crate::common::ids::{ExprId, HandlerId, VarId};

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

#[cfg(test)]
mod tests {
    use super::DenseMap;
    use crate::common::ids::{ExprId, VarId};

    #[test]
    fn densemap_supports_sparse_id_insert_and_lookup() {
        let mut map = DenseMap::<ExprId, i32>::default();
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);

        let _ = map.insert(ExprId::new(5), 10);
        let _ = map.insert(ExprId::new(1), 20);

        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&ExprId::new(5)), Some(&10));
        assert_eq!(map.get(&ExprId::new(1)), Some(&20));
        assert_eq!(map.get(&ExprId::new(3)), None);
        assert!(map.contains_key(&ExprId::new(5)));
        assert!(!map.contains_key(&ExprId::new(3)));
    }

    #[test]
    fn densemap_iterates_ids_in_dense_order() {
        let mut map = DenseMap::<VarId, &'static str>::default();
        let _ = map.insert(VarId::new(3), "c");
        let _ = map.insert(VarId::new(1), "a");
        let _ = map.insert(VarId::new(2), "b");

        let ids = map.iter().map(|(id, _)| id.index()).collect::<Vec<_>>();
        let values = map.values().copied().collect::<Vec<_>>();
        assert_eq!(ids, vec![1, 2, 3]);
        assert_eq!(values, vec!["a", "b", "c"]);
    }
}
