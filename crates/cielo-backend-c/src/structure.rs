//! Recovers structured control flow from a function's CFG.
//!
//! The plan *is* the dominator tree. Every reachable block is laid out exactly
//! once, at its immediate dominator's site, because the only recursion here is
//! onto dominator-tree children: a block with a single incoming edge has that
//! edge's source as its immediate dominator, and a block with several is a join
//! whose immediate dominator owns it.
//!
//! A single-predecessor block nests directly into the edge that reaches it, so
//! a chain of them collapses into straight-line C with no label and no `goto`.
//! A join is laid out after the branch its dominator owns; edges into it become
//! `goto` unless they already fall through to it.
//!
//! Back edges keep the `goto`: nothing here recovers a `while`. Core has no loop
//! form — `StmtKind` is a tree threaded by `next` — so recursion is the only
//! cycle a program can express, and that is a call, not a CFG edge. A hand-built
//! cyclic CFG still emits correct code, since a loop header has two predecessors
//! and therefore becomes a labelled join; it just stays unstructured.

use std::collections::{HashMap, HashSet};

use cielo_base::ids::CfgBlockId;
use cielo_ir::cfg::{CfgProgram, CfgTerminator};

pub struct Layout {
    pub root: Region,
    /// Blocks some edge reaches by `goto`. A label emitted for anything else
    /// would trip `-Wunused-label`, which the backend tests compile with.
    pub labels: HashSet<CfgBlockId>,
}

pub struct Region {
    pub block: CfgBlockId,
    /// One per terminator successor, in `CfgTerminator::successors` order.
    pub edges: Vec<Edge>,
    /// Joins this block immediately dominates, laid out after its branch.
    pub joins: Vec<Region>,
}

pub enum Edge {
    /// The target has one incoming edge, so its body nests here.
    Inline(Box<Region>),
    Goto(CfgBlockId),
    /// The target is the next thing laid out, so the edge emits nothing.
    Fallthrough,
}

pub fn plan(program: &CfgProgram, entry: CfgBlockId) -> Layout {
    let mut planner = Planner {
        program,
        doms: Doms::compute(program, entry),
        labels: HashSet::new(),
        open: HashSet::new(),
    };
    let root = planner.region(entry, None);
    Layout {
        root,
        labels: planner.labels,
    }
}

struct Planner<'a> {
    program: &'a CfgProgram,
    doms: Doms,
    labels: HashSet<CfgBlockId>,
    open: HashSet<CfgBlockId>,
}

impl Planner<'_> {
    fn region(&mut self, block: CfgBlockId, fallthrough: Option<CfgBlockId>) -> Region {
        let program = self.program;
        let joins = self.doms.joins_of(block);
        self.open.insert(block);

        let terminator = program.block(block).map(|node| &node.terminator);
        // C falls out of a compound statement to one place, so every arm of an
        // `if` chain can elide a jump to whatever is laid out next. A `switch`
        // case instead falls into the following case, so none of its arms may.
        let arm_fallthrough = match terminator {
            Some(CfgTerminator::Switch { .. }) => None,
            _ => joins.first().copied().or(fallthrough),
        };
        let successors = terminator.map(CfgTerminator::successors).unwrap_or_default();
        let edges = successors
            .into_iter()
            .map(|target| self.edge(target, arm_fallthrough))
            .collect();

        let mut nested = Vec::with_capacity(joins.len());
        for (index, join) in joins.iter().enumerate() {
            let next = joins.get(index + 1).copied().or(fallthrough);
            nested.push(self.region(*join, next));
        }

        self.open.remove(&block);
        Region {
            block,
            edges,
            joins: nested,
        }
    }

    fn edge(&mut self, target: CfgBlockId, fallthrough: Option<CfgBlockId>) -> Edge {
        // `open` cannot fire on a well-formed CFG: a single-predecessor block
        // whose one predecessor it dominates is unreachable. It is the guard
        // that keeps a hand-built cycle from recursing forever.
        if self.doms.edge_count(target) == 1 && !self.open.contains(&target) {
            return Edge::Inline(Box::new(self.region(target, fallthrough)));
        }
        if Some(target) == fallthrough {
            return Edge::Fallthrough;
        }
        self.labels.insert(target);
        Edge::Goto(target)
    }
}

