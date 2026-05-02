//! Backward liveness and last-use analysis over block-form CFG.

use std::collections::{BTreeSet, HashMap};

use cielo_base::ids::{CfgBlockId, CfgExprId, CfgInstId, CfgValueId};
use cielo_ir::cfg::{CfgExpr, CfgInstruction, CfgProgram, CfgTerminator};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum CfgUseSite {
    Instruction(CfgInstId),
    Terminator(CfgBlockId),
}

#[derive(Clone, Debug, Default)]
pub struct CfgLiveness {
    live_in: Vec<BTreeSet<CfgValueId>>,
    live_out: Vec<BTreeSet<CfgValueId>>,
    live_after: HashMap<CfgUseSite, BTreeSet<CfgValueId>>,
    live_after_entry: Vec<BTreeSet<CfgValueId>>,
    last_uses: HashMap<CfgUseSite, BTreeSet<CfgValueId>>,
}

impl CfgLiveness {
    pub fn analyze(program: &CfgProgram) -> Self {
        let block_count = program.blocks().len();
        let mut uses = vec![BTreeSet::new(); block_count];
        let mut defs = vec![BTreeSet::new(); block_count];

        for block in program.blocks() {
            let block_defs = &mut defs[block.id.index()];
            block_defs.extend(block.params.iter().copied());

            for instruction_id in &block.instructions {
                let Some(instruction) = program.instruction(*instruction_id) else {
                    continue;
                };
                let mut instruction_uses = BTreeSet::new();
                collect_instruction_uses(program, &instruction.kind, &mut instruction_uses);
                for value in instruction_uses {
                    if !block_defs.contains(&value) {
                        uses[block.id.index()].insert(value);
                    }
                }
                collect_instruction_defs(&instruction.kind, block_defs);
            }

            let mut terminator_uses = BTreeSet::new();
            collect_terminator_uses(program, &block.terminator, &mut terminator_uses);
            for value in terminator_uses {
                if !block_defs.contains(&value) {
                    uses[block.id.index()].insert(value);
                }
            }
        }

        let mut live_in = vec![BTreeSet::new(); block_count];
        let mut live_out = vec![BTreeSet::new(); block_count];
        let mut changed = true;
        while changed {
            changed = false;
            for block in program.blocks().iter().rev() {
                let mut new_out = BTreeSet::new();
                for successor in block.terminator.successors() {
                    if let Some(successor_live_in) = live_in.get(successor.index()) {
                        new_out.extend(successor_live_in.iter().copied());
                    }
                }

                let mut new_in = uses[block.id.index()].clone();
                new_in.extend(
                    new_out
                        .iter()
                        .filter(|value| !defs[block.id.index()].contains(value))
                        .copied(),
                );

                if new_out != live_out[block.id.index()] {
                    live_out[block.id.index()] = new_out;
                    changed = true;
                }
                if new_in != live_in[block.id.index()] {
                    live_in[block.id.index()] = new_in;
                    changed = true;
                }
            }
        }

        let mut result = Self {
            live_in,
            live_out,
            live_after: HashMap::new(),
            live_after_entry: vec![BTreeSet::new(); block_count],
            last_uses: HashMap::new(),
        };
        result.find_last_uses(program);
        result
    }

    pub fn live_in(&self, block: CfgBlockId) -> Option<&BTreeSet<CfgValueId>> {
        self.live_in.get(block.index())
    }

    pub fn live_out(&self, block: CfgBlockId) -> Option<&BTreeSet<CfgValueId>> {
        self.live_out.get(block.index())
    }

    pub fn last_uses_at(&self, site: CfgUseSite) -> Option<&BTreeSet<CfgValueId>> {
        self.last_uses.get(&site)
    }

    pub fn live_after(&self, site: CfgUseSite) -> Option<&BTreeSet<CfgValueId>> {
        self.live_after.get(&site)
    }

    /// Values needed after block parameters have been defined.
    pub fn live_after_entry(&self, block: CfgBlockId) -> Option<&BTreeSet<CfgValueId>> {
        self.live_after_entry.get(block.index())
    }

    pub fn is_last_use(&self, site: CfgUseSite, value: CfgValueId) -> bool {
        self.last_uses
            .get(&site)
            .is_some_and(|values| values.contains(&value))
    }

