use std::collections::HashSet;

use crate::analysis::arc_cfg::ArcCfg;
use crate::common::ids::{StmtId, VarId};
use smallvec::SmallVec;

#[derive(Clone, Debug, Default)]
pub struct ArcLastUseTables {
    live_in: Vec<HashSet<VarId>>,
    live_out: Vec<HashSet<VarId>>,
    last_uses: Vec<Option<SmallVec<[VarId; 4]>>>,
}

impl ArcLastUseTables {
    pub fn analyze(cfg: &ArcCfg) -> Self {
        let stmt_capacity = cfg.stmt_capacity();
        let mut tables = ArcLastUseTables {
            live_in: vec![HashSet::new(); stmt_capacity],
            live_out: vec![HashSet::new(); stmt_capacity],
            last_uses: vec![None; stmt_capacity],
        };

        if cfg.reachable().is_empty() {
            return tables;
        }

        let mut changed = true;
        while changed {
            changed = false;
            for stmt_id in cfg.reachable().iter().copied().rev() {
                let Some(summary) = cfg.summary(stmt_id) else {
                    continue;
                };
                let mut out = HashSet::new();
                for succ in &summary.successors {
                    out.extend(tables.live_in[succ.index()].iter().copied());
                }

                let mut next_in = out.clone();
                for def in &summary.defs {
                    next_in.remove(def);
                }
                next_in.extend(summary.uses.iter().copied());

                if out != tables.live_out[stmt_id.index()] {
                    tables.live_out[stmt_id.index()] = out;
                    changed = true;
                }
                if next_in != tables.live_in[stmt_id.index()] {
                    tables.live_in[stmt_id.index()] = next_in;
                    changed = true;
                }
            }
        }

        for stmt_id in cfg.reachable().iter().copied() {
            let Some(summary) = cfg.summary(stmt_id) else {
                continue;
            };
            let live_out = &tables.live_out[stmt_id.index()];
            let mut candidates = SmallVec::<[VarId; 4]>::new();
            for used in &summary.uses {
                if !live_out.contains(used) {
                    push_unique_var(&mut candidates, *used);
                }
            }
            for def in &summary.defs {
                if !live_out.contains(def) {
                    push_unique_var(&mut candidates, *def);
                }
            }
            if !candidates.is_empty() {
                tables.last_uses[stmt_id.index()] = Some(candidates);
            }
        }

        tables
    }

    pub fn live_in(&self, stmt_id: StmtId) -> Option<&HashSet<VarId>> {
        self.live_in.get(stmt_id.index())
    }

    pub fn live_out(&self, stmt_id: StmtId) -> Option<&HashSet<VarId>> {
        self.live_out.get(stmt_id.index())
    }

    pub fn last_uses(&self, stmt_id: StmtId) -> &[VarId] {
        self.last_uses
            .get(stmt_id.index())
            .and_then(Option::as_deref)
            .unwrap_or(&[])
    }
}

fn push_unique_var(out: &mut SmallVec<[VarId; 4]>, var: VarId) {
    if !out.contains(&var) {
        out.push(var);
    }
}
