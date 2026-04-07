#![allow(dead_code)]

use cielo_staging::pipeline::phases::{Reason, Stage};

pub fn stage_has_valid_func_ids(stage: Stage, func_count: usize) -> bool {
    match stage {
        Stage::Ct => true,
        Stage::Rt(reason) => reason_has_valid_func_ids(reason, func_count),
    }
}

pub fn reason_has_valid_func_ids(reason: Reason, func_count: usize) -> bool {
    match reason {
        Reason::Parameter { func, .. } | Reason::CtOnlyWithRuntimeArgs(func) => {
            func.index() < func_count
        }
        _ => true,
    }
}
