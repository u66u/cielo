todo:
- [ ] think abt poly trees/OrgTr for partial eval
- [ ] think abt optimal pre-gc IR: just ANF or something better?
- [ ] hash consing
- [ ] loops as tail recursion once we have a contify pass
- [ ] abstract interpreter for `Expr` nodes, `Stmt` nodes stay manual for now. No visitors, no AI.
- [ ] does abstract interpretation of effects make sense? Can we define rules for effects? E.g. effects of type A are commutative with effects of type B, but not of type C and use that for cheap analysis?
- [ ] should types for evaluation/interpretation/BTA carry an inherit context? About valid values they can have based on environment they're in. For example for comptime passes regarding word size
