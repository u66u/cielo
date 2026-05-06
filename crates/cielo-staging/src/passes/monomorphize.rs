use std::collections::{HashMap, HashSet, VecDeque};

// Pass 3/9: monomorphize
//
// Inputs:
// - Typed Core program plus the type arguments inference recorded per call site
//
// Outputs:
// - Core holding one specialized copy per distinct type-argument tuple, with
//   call sites retargeted and the generic templates pruned
// - Semantic tables re-derived for the rewritten program
//
// Invariants:
// - Every function reachable from an entrypoint has a type-parameter-free signature
// - A specialized copy keeps its template's `declared_effects`: effect rows are
//   monomorphic, so nothing in the row varies with the type arguments
// - Cloned bodies get fresh VarIds, because `ownership_of_var` is keyed globally
//   and two instances of one template classify the same source var differently
//
// Diagnostics:
// - MONO_GENERIC_ENTRYPOINT, MONO_MISSING_TYPE_ARGS, MONO_POLYMORPHIC_RECURSION
//
// Complexity:
// - O(instances * body size)

use cielo_base::diagnostics::DiagnosticBag;
use cielo_base::{ExprId, FuncId, HandlerId, Span, StmtId, SymbolId, VarId};
use cielo_ir::core::{
    CoreProgram, CoreTypeRef, ExprKind, ExprNode, FunctionDecl, HandlerClause, HandlerDef,
    MatchArm, StmtKind, StmtNode,
};
use cielo_ir::function_graph::prune_unreachable_functions;
use cielo_sema::facts::CallSite;
use cielo_sema::typecheck::typecheck_core;
use cielo_sema::{SemanticTables, TypedCore};

use crate::pipeline::phases::{MonomorphizationSummary, Monomorphized};

/// A type argument this deep is only reachable by a call that wraps its own
/// type parameter on every recursive step, which never bottoms out.
const MAX_TYPE_ARG_DEPTH: usize = 8;
const MAX_INSTANCES: usize = 512;

/// Type arguments keyed by the callee's type-parameter name, in the callee's
/// declaration order. Doubles as the specialization key.
type Bindings = Vec<(SymbolId, CoreTypeRef)>;

pub fn run(typed: TypedCore) -> Monomorphized {
    let (mut program, mut diagnostics, sema) = typed.into_parts();

    // Specializing a program that already failed to typecheck would build
    // instances from unreliable type arguments.
    let specialize = !diagnostics.has_errors() && program.functions().iter().any(is_generic);
    if !specialize {
        let mono = identity_summary(&program);
        return Monomorphized::new(program, diagnostics, sema, mono);
    }

    let source_to_mono = Specializer::new(&sema, &mut diagnostics).run(&mut program);

    let remap = prune_unreachable_functions(&mut program);
    let mut mono = MonomorphizationSummary { source_to_mono };
    mono.remap_func_ids(&remap);
    for idx in 0..program.functions().len() {
        let id = FuncId::new(idx);
        mono.source_to_mono.entry(id).or_insert_with(|| vec![id]);
    }

    // Pruning leaves only what an entrypoint reaches, so anything still generic
    // here escaped specialization. Downstream passes would type it as an opaque
    // parameter and emit code for it, so it has to be an error.
    for function in program.functions() {
        if is_generic(function) {
            diagnostics.error(
                "MONO_UNSPECIALIZED_FUNCTION",
                "Generic function is reachable but was never specialized",
                function.span,
            );
        }
    }

    // The recorded types describe the templates, not the instances. The
    // rewritten program is concrete, so re-deriving every table is both
    // cheaper and less error-prone than substituting into each of them.
    let mut recheck = DiagnosticBag::default();
    let sema = typecheck_core(&program, &mut recheck);
    diagnostics.extend(recheck);

    Monomorphized::new(program, diagnostics, sema, mono)
}

fn identity_summary(program: &CoreProgram) -> MonomorphizationSummary {
    let source_to_mono = (0..program.functions().len())
        .map(|idx| {
            let id = FuncId::new(idx);
            (id, vec![id])
        })
        .collect();
    MonomorphizationSummary { source_to_mono }
}

fn is_generic(function: &FunctionDecl) -> bool {
    function.return_type.mentions_param()
        || function.param_types.iter().any(CoreTypeRef::mentions_param)
}

/// A function body still to be walked. A non-generic function is walked in
/// place; a generic one is cloned into a freshly declared instance.
struct Job {
    source: FuncId,
    target: FuncId,
    bindings: Bindings,
    clone_body: bool,
}

