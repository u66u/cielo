use std::collections::{HashMap, HashSet};

use cielo_base::{CfgBlockId, CfgExprId, CfgInstId, CfgValueId, DiagnosticBag, Span};
use cielo_ir::cfg::{CfgExpr, CfgInstruction, CfgProgram, CfgTerminator};
use cielo_ir::runtime::RuntimeSourceMap;
use cielo_ir::walk::{Walk, walk_exprs_from};

use crate::refcount::analysis::managed::is_managed;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum BorrowHazardKind {
    AliasFanout,
    Projection,
    CallEscape,
}

impl BorrowHazardKind {
    fn diagnostic_code(self) -> &'static str {
        match self {
            Self::AliasFanout => "BORROW_HAZARD_ALIAS_FANOUT",
            Self::Projection => "BORROW_HAZARD_PROJECTION",
            Self::CallEscape => "BORROW_HAZARD_CALL_ESCAPE",
        }
    }

    fn diagnostic_message(self) -> &'static str {
        match self {
            Self::AliasFanout => {
                "managed alias fanout may conflict with future borrow/cursor exclusivity"
            }
            Self::Projection => {
                "managed match projection introduces borrow/cursor mutation hazard potential"
            }
            Self::CallEscape => {
                "managed value escapes through call boundary with potential borrow hazard"
            }
        }
    }

    fn repro_tag(self) -> &'static str {
        match self {
            Self::AliasFanout => "alias-fanout",
            Self::Projection => "projection",
            Self::CallEscape => "call-escape",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BorrowHazardSite {
    Instruction(CfgInstId),
    Terminator(CfgBlockId),
}

impl BorrowHazardSite {
    fn sort_key(self) -> (u8, usize) {
        match self {
            Self::Instruction(id) => (0, id.index()),
            Self::Terminator(id) => (1, id.index()),
        }
    }

    fn label(self) -> String {
        match self {
            Self::Instruction(id) => format!("i{}", id.as_u32()),
            Self::Terminator(id) => format!("b{}", id.as_u32()),
        }
    }

    fn span(self, sources: &RuntimeSourceMap) -> Span {
        match self {
            Self::Instruction(id) => sources.instruction_span(id),
            Self::Terminator(id) => sources.block_span(id),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BorrowHazardHotspot {
    pub kind: BorrowHazardKind,
    pub site: BorrowHazardSite,
}

impl BorrowHazardHotspot {
    pub fn repro_key(self) -> String {
        format!("{}-{}", self.kind.repro_tag(), self.site.label())
    }
}

#[derive(Clone, Debug, Default)]
pub struct BorrowHazardReport {
    pub alias_fanout_count: u32,
    pub projection_count: u32,
    pub call_escape_count: u32,
    pub alias_fanout_sites: Vec<CfgInstId>,
    pub projection_sites: Vec<CfgBlockId>,
    pub call_escape_sites: Vec<CfgBlockId>,
    pub hotspots: Vec<BorrowHazardHotspot>,
}

pub fn analyze(cfg: &CfgProgram, managed: &[bool]) -> BorrowHazardReport {
    let use_counts = count_value_uses(cfg);
    let mut report = BorrowHazardReport::default();

    for block in cfg.blocks() {
        for instruction_id in &block.instructions {
            let Some(instruction) = cfg.instruction(*instruction_id) else {
                continue;
            };
            if let CfgInstruction::Let { result, value } = instruction.kind
                && let Some(source) = direct_value(cfg, value)
                && is_managed(managed, source)
                && is_managed(managed, result)
                && use_counts.get(&source).copied().unwrap_or(0) > 1
            {
                push_unique(&mut report.alias_fanout_sites, *instruction_id);
            }
        }

        match &block.terminator {
            CfgTerminator::Match {
                scrutinee, arms, ..
            } => {
                if direct_value(cfg, *scrutinee).is_some_and(|value| is_managed(managed, value))
                    && arms
                        .iter()
                        .flat_map(|arm| arm.binders.iter())
                        .copied()
                        .any(|binder| is_managed(managed, binder))
                {
                    push_unique(&mut report.projection_sites, block.id);
                }
            }
            CfgTerminator::Call { args, .. } | CfgTerminator::Perform { args, .. } => {
                let mut seen = HashSet::new();
                let escapes_shared_value = args.iter().copied().any(|argument| {
                    managed_values_in_expr(cfg, argument, managed, &mut seen)
                        .iter()
                        .any(|value| use_counts.get(value).copied().unwrap_or(0) > 1)
                });
                if escapes_shared_value {
                    push_unique(&mut report.call_escape_sites, block.id);
                }
            }
            _ => {}
        }
    }

    report.alias_fanout_sites.sort_by_key(|id| id.index());
    report.projection_sites.sort_by_key(|id| id.index());
    report.call_escape_sites.sort_by_key(|id| id.index());
    report.alias_fanout_count = report.alias_fanout_sites.len() as u32;
    report.projection_count = report.projection_sites.len() as u32;
    report.call_escape_count = report.call_escape_sites.len() as u32;
    report.hotspots = collect_hotspots(&report);
    report
}

fn count_value_uses(cfg: &CfgProgram) -> HashMap<CfgValueId, u32> {
    let mut counts = HashMap::new();
    for block in cfg.blocks() {
        for instruction in &block.instructions {
            let Some(instruction) = cfg.instruction(*instruction) else {
                continue;
            };
            let mut seen = HashSet::new();
            match instruction.kind {
                CfgInstruction::Let { value, .. } | CfgInstruction::Eval { value, .. } => {
                    count_expr_uses(cfg, value, &mut seen, &mut counts)
                }
                _ => {}
            }
        }
        let mut seen = HashSet::new();
        count_terminator_uses(cfg, &block.terminator, &mut seen, &mut counts);
    }
    counts
}

fn count_terminator_uses(
    cfg: &CfgProgram,
    terminator: &CfgTerminator,
    seen: &mut HashSet<CfgExprId>,
    counts: &mut HashMap<CfgValueId, u32>,
) {
    for operand in terminator.child_exprs() {
        count_expr_uses(cfg, operand, seen, counts);
    }
}

/// Counts distinct expression nodes, not occurrences: hazard reporting asks
/// "does more than one place read this value", so a sub-expression shared by two
/// parents must not answer that on its own. `cfg_arc` deliberately counts the
/// other way.
fn count_expr_uses(
    cfg: &CfgProgram,
    expression: CfgExprId,
    seen: &mut HashSet<CfgExprId>,
    counts: &mut HashMap<CfgValueId, u32>,
) {
    walk_exprs_from(cfg, expression, seen, &mut |_, node| {
        if let CfgExpr::Value(value) = &node.kind {
            let count = counts.entry(*value).or_default();
            *count = (*count).saturating_add(1);
        }
        Walk::Descend
    });
}

fn managed_values_in_expr(
    cfg: &CfgProgram,
    expression: CfgExprId,
    managed: &[bool],
    seen: &mut HashSet<CfgExprId>,
) -> Vec<CfgValueId> {
    let mut values = Vec::new();
    walk_exprs_from(cfg, expression, seen, &mut |_, node| {
        if let CfgExpr::Value(value) = &node.kind
            && is_managed(managed, *value)
        {
            push_unique(&mut values, *value);
        }
        Walk::Descend
    });
    values
}

pub fn emit_diagnostics(
    sources: &RuntimeSourceMap,
    report: &BorrowHazardReport,
    diagnostics: &mut DiagnosticBag,
) {
    for hotspot in &report.hotspots {
        diagnostics.warning(
            hotspot.kind.diagnostic_code(),
            format!(
                "{} (site {}, repro={})",
                hotspot.kind.diagnostic_message(),
                hotspot.site.label(),
                hotspot.repro_key()
            ),
            hotspot.site.span(sources),
        );
    }

    if !report.hotspots.is_empty() {
        let repro_keys = report
            .hotspots
            .iter()
            .take(8)
            .map(|hotspot| hotspot.repro_key())
            .collect::<Vec<_>>()
            .join(", ");
        diagnostics.note(
            "BORROW_HAZARD_SUMMARY",
            format!(
                "borrow hazard groundwork flagged {} site(s): alias_fanout={}, projection={}, call_escape={}, repro_keys=[{}]",
                report.hotspots.len(),
                report.alias_fanout_count,
                report.projection_count,
                report.call_escape_count,
                repro_keys
            ),
            Span::synthetic(),
        );
    }
}

fn collect_hotspots(report: &BorrowHazardReport) -> Vec<BorrowHazardHotspot> {
    let mut hotspots = Vec::new();
    hotspots.extend(
        report
            .alias_fanout_sites
            .iter()
            .map(|site| BorrowHazardHotspot {
                kind: BorrowHazardKind::AliasFanout,
                site: BorrowHazardSite::Instruction(*site),
            }),
    );
    hotspots.extend(
        report
            .projection_sites
            .iter()
            .map(|site| BorrowHazardHotspot {
                kind: BorrowHazardKind::Projection,
                site: BorrowHazardSite::Terminator(*site),
            }),
    );
    hotspots.extend(
        report
            .call_escape_sites
            .iter()
            .map(|site| BorrowHazardHotspot {
                kind: BorrowHazardKind::CallEscape,
                site: BorrowHazardSite::Terminator(*site),
            }),
    );
    hotspots.sort_by_key(|hotspot| (hotspot.site.sort_key(), hotspot.kind));
    hotspots.dedup();
    hotspots
}

fn direct_value(cfg: &CfgProgram, expression: CfgExprId) -> Option<CfgValueId> {
    match cfg.expr(expression).map(|expression| &expression.kind) {
        Some(CfgExpr::Value(value)) => Some(*value),
        _ => None,
    }
}

fn push_unique<T: Copy + PartialEq>(values: &mut Vec<T>, value: T) {
    if !values.contains(&value) {
        values.push(value);
    }
}
