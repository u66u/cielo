use std::collections::HashSet;

use crate::analysis::arc_cfg::ArcCfg;
use crate::common::densemap::DenseMap;
use crate::common::ids::{ExprId, StmtId, VarId};
use crate::ir::core::{CoreProgram, ExprKind, StmtKind};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ArcAliasClass {
    #[default]
    Unique,
    Shared,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArcAliasRelation {
    Same,
    MayAlias,
    Disjoint,
}

#[derive(Clone, Debug, Default)]
pub struct ArcAliasTables {
    pub class_of_var: DenseMap<VarId, ArcAliasClass>,
    pub copy_alias_edges: Vec<(VarId, VarId)>,
}

impl ArcAliasTables {
    pub fn analyze(program: &CoreProgram, cfg: &ArcCfg) -> Self {
        let mut tables = ArcAliasTables::default();
        for stmt_id in cfg.reachable().iter().copied() {
            let Some(summary) = cfg.summary(stmt_id) else {
                continue;
            };
            for var in summary.uses.iter().chain(summary.defs.iter()).copied() {
                tables.class_of_var.insert(
                    var,
                    tables.class_of_var(var).unwrap_or(ArcAliasClass::Unique),
                );
            }
        }

        for stmt_id in cfg.reachable().iter().copied() {
            classify_stmt_alias(program, stmt_id, &mut tables);
        }

        tables.copy_alias_edges.sort_unstable_by_key(|(lhs, rhs)| {
            (lhs.index().min(rhs.index()), lhs.index().max(rhs.index()))
        });
        tables.copy_alias_edges.dedup();
        tables
    }

    pub fn class_of_var(&self, var: VarId) -> Option<ArcAliasClass> {
        self.class_of_var.get(&var).copied()
    }

    pub fn relation(&self, lhs: VarId, rhs: VarId) -> ArcAliasRelation {
        if lhs == rhs {
            return ArcAliasRelation::Same;
        }
        let lhs_class = self.class_of_var(lhs).unwrap_or(ArcAliasClass::Unique);
        let rhs_class = self.class_of_var(rhs).unwrap_or(ArcAliasClass::Unique);
        if lhs_class == ArcAliasClass::Shared && rhs_class == ArcAliasClass::Shared {
            ArcAliasRelation::MayAlias
        } else {
            ArcAliasRelation::Disjoint
        }
    }
}

fn classify_stmt_alias(program: &CoreProgram, stmt_id: StmtId, tables: &mut ArcAliasTables) {
    let Some(stmt) = program.stmt(stmt_id) else {
        return;
    };
    match &stmt.kind {
        StmtKind::Let { binding, value, .. } => {
            if let Some(expr) = program.expr(*value)
                && let ExprKind::Var(source) = expr.kind
            {
                mark_shared(*binding, tables);
                mark_shared(source, tables);
                tables.copy_alias_edges.push((*binding, source));
            }
        }
        StmtKind::Call { .. } | StmtKind::Perform { .. } => {}
        StmtKind::Resume { arg, resume, .. } => {
            mark_shared(*resume, tables);
            mark_expr_vars_shared(program, *arg, tables);
        }
        StmtKind::Match { arms, .. } => {
            for arm in arms {
                for binder in arm.binders.iter().copied() {
                    mark_shared(binder, tables);
                }
            }
        }
        StmtKind::Return(_) | StmtKind::If { .. } => {}
        StmtKind::Val { .. }
        | StmtKind::Handle { .. }
        | StmtKind::Stage { .. }
        | StmtKind::Hole { .. }
        | StmtKind::Error(_) => {}
    }
}

fn mark_expr_vars_shared(program: &CoreProgram, expr_id: ExprId, tables: &mut ArcAliasTables) {
    let mut stack = vec![expr_id];
    let mut seen = HashSet::new();
    while let Some(current) = stack.pop() {
        if !seen.insert(current) {
            continue;
        }
        let Some(expr) = program.expr(current) else {
            continue;
        };
        match &expr.kind {
            ExprKind::Var(var) => mark_shared(*var, tables),
            ExprKind::Unary { expr, .. } => stack.push(*expr),
            ExprKind::Binary { lhs, rhs, .. } => {
                stack.push(*lhs);
                stack.push(*rhs);
            }
            ExprKind::PureCall { args, .. }
            | ExprKind::MakeStruct { fields: args, .. }
            | ExprKind::MakeEnum { fields: args, .. } => stack.extend(args.iter().copied()),
            ExprKind::Literal(_) | ExprKind::Error(_) => {}
        }
    }
}

fn mark_shared(var: VarId, tables: &mut ArcAliasTables) {
    tables.class_of_var.insert(var, ArcAliasClass::Shared);
}