/// Immediate dominators over the reachable subgraph, by Cooper, Harvey and
/// Kennedy's iterative algorithm.
struct Doms {
    /// Incoming *edges*, not distinct predecessors: a `Branch` whose arms share
    /// a target reaches it twice and must not be inlined into either arm.
    incoming: HashMap<CfgBlockId, usize>,
    joins: HashMap<CfgBlockId, Vec<CfgBlockId>>,
}

const UNSET: usize = usize::MAX;

impl Doms {
    fn compute(program: &CfgProgram, entry: CfgBlockId) -> Self {
        let rpo = reverse_postorder(program, entry);
        let order = rpo
            .iter()
            .enumerate()
            .map(|(index, block)| (*block, index))
            .collect::<HashMap<_, _>>();

        let mut preds = vec![Vec::new(); rpo.len()];
        for (index, block) in rpo.iter().enumerate() {
            let Some(node) = program.block(*block) else {
                continue;
            };
            for successor in node.terminator.successors() {
                if let Some(target) = order.get(&successor) {
                    preds[*target].push(index);
                }
            }
        }

        let mut idom = vec![UNSET; rpo.len()];
        if !rpo.is_empty() {
            idom[0] = 0;
        }
        let mut changed = true;
        while changed {
            changed = false;
            for index in 1..rpo.len() {
                let mut candidate = UNSET;
                for pred in &preds[index] {
                    if idom[*pred] == UNSET {
                        continue;
                    }
                    candidate = if candidate == UNSET {
                        *pred
                    } else {
                        intersect(&idom, *pred, candidate)
                    };
                }
                if candidate != UNSET && idom[index] != candidate {
                    idom[index] = candidate;
                    changed = true;
                }
            }
        }

        // Ascending RPO index, so each owner's list comes out in RPO order.
        let mut joins: HashMap<CfgBlockId, Vec<CfgBlockId>> = HashMap::new();
        for index in 1..rpo.len() {
            if preds[index].len() > 1 && idom[index] != UNSET {
                joins.entry(rpo[idom[index]]).or_default().push(rpo[index]);
            }
        }

        Doms {
            incoming: rpo
                .iter()
                .enumerate()
                .map(|(index, block)| (*block, preds[index].len()))
                .collect(),
            joins,
        }
    }

    /// Zero for a block outside the reachable subgraph, which keeps an edge to
    /// a dangling successor on the `goto` path instead of inlining nothing.
    fn edge_count(&self, block: CfgBlockId) -> usize {
        self.incoming.get(&block).copied().unwrap_or(0)
    }

    fn joins_of(&self, block: CfgBlockId) -> Vec<CfgBlockId> {
        self.joins.get(&block).cloned().unwrap_or_default()
    }
}

fn intersect(idom: &[usize], mut left: usize, mut right: usize) -> usize {
    while left != right {
        while left > right {
            left = idom[left];
        }
        while right > left {
            right = idom[right];
        }
    }
    left
}

fn reverse_postorder(program: &CfgProgram, entry: CfgBlockId) -> Vec<CfgBlockId> {
    if program.block(entry).is_none() {
        return Vec::new();
    }
    let mut order = Vec::new();
    let mut seen = HashSet::from([entry]);
    let mut stack = vec![(entry, 0usize)];
    while let Some((block, index)) = stack.pop() {
        let successors = program
            .block(block)
            .map(|node| node.terminator.successors())
            .unwrap_or_default();
        match successors.get(index) {
            Some(successor) => {
                stack.push((block, index + 1));
                if program.block(*successor).is_some() && seen.insert(*successor) {
                    stack.push((*successor, 0));
                }
            }
            None => order.push(block),
        }
    }
    order.reverse();
    order
}
