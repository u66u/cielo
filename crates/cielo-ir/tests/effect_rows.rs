use cielo_base::EffectLabelId;
use cielo_ir::effect::SortedEffectRow;

#[test]
fn sorted_effect_row_is_unique_and_ordered() {
    let row = SortedEffectRow::from_slice(&[
        EffectLabelId::from_u32(5),
        EffectLabelId::from_u32(2),
        EffectLabelId::from_u32(5),
    ]);
    assert_eq!(
        row.as_slice(),
        &[EffectLabelId::from_u32(2), EffectLabelId::from_u32(5)]
    );
}

#[test]
fn union_and_subtract_work() {
    let left =
        SortedEffectRow::from_slice(&[EffectLabelId::from_u32(1), EffectLabelId::from_u32(2)]);
    let right =
        SortedEffectRow::from_slice(&[EffectLabelId::from_u32(2), EffectLabelId::from_u32(3)]);

    let union = left.union(&right);
    assert_eq!(
        union.as_slice(),
        &[
            EffectLabelId::from_u32(1),
            EffectLabelId::from_u32(2),
            EffectLabelId::from_u32(3),
        ]
    );

    let diff = union.subtract(&right);
    assert_eq!(diff.as_slice(), &[EffectLabelId::from_u32(1)]);
}