struct Specializer<'a> {
    sema: &'a SemanticTables,
    diagnostics: &'a mut DiagnosticBag,
    instances: HashMap<(FuncId, Bindings), FuncId>,
    source_to_mono: HashMap<FuncId, Vec<FuncId>>,
    queue: VecDeque<Job>,
    walked_in_place: HashSet<FuncId>,
    next_var: u32,
    recursion_reported: HashSet<FuncId>,
}

impl<'a> Specializer<'a> {
    fn new(sema: &'a SemanticTables, diagnostics: &'a mut DiagnosticBag) -> Self {
        Self {
            sema,
            diagnostics,
            instances: HashMap::new(),
            source_to_mono: HashMap::new(),
            queue: VecDeque::new(),
            walked_in_place: HashSet::new(),
            next_var: 0,
            recursion_reported: HashSet::new(),
        }
    }

    fn run(mut self, program: &mut CoreProgram) -> HashMap<FuncId, Vec<FuncId>> {
        self.next_var = next_var_id(program);

        for entry in program.entrypoints().to_vec() {
            match program.function(entry) {
                Some(function) if is_generic(function) => {
                    self.diagnostics.error(
                        "MONO_GENERIC_ENTRYPOINT",
                        "An entrypoint cannot be generic: nothing calls it, so its type arguments are never determined",
                        function.span,
                    );
                }
                Some(_) => self.enqueue_in_place(entry),
                None => {}
            }
        }

        while let Some(job) = self.queue.pop_front() {
            let Some(root) = program.function(job.source).map(|function| function.body) else {
                continue;
            };
            let sites = collect_call_sites(program, root);
            let rewrites = self.resolve_sites(program, &sites, &job.bindings);
            if job.clone_body {
                self.clone_body(program, &job, root, &rewrites);
            } else {
                apply_in_place(program, &rewrites);
            }
        }

        self.source_to_mono
    }

    fn enqueue_in_place(&mut self, func: FuncId) {
        if !self.walked_in_place.insert(func) {
            return;
        }
        self.source_to_mono
            .entry(func)
            .or_insert_with(|| vec![func]);
        self.queue.push_back(Job {
            source: func,
            target: func,
            bindings: Bindings::new(),
            clone_body: false,
        });
    }

    fn resolve_sites(
        &mut self,
        program: &mut CoreProgram,
        sites: &[(CallSite, FuncId, Span)],
        enclosing: &Bindings,
    ) -> HashMap<CallSite, FuncId> {
        let mut out = HashMap::new();
        for (site, callee, span) in sites.iter().copied() {
            let Some(function) = program.function(callee) else {
                continue;
            };
            if !is_generic(function) {
                self.enqueue_in_place(callee);
                continue;
            }

            let Some(recorded) = self.sema.type_args_of_call.get(&site) else {
                self.diagnostics.error(
                    "MONO_MISSING_TYPE_ARGS",
                    "Call to a generic function has no inferred type arguments",
                    span,
                );
                continue;
            };
            let bindings = recorded
                .iter()
                .map(|(name, ty)| (*name, substitute(ty, enclosing)))
                .collect::<Bindings>();
            if let Some(target) = self.ensure_instance(program, callee, bindings, span) {
                out.insert(site, target);
            }
        }
        out
    }

    fn ensure_instance(
        &mut self,
        program: &mut CoreProgram,
        callee: FuncId,
        bindings: Bindings,
        span: Span,
    ) -> Option<FuncId> {
        if let Some(existing) = self.instances.get(&(callee, bindings.clone())).copied() {
            return Some(existing);
        }

        let too_deep = bindings
            .iter()
            .any(|(_, ty)| ty.depth() > MAX_TYPE_ARG_DEPTH);
        if too_deep || self.instances.len() >= MAX_INSTANCES {
            if self.recursion_reported.insert(callee) {
                self.diagnostics.error(
                    "MONO_POLYMORPHIC_RECURSION",
                    "Generic function instantiates itself at an ever larger type, so it has no finite set of specializations",
                    span,
                );
            }
            return None;
        }

        let source = program.function(callee)?.clone();
        let params = source
            .params
            .iter()
            .map(|_| self.fresh_var())
            .collect::<Vec<_>>();
        let specialized = FunctionDecl {
            params,
            param_types: source
                .param_types
                .iter()
                .map(|ty| substitute(ty, &bindings))
                .collect(),
            return_type: substitute(&source.return_type, &bindings),
            // The body is patched once the clone exists; declaring the
            // instance first is what lets a recursive call find its own id.
            body: source.body,
            ..source
        };
        let target = program.add_function(specialized);

        self.instances.insert((callee, bindings.clone()), target);
        self.source_to_mono.entry(callee).or_default().push(target);
        self.queue.push_back(Job {
            source: callee,
            target,
            bindings,
            clone_body: true,
        });
        Some(target)
    }

