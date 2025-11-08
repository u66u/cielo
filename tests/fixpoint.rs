use cielo::common::fixpoint::fixpoint;

#[test]
fn converges_to_expected_value() {
    let out = fixpoint(0i32, |v| if *v < 3 { v + 1 } else { *v }, 8);
    assert_eq!(out, 3);
}
