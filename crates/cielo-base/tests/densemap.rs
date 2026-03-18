use cielo_base::densemap::DenseMap;
use cielo_base::{ExprId, VarId};

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
