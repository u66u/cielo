use crate::pipeline::phases::{BtaClassified, BtaTables, CtPropagated, Reason, Stage};

pub fn run(ct: CtPropagated) -> BtaClassified {
    let mut bta = BtaTables::default();

    for idx in 0..ct.program.exprs().len() {
        let expr_id = crate::common::ids::ExprId::new(idx);
        if ct.ct.ct_cache.contains_key(&expr_id) {
            bta.stage_of_expr.insert(expr_id, Stage::Ct);
        } else {
            bta.stage_of_expr
                .insert(expr_id, Stage::Rt(Reason::UserForcedRuntime));
        }
    }

    ct.into_bta_classified(bta)
}
