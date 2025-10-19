use cielo::common::symbols::Interner;

#[test]
fn interning_is_stable() {
    let mut interner = Interner::new();
    let a = interner.intern("hello");
    let b = interner.intern("hello");
    let c = interner.intern("world");

    assert_eq!(a, b);
    assert_ne!(a, c);
    assert_eq!(interner.resolve(a), Some("hello"));
}
