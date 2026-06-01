//! Collapses a cycle of mutually tail-recursive functions into one function.
//!
//! CIELO-60 got a self-recursive tail call emitted as `return f(...)` and left
//! the rest to the C compiler's sibling-call transform. That works for a single
//! function and stops working the moment the cycle has two members: GCC inlines
//! one member into the other, the copy's exits merge, the surviving call falls
//! out of tail position, and a million-deep alternation runs off the stack
//! (CIELO-62). Nothing about the emitted C is wrong there -- both calls really
//! are in tail position -- so the fix is to stop depending on the heuristic.
//!
//! The members of a cycle become blocks of one dispatch function. A tail call
//! from one member to another is then an edge inside that function, so it
//! lowers to `goto` and the frame is reused by construction rather than by a
//! C compiler's choice. Each original function survives as a wrapper that
//! enters the dispatcher at its own state, which keeps every signature the rest
//! of the program calls through exactly as it was.
//!
//! This runs on the CFG rather than on the emitted text because the CFG already
//! has all three pieces: `structure::plan` turns a cyclic CFG into labels and
//! `goto`s, `Switch` gives the entry dispatch, and edge arguments are already
//! sequenced through temporaries -- which is the one real hazard of a
//! trampoline, since `f(g(x), x)` must not read `x` back after assigning it.

use std::collections::{HashMap, HashSet};

use cielo_base::ids::{CfgBlockId, CfgExprId, CfgFuncId};
use cielo_ir::cfg::{
    CfgCallConvention, CfgExpr, CfgFunction, CfgInstruction, CfgProgram, CfgTerminator,
};
use cielo_ir::core::Literal;

use crate::cfg_codegen::{reachable_blocks, tail_call_blocks};

pub struct Collapsed {
    pub program: CfgProgram,
    /// The synthesized functions, which take a name of their own rather than
    /// the first member's: `cielo_fn_is_even_3` sitting next to the wrapper
    /// `cielo_fn_is_even_0` is the kind of thing that costs an hour when
    /// reading emitted C, which is how this class of bug gets found at all.
    pub dispatchers: HashSet<CfgFuncId>,
}

/// The transformed program, or `None` when no cycle qualifies.
///
/// Returning an owned program keeps the cost off the common case: most programs
/// have no mutually recursive tail calls at all and never pay for the clone.
pub fn collapse_tail_call_cycles(program: &CfgProgram) -> Option<Collapsed> {
    let tails = tail_call_blocks(program);
    let cycles = tail_call_cycles(program, &tails);
    let eligible = cycles
        .into_iter()
        .filter(|members| is_eligible(program, members, &tails))
        .collect::<Vec<_>>();
    if eligible.is_empty() {
        return None;
    }
    let mut program = program.clone();
    let dispatchers = eligible
        .into_iter()
        .map(|members| collapse(&mut program, &members, &tails))
        .collect();
    Some(Collapsed {
        program,
        dispatchers,
    })
}

/// The call a tail block makes: who it calls and with what.
///
/// Two terminator shapes reach C as `return f(...)`. A `Call` whose
/// continuation does nothing is the direct one; a `Goto` carrying a single
/// `PureCall` edge argument is the same thing left in expression position by
/// lowering.
fn tail_target(program: &CfgProgram, block: CfgBlockId) -> Option<(CfgFuncId, Vec<CfgExprId>)> {
    match &program.block(block)?.terminator {
        CfgTerminator::Call {
            callee_fn, args, ..
        } => Some((*callee_fn, args.clone())),
        CfgTerminator::Goto { args, .. } if args.len() == 1 => match &program.expr(args[0])?.kind {
            CfgExpr::PureCall {
                callee_fn, args, ..
            } => Some((*callee_fn, args.clone())),
            _ => None,
        },
        _ => None,
    }
}

/// Strongly connected components of the graph whose edges are tail calls only.
///
/// A non-tail call cannot become a `goto` -- work is left to run once it comes
/// back -- so a cycle that closes through one is not a cycle this can collapse,
/// and including it would only make the dispatcher bigger for nothing.
fn tail_call_cycles(program: &CfgProgram, tails: &HashSet<CfgBlockId>) -> Vec<Vec<CfgFuncId>> {
    let edges = program
        .functions
        .iter()
        .map(|function| {
            let callees = reachable_blocks(program, function.entry, tails)
                .into_iter()
                .filter(|block| tails.contains(block))
                .filter_map(|block| tail_target(program, block).map(|(callee, _)| callee))
                .collect::<HashSet<_>>();
            (function.id, callees)
        })
        .collect::<HashMap<_, _>>();

    let reach = edges
        .keys()
        .map(|from| (*from, transitive(&edges, *from)))
        .collect::<HashMap<_, _>>();

    let mut cycles = Vec::new();
    let mut assigned = HashSet::new();
    for function in &program.functions {
        if !assigned.insert(function.id) {
            continue;
        }
        let mut members = vec![function.id];
        for other in &program.functions {
            if other.id == function.id {
                continue;
            }
            if reach[&function.id].contains(&other.id) && reach[&other.id].contains(&function.id) {
                assigned.insert(other.id);
                members.push(other.id);
            }
        }
        if members.len() > 1 {
            cycles.push(members);
        }
    }
    cycles
}

fn transitive(
    edges: &HashMap<CfgFuncId, HashSet<CfgFuncId>>,
    from: CfgFuncId,
) -> HashSet<CfgFuncId> {
    let mut seen = HashSet::new();
    let mut stack = edges[&from].iter().copied().collect::<Vec<_>>();
    while let Some(next) = stack.pop() {
        if seen.insert(next)
            && let Some(callees) = edges.get(&next)
        {
            stack.extend(callees.iter().copied());
        }
    }
    seen
}