    fn clone_body(
        &mut self,
        program: &mut CoreProgram,
        job: &Job,
        root: StmtId,
        rewrites: &HashMap<CallSite, FuncId>,
    ) {
        let source_params = program
            .function(job.source)
            .map(|function| function.params.clone())
            .unwrap_or_default();
        let target_params = program
            .function(job.target)
            .map(|function| function.params.clone())
            .unwrap_or_default();

        let mut cloner = BodyCloner {
            program,
            rewrites,
            next_var: self.next_var,
            vars: source_params
                .into_iter()
                .zip(target_params)
                .collect::<HashMap<_, _>>(),
            stmts: HashMap::new(),
            exprs: HashMap::new(),
            handlers: HashMap::new(),
        };
        let body = cloner.clone_stmt(root);
        self.next_var = cloner.next_var;

        if let Some(function) = program.function_mut(job.target) {
            function.body = body;
        }
    }

    fn fresh_var(&mut self) -> VarId {
        let id = VarId::from_u32(self.next_var);
        self.next_var += 1;
        id
    }
}

fn substitute(ty: &CoreTypeRef, bindings: &Bindings) -> CoreTypeRef {
    ty.substitute(&|name| {
        bindings
            .iter()
            .find(|(bound, _)| *bound == name)
            .map(|(_, ty)| ty.clone())
    })
}

fn apply_in_place(program: &mut CoreProgram, rewrites: &HashMap<CallSite, FuncId>) {
    for (site, target) in rewrites {
        match site {
            CallSite::Stmt(stmt_id) => {
                if let Some(stmt) = program.stmt_mut(*stmt_id)
                    && let StmtKind::Call { callee, .. } = &mut stmt.kind
                {
                    *callee = *target;
                }
            }
            CallSite::Expr(expr_id) => {
                if let Some(expr) = program.expr_mut(*expr_id)
                    && let ExprKind::PureCall { callee, .. } = &mut expr.kind
                {
                    *callee = *target;
                }
            }
        }
    }
}

/// Every call reachable from `root`, including through the handlers installed
/// inside it: a handler clause body belongs to the enclosing function and is
/// specialized along with it.
fn collect_call_sites(program: &CoreProgram, root: StmtId) -> Vec<(CallSite, FuncId, Span)> {
    let mut out = Vec::new();
    let mut seen_stmts = HashSet::new();
    let mut seen_exprs = HashSet::new();
    let mut stack = vec![root];

    while let Some(stmt_id) = stack.pop() {
        if !seen_stmts.insert(stmt_id) {
            continue;
        }
        let Some(stmt) = program.stmt(stmt_id) else {
            continue;
        };
        match &stmt.kind {
            StmtKind::Call { callee, .. } => {
                out.push((CallSite::Stmt(stmt_id), *callee, stmt.span));
            }
            StmtKind::Handle { handler, .. } => {
                if let Some(def) = program.handlers().get(handler.index()) {
                    stack.push(def.return_body);
                    stack.extend(def.clauses.iter().map(|clause| clause.body));
                }
            }
            _ => {}
        }
        for expr_id in stmt.child_exprs() {
            collect_expr_call_sites(program, expr_id, &mut seen_exprs, &mut out);
        }
        stack.extend(stmt.child_stmts());
    }
    out
}

fn collect_expr_call_sites(
    program: &CoreProgram,
    expr_id: ExprId,
    seen: &mut HashSet<ExprId>,
    out: &mut Vec<(CallSite, FuncId, Span)>,
) {
    if !seen.insert(expr_id) {
        return;
    }
    let Some(expr) = program.expr(expr_id) else {
        return;
    };
    match &expr.kind {
        ExprKind::PureCall { callee, args } => {
            out.push((CallSite::Expr(expr_id), *callee, expr.span));
            for arg in args {
                collect_expr_call_sites(program, *arg, seen, out);
            }
        }
        ExprKind::Unary { expr, .. } | ExprKind::Field { base: expr, .. } => {
            collect_expr_call_sites(program, *expr, seen, out);
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            collect_expr_call_sites(program, *lhs, seen, out);
            collect_expr_call_sites(program, *rhs, seen, out);
        }
        ExprKind::MakeStruct { fields, .. } | ExprKind::MakeEnum { fields, .. } => {
            for field in fields {
                collect_expr_call_sites(program, *field, seen, out);
            }
        }
        ExprKind::Var(_) | ExprKind::Literal(_) | ExprKind::Error(_) => {}
    }
}