    fn find_last_uses(&mut self, program: &CfgProgram) {
        for block in program.blocks() {
            let mut live = self.live_out[block.id.index()].clone();
            self.live_after
                .insert(CfgUseSite::Terminator(block.id), live.clone());
            let mut terminator_uses = BTreeSet::new();
            collect_terminator_uses(program, &block.terminator, &mut terminator_uses);
            record_last_uses(
                &mut self.last_uses,
                CfgUseSite::Terminator(block.id),
                &mut live,
                terminator_uses,
            );

            for instruction_id in block.instructions.iter().rev() {
                let Some(instruction) = program.instruction(*instruction_id) else {
                    continue;
                };
                let mut instruction_defs = BTreeSet::new();
                collect_instruction_defs(&instruction.kind, &mut instruction_defs);
                self.live_after
                    .insert(CfgUseSite::Instruction(*instruction_id), live.clone());
                for value in instruction_defs {
                    live.remove(&value);
                }

                let mut instruction_uses = BTreeSet::new();
                collect_instruction_uses(program, &instruction.kind, &mut instruction_uses);
                record_last_uses(
                    &mut self.last_uses,
                    CfgUseSite::Instruction(*instruction_id),
                    &mut live,
                    instruction_uses,
                );
            }

            self.live_after_entry[block.id.index()] = live.clone();
            for param in &block.params {
                live.remove(param);
            }
        }
    }
}

fn record_last_uses(
    last_uses: &mut HashMap<CfgUseSite, BTreeSet<CfgValueId>>,
    site: CfgUseSite,
    live: &mut BTreeSet<CfgValueId>,
    uses: BTreeSet<CfgValueId>,
) {
    for value in uses {
        if !live.contains(&value) {
            last_uses.entry(site).or_default().insert(value);
        }
        live.insert(value);
    }
}

fn collect_instruction_defs(instruction: &CfgInstruction, output: &mut BTreeSet<CfgValueId>) {
    match instruction {
        CfgInstruction::Let { result, .. } | CfgInstruction::Eval { result, .. } => {
            output.insert(*result);
        }
        _ => {}
    }
}

fn collect_instruction_uses(
    program: &CfgProgram,
    instruction: &CfgInstruction,
    output: &mut BTreeSet<CfgValueId>,
) {
    match instruction {
        CfgInstruction::Let { value, .. } | CfgInstruction::Eval { value, .. } => {
            collect_expr_uses(program, *value, output);
        }
        _ => {}
    }
}

fn collect_terminator_uses(
    program: &CfgProgram,
    terminator: &CfgTerminator,
    output: &mut BTreeSet<CfgValueId>,
) {
    match terminator {
        CfgTerminator::Return(value) => collect_expr_uses(program, *value, output),
        CfgTerminator::Goto { args, .. }
        | CfgTerminator::Call { args, .. }
        | CfgTerminator::Perform { args, .. } => {
            for arg in args {
                collect_expr_uses(program, *arg, output);
            }
        }
        CfgTerminator::Branch { cond, .. } => collect_expr_uses(program, *cond, output),
        CfgTerminator::Match { scrutinee, .. } => collect_expr_uses(program, *scrutinee, output),
        CfgTerminator::Switch { selector, .. } => collect_expr_uses(program, *selector, output),
        CfgTerminator::Unreachable => {}
    }
}

fn collect_expr_uses(
    program: &CfgProgram,
    expression: CfgExprId,
    output: &mut BTreeSet<CfgValueId>,
) {
    let Some(expression) = program.expr(expression) else {
        return;
    };
    match &expression.kind {
        CfgExpr::Value(value) => {
            output.insert(*value);
        }
        CfgExpr::Unary { expr, .. } | CfgExpr::Field { base: expr, .. } => {
            collect_expr_uses(program, *expr, output)
        }
        CfgExpr::Binary { lhs, rhs, .. } => {
            collect_expr_uses(program, *lhs, output);
            collect_expr_uses(program, *rhs, output);
        }
        CfgExpr::PureCall { args, .. }
        | CfgExpr::MakeStruct { fields: args, .. }
        | CfgExpr::MakeEnum { fields: args, .. } => {
            for arg in args {
                collect_expr_uses(program, *arg, output);
            }
        }
        CfgExpr::Literal(_) | CfgExpr::Error => {}
    }
}
