pub fn fixpoint<S, Step>(mut state: S, mut step: Step, limit: usize) -> S
where
    S: PartialEq + Clone,
    Step: FnMut(&S) -> S,
{
    assert!(limit > 0, "fixpoint limit must be greater than zero");
    for _ in 0..limit {
        let next = step(&state);
        if next == state {
            return state;
        }
        state = next;
    }
    panic!("fixpoint did not converge in {limit} iterations");
}