/// What the dispatcher's shared frame must not have to reason about.
///
/// Every recursion step used to get its own C frame; merging them into one loop
/// gives them one. Handler evidence, region headers and a captured continuation
/// all key off that frame, so a cycle whose members touch any of them is left
/// on the old path rather than being quietly given different semantics. Plain
/// arithmetic recursion -- the case that overflows -- touches none of it.
fn is_eligible(program: &CfgProgram, members: &[CfgFuncId], tails: &HashSet<CfgBlockId>) -> bool {
    let mut owner = HashMap::new();
    let mut params = HashSet::new();
    for id in members {
        // `collapse` reaches a member by indexing, and appends the dispatcher at
        // `functions.len()`, so both depend on a function's id being its index.
        let Some(function) = program.functions.get(id.index()).filter(|f| f.id == *id) else {
            return false;
        };
        // The dispatcher declares the union of the members' parameters and
        // relies on each member reading only its own, so the value namespaces
        // have to be disjoint and the entry block has to be the function's.
        if program
            .block(function.entry)
            .map(|block| block.params.as_slice())
            != Some(function.params.as_slice())
        {
            return false;
        }
        if !function.params.iter().all(|param| params.insert(*param)) {
            return false;
        }
        for block_id in reachable_blocks(program, function.entry, tails) {
            if owner
                .insert(block_id, *id)
                .is_some_and(|first| first != *id)
            {
                return false;
            }
            let block = program.block(block_id).expect("known block");
            if matches!(block.terminator, CfgTerminator::Perform { .. }) {
                return false;
            }
            for instruction in &block.instructions {
                let Some(node) = program.instruction(*instruction) else {
                    continue;
                };
                if matches!(
                    node.kind,
                    CfgInstruction::HandlerEnter { .. } | CfgInstruction::RegionEnter { .. }
                ) {
                    return false;
                }
            }
        }
    }
    true
}

/// Returns the dispatcher the cycle became.
fn collapse(
    program: &mut CfgProgram,
    members: &[CfgFuncId],
    tails: &HashSet<CfgBlockId>,
) -> CfgFuncId {
    let entries = members
        .iter()
        .map(|id| program.functions[id.index()].entry)
        .collect::<Vec<_>>();
    let slots = members
        .iter()
        .map(|id| program.functions[id.index()].params.clone())
        .collect::<Vec<_>>();

    // One parameter pack, built as the concatenation of the members' own
    // parameters rather than a max-arity array. Every value id in a CFG is
    // unique program-wide, so the union needs no renaming: a member's body
    // still reads `v7` and `v7` is still its own slot. A shared array would
    // need every body rewritten to index it, and would have to answer what
    // happens when two members disagree on what slot 0 holds. The pack is as
    // wide as the sum of the arities, which is fine for the handful of
    // functions a real cycle has.
    let state = program.push_value(None);
    let mut params = vec![state];
    for slot in &slots {
        params.extend(slot.iter().copied());
    }

    let dispatch_entry = program.push_block(params.clone(), None);
    let selector = program.push_expr(CfgExpr::Value(state), None);
    // Only the wrappers below call the dispatcher, and each passes its own
    // index, so no other state can arrive.
    let unreachable = program.push_block(Vec::new(), None);
    program.set_terminator(unreachable, CfgTerminator::Unreachable);
    program.set_terminator(
        dispatch_entry,
        CfgTerminator::Switch {
            selector,
            targets: entries.clone(),
            default: unreachable,
        },
    );

    let dispatch = CfgFuncId::new(program.functions.len());
    let name = program.functions[members[0].index()].name;
    program.functions.push(CfgFunction {
        id: dispatch,
        name,
        params,
        entry: dispatch_entry,
    });

    let index_of = members
        .iter()
        .enumerate()
        .map(|(index, id)| (*id, index))
        .collect::<HashMap<_, _>>();

    // Every intra-cycle tail call becomes an edge to the callee's entry block,
    // which assigns that member's parameters and jumps. Both terminator shapes
    // collapse to the same `Goto`; the call's continuation is left behind and
    // simply stops being reachable.
    let rewrites = entries
        .iter()
        .flat_map(|entry| reachable_blocks(program, *entry, tails))
        .filter(|block| tails.contains(block))
        .filter_map(|block| {
            let (callee, args) = tail_target(program, block)?;
            Some((block, entries[*index_of.get(&callee)?], args))
        })
        .collect::<Vec<_>>();
    for (block, target, args) in rewrites {
        program.set_terminator(block, CfgTerminator::Goto { target, args });
    }

    for (index, id) in members.iter().enumerate() {
        let result = program.push_value(None);
        let returns = program.push_block(vec![result], None);
        let returned = program.push_expr(CfgExpr::Value(result), None);
        program.set_terminator(returns, CfgTerminator::Return(returned));

        let mut args = vec![program.push_expr(CfgExpr::Literal(Literal::Int(index as i64)), None)];
        for (slot, values) in slots.iter().enumerate() {
            for value in values {
                let kind = if slot == index {
                    CfgExpr::Value(*value)
                } else {
                    CfgExpr::Literal(Literal::Unit)
                };
                args.push(program.push_expr(kind, None));
            }
        }

        let wrapper = program.push_block(slots[index].clone(), None);
        program.set_terminator(
            wrapper,
            CfgTerminator::Call {
                convention: CfgCallConvention::Pure,
                callee: name,
                callee_fn: dispatch,
                args,
                result,
                target: returns,
            },
        );
        program.functions[id.index()].entry = wrapper;
    }
    dispatch
}