/// Copies a body into a specialized instance. Vars are renamed because the
/// clone coexists with its template's other instances in one global var space,
/// and handlers are copied because their clause bodies are part of the body.
struct BodyCloner<'a> {
    program: &'a mut CoreProgram,
    rewrites: &'a HashMap<CallSite, FuncId>,
    next_var: u32,
    vars: HashMap<VarId, VarId>,
    stmts: HashMap<StmtId, StmtId>,
    exprs: HashMap<ExprId, ExprId>,
    handlers: HashMap<HandlerId, HandlerId>,
}

impl BodyCloner<'_> {
    fn var(&mut self, var: VarId) -> VarId {
        if let Some(existing) = self.vars.get(&var).copied() {
            return existing;
        }
        let fresh = VarId::from_u32(self.next_var);
        self.next_var += 1;
        self.vars.insert(var, fresh);
        fresh
    }

    fn vars_of(&mut self, vars: Vec<VarId>) -> Vec<VarId> {
        vars.into_iter().map(|var| self.var(var)).collect()
    }

    fn clone_exprs(&mut self, exprs: Vec<ExprId>) -> Vec<ExprId> {
        exprs.into_iter().map(|id| self.clone_expr(id)).collect()
    }

    fn clone_optional_stmt(&mut self, stmt: Option<StmtId>) -> Option<StmtId> {
        stmt.map(|id| self.clone_stmt(id))
    }

    fn clone_handler(&mut self, handler: HandlerId) -> HandlerId {
        if let Some(existing) = self.handlers.get(&handler).copied() {
            return existing;
        }
        let Some(def) = self.program.handlers().get(handler.index()).cloned() else {
            return handler;
        };
        let return_param = self.var(def.return_param);
        let return_body = self.clone_stmt(def.return_body);
        let clauses = def
            .clauses
            .into_iter()
            .map(|clause| HandlerClause {
                operation: clause.operation,
                params: self.vars_of(clause.params),
                resume_param: clause.resume_param.map(|param| self.var(param)),
                body: self.clone_stmt(clause.body),
                span: clause.span,
            })
            .collect();
        let cloned = self.program.add_handler(HandlerDef {
            effect: def.effect,
            return_param,
            return_body,
            clauses,
            span: def.span,
        });
        self.handlers.insert(handler, cloned);
        cloned
    }

    fn clone_stmt(&mut self, source: StmtId) -> StmtId {
        if let Some(existing) = self.stmts.get(&source).copied() {
            return existing;
        }
        let Some(node) = self.program.stmt(source).cloned() else {
            return source;
        };

        let kind = match node.kind {
            StmtKind::Return(expr) => StmtKind::Return(self.clone_expr(expr)),
            StmtKind::Let {
                binding,
                value,
                next,
            } => StmtKind::Let {
                binding: self.var(binding),
                value: self.clone_expr(value),
                next: self.clone_stmt(next),
            },
            StmtKind::Val {
                binding,
                value,
                next,
            } => StmtKind::Val {
                binding: self.var(binding),
                value: self.clone_stmt(value),
                next: self.clone_stmt(next),
            },
            StmtKind::Call {
                result,
                callee,
                args,
                effects,
                next,
            } => StmtKind::Call {
                result: self.var(result),
                callee: self
                    .rewrites
                    .get(&CallSite::Stmt(source))
                    .copied()
                    .unwrap_or(callee),
                args: self.clone_exprs(args),
                effects,
                next: self.clone_stmt(next),
            },
            StmtKind::If {
                cond,
                then_branch,
                else_branch,
            } => StmtKind::If {
                cond: self.clone_expr(cond),
                then_branch: self.clone_stmt(then_branch),
                else_branch: self.clone_stmt(else_branch),
            },
            StmtKind::Match {
                scrutinee,
                arms,
                default,
            } => StmtKind::Match {
                scrutinee: self.clone_expr(scrutinee),
                arms: arms
                    .into_iter()
                    .map(|arm| MatchArm {
                        tag: arm.tag,
                        binders: self.vars_of(arm.binders),
                        body: self.clone_stmt(arm.body),
                        span: arm.span,
                    })
                    .collect(),
                default: self.clone_optional_stmt(default),
            },
            StmtKind::Perform {
                result,
                effect,
                operation,
                args,
                next,
            } => StmtKind::Perform {
                result: result.map(|var| self.var(var)),
                effect,
                operation,
                args: self.clone_exprs(args),
                next: self.clone_stmt(next),
            },
            StmtKind::Resume {
                result,
                resume,
                arg,
                next,
            } => StmtKind::Resume {
                result: self.var(result),
                resume: self.var(resume),
                arg: self.clone_expr(arg),
                next: self.clone_stmt(next),
            },
            StmtKind::Handle {
                handler,
                body,
                next,
            } => StmtKind::Handle {
                handler: self.clone_handler(handler),
                body: self.clone_stmt(body),
                next: self.clone_optional_stmt(next),
            },
            StmtKind::Stage { stage, body, next } => StmtKind::Stage {
                stage,
                body: self.clone_stmt(body),
                next: self.clone_optional_stmt(next),
            },
            StmtKind::Hole { ty } => StmtKind::Hole { ty },
            StmtKind::Error(error) => StmtKind::Error(error),
        };

        let cloned = self.program.push_stmt(StmtNode {
            span: node.span,
            kind,
        });
        self.stmts.insert(source, cloned);
        cloned
    }

    fn clone_expr(&mut self, source: ExprId) -> ExprId {
        if let Some(existing) = self.exprs.get(&source).copied() {
            return existing;
        }
        let Some(node) = self.program.expr(source).cloned() else {
            return source;
        };

        let kind = match node.kind {
            ExprKind::Var(var) => ExprKind::Var(self.var(var)),
            ExprKind::Literal(literal) => ExprKind::Literal(literal),
            ExprKind::Unary { op, expr } => ExprKind::Unary {
                op,
                expr: self.clone_expr(expr),
            },
            ExprKind::Field { base, field } => ExprKind::Field {
                base: self.clone_expr(base),
                field,
            },
            ExprKind::Binary { op, lhs, rhs } => ExprKind::Binary {
                op,
                lhs: self.clone_expr(lhs),
                rhs: self.clone_expr(rhs),
            },
            ExprKind::PureCall { callee, args } => ExprKind::PureCall {
                callee: self
                    .rewrites
                    .get(&CallSite::Expr(source))
                    .copied()
                    .unwrap_or(callee),
                args: self.clone_exprs(args),
            },
            ExprKind::MakeStruct { ty, fields } => ExprKind::MakeStruct {
                ty,
                fields: self.clone_exprs(fields),
            },
            ExprKind::MakeEnum {
                ty,
                variant,
                fields,
            } => ExprKind::MakeEnum {
                ty,
                variant,
                fields: self.clone_exprs(fields),
            },
            ExprKind::Error(error) => ExprKind::Error(error),
        };

        let cloned = self.program.push_expr(ExprNode {
            span: node.span,
            kind,
        });
        self.exprs.insert(source, cloned);
        cloned
    }
}

fn next_var_id(program: &CoreProgram) -> u32 {
    let mut max = 0u32;
    let mut note = |var: VarId| max = max.max(var.as_u32() + 1);

    for function in program.functions() {
        function.params.iter().copied().for_each(&mut note);
    }
    for handler in program.handlers() {
        note(handler.return_param);
        for clause in &handler.clauses {
            clause.params.iter().copied().for_each(&mut note);
            if let Some(resume) = clause.resume_param {
                note(resume);
            }
        }
    }
    for stmt in program.stmts() {
        match &stmt.kind {
            StmtKind::Let { binding, .. } | StmtKind::Val { binding, .. } => note(*binding),
            StmtKind::Call { result, .. } => note(*result),
            StmtKind::Resume { result, resume, .. } => {
                note(*result);
                note(*resume);
            }
            StmtKind::Perform { result, .. } => {
                if let Some(result) = result {
                    note(*result);
                }
            }
            StmtKind::Match { arms, .. } => {
                for arm in arms {
                    arm.binders.iter().copied().for_each(&mut note);
                }
            }
            StmtKind::Return(_)
            | StmtKind::If { .. }
            | StmtKind::Handle { .. }
            | StmtKind::Stage { .. }
            | StmtKind::Hole { .. }
            | StmtKind::Error(_) => {}
        }
    }
    for expr in program.exprs() {
        if let ExprKind::Var(var) = expr.kind {
            note(var);
        }
    }
    max
}
