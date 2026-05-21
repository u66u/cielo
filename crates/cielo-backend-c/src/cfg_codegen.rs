//! Direct CFG-to-C lowering.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write;

use crate::c_constants::CConstantPools;
use cielo_base::ids::{
    CfgBlockId, CfgExprId, CfgFuncId, CfgHandlerId, CfgRegionId, CfgValueId, EffectLabelId,
    SymbolId,
};
use cielo_base::symbols::Interner;
use cielo_ir::builtins::Builtin;
use cielo_ir::cfg::{
    CfgArcOp, CfgArcOpKind, CfgBlock, CfgCallConvention, CfgExpr, CfgFunction, CfgInstruction,
    CfgProgram, CfgProjectionMode, CfgTerminator, ctor_literal_key,
};
use cielo_ir::constants::{ConstantTable, ScalarLiteralKey};
use cielo_ir::core::Literal;
use cielo_ir::region::{Placement, RegionOwner, RegionSlotKind};
use cielo_ir::walk::{Walk, walk_exprs};

use crate::structure::{self, Edge, Layout, Region};

const C_RUNTIME_HEADER: &str = include_str!("cielo_runtime.h");

/// Separates the runtime header from everything this program emitted.
///
/// Counting `cielo_arc_retain` over a whole emitted file measures the header,
/// which defines and calls the helper itself. The storage class of an emitted
/// function is not a boundary to key on: a small body earns `static inline`.
pub const EMITTED_BODIES_MARKER: &str = "/* --- cielo emitted bodies --- */\n";

pub fn emit(
    program: &CfgProgram,
    interner: &Interner,
    constants: &ConstantTable,
    arc_trace: bool,
) -> String {
    let mut out = String::new();
    // The generated unit owns the runtime's mutable state and out-of-line
    // functions; anything else that includes the header links against it.
    out.push_str("#define CIELO_RUNTIME_IMPL\n");
    emit_builtin_table(&mut out, program, interner);
    out.push_str(C_RUNTIME_HEADER);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    let mut pools = CConstantPools::build(constants, interner);
    // The constant table drops strings once its size budget is spent, but a
    // string literal has no inline form, so every one still needs an object.
    for expression in program.exprs() {
        if let CfgExpr::Literal(Literal::String(value)) = &expression.kind {
            pools.ensure_string(value);
        }
    }
    out.push_str(&pools.declarations);
    if !pools.declarations.is_empty() {
        out.push('\n');
    }

    out.push_str(EMITTED_BODIES_MARKER);
    out.push('\n');

    let names = function_names(program, interner);
    let tails = tail_call_blocks(program);
    let recursive = recursive_functions(program);
    let signatures = program
        .functions
        .iter()
        .map(|function| {
            (
                function.id,
                Signature::of(program, function, &tails, &recursive),
            )
        })
        .collect::<HashMap<_, _>>();
    for function in &program.functions {
        emit_signature(
            &mut out,
            function,
            &names[&function.id],
            &signatures[&function.id],
        );
        out.push_str(";\n");
    }
    out.push('\n');
    emit_closure_adapters(&mut out, program, &names);
    emit_clause_tables(&mut out, program, &names);
    for function in &program.functions {
        emit_function(
            &mut out,
            program,
            function,
            &names,
            &signatures[&function.id],
            interner,
            &pools,
            arc_trace,
            &tails,
        );
        out.push('\n');
    }
    emit_main(&mut out, program, &names);
    out
}

/// Keyed by id, not name: handler specialization clones a function without
/// renaming it, so several functions share one name symbol.
fn function_names(program: &CfgProgram, interner: &Interner) -> HashMap<CfgFuncId, String> {
    program
        .functions
        .iter()
        .map(|function| {
            (
                function.id,
                format!(
                    "cielo_fn_{}_{}",
                    sanitize(symbol_text(interner, function.name)),
                    function.id.as_u32()
                ),
            )
        })
        .collect()
}

/// One adapter per residual clause plus the `CieloClauseEntry` array the
/// evidence points at.
///
/// The adapter exists only to unpack the dispatcher's flat argument array into
/// the clause function's parameters. It takes ownership of those arguments,
/// matching the sink convention every other call site uses, and returns the
/// resumption argument, which `cielo_dispatch_with_evidence` hands back to the
/// perform site.
fn emit_clause_tables(out: &mut String, program: &CfgProgram, names: &HashMap<CfgFuncId, String>) {
    for table in program.clause_tables() {
        if table.clauses.is_empty() {
            continue;
        }
        for (index, clause) in table.clauses.iter().enumerate() {
            let arity = program
                .functions
                .get(clause.function.index())
                .map(|function| function.params.len())
                .unwrap_or(0);
            let callee = names
                .get(&clause.function)
                .cloned()
                .unwrap_or_else(|| format!("cielo_missing_fn_{}", clause.function.as_u32()));
            writeln!(
                out,
                "static CieloValue cielo_clause_h{}_{index}(CieloEvidence *evidence, CieloContinuation *continuation, size_t argc, const CieloValue *args) {{",
                table.handler.as_u32()
            )
            .expect("in-memory write");
            out.push_str("    (void)evidence;\n    (void)continuation;\n");
            writeln!(
                out,
                "    if (argc != {arity}u) cielo_trap(\"handler clause arity mismatch\");"
            )
            .expect("in-memory write");
            let args = (0..arity)
                .map(|index| format!("args[{index}]"))
                .collect::<Vec<_>>();
            writeln!(out, "    return {callee}({});\n}}", args.join(", "))
                .expect("in-memory write");
        }
        writeln!(
            out,
            "static const CieloClauseEntry cielo_clauses_h{}[] = {{",
            table.handler.as_u32()
        )
        .expect("in-memory write");
        for (index, clause) in table.clauses.iter().enumerate() {
            writeln!(
                out,
                "    {{ {}u, cielo_clause_h{}_{index} }},",
                clause.operation.as_u32(),
                table.handler.as_u32()
            )
            .expect("in-memory write");
        }
        out.push_str("};\n\n");
    }
}

fn closure_adapter_name(function: CfgFuncId) -> String {
    format!("cielo_closure_fn_{}", function.as_u32())
}

/// One adapter per lifted closure body: the uniform `CieloClosureFn` signature
/// outside, the body's own parameter list inside.
///
/// The capture count comes from the construction sites, not the declaration,
/// which is why the adapter re-checks it: every site naming one body was built
/// from one lambda and passes the same number, and a disagreement would
/// otherwise read past the end of the environment.
fn emit_closure_adapters(
    out: &mut String,
    program: &CfgProgram,
    names: &HashMap<CfgFuncId, String>,
) {
    let adapters = closure_capture_counts(program);
    for (function, captures) in &adapters {
        let (function, captures) = (*function, *captures);
        let arity = program
            .functions
            .get(function.index())
            .map(|node| node.params.len().saturating_sub(captures))
            .unwrap_or(0);
        let callee = names
            .get(&function)
            .cloned()
            .unwrap_or_else(|| format!("cielo_missing_fn_{}", function.as_u32()));
        writeln!(
            out,
            "static CieloValue {}(size_t capture_count, const CieloValue *captures, size_t argc, const CieloValue *args) {{",
            closure_adapter_name(function)
        )
        .expect("in-memory write");
        writeln!(
            out,
            "    if (capture_count != {captures}u || argc != {arity}u) cielo_trap(\"closure arity mismatch\");"
        )
        .expect("in-memory write");
        if captures == 0 {
            out.push_str("    (void)captures;\n");
        }
        if arity == 0 {
            out.push_str("    (void)args;\n");
        }
        // Captures belong to a closure that may be called again, and the body's
        // parameters are sink arguments, so each capture is retained on the way
        // in. Arguments arrive owned and pass straight through.
        let arguments = (0..captures)
            .map(|index| format!("cielo_arc_retained(captures[{index}])"))
            .chain((0..arity).map(|index| format!("args[{index}]")))
            .collect::<Vec<_>>();
        writeln!(out, "    return {callee}({});\n}}", arguments.join(", "))
            .expect("in-memory write");
    }
    if !adapters.is_empty() {
        out.push('\n');
    }
}

/// Capture count per lifted body, in dense id order so the emitted adapters do
/// not move between runs.
fn closure_capture_counts(program: &CfgProgram) -> Vec<(CfgFuncId, usize)> {
    let mut counts: HashMap<CfgFuncId, usize> = HashMap::new();
    for expression in program.exprs() {
        if let CfgExpr::MakeClosure {
            callee_fn,
            captures,
            ..
        } = &expression.kind
        {
            counts.entry(*callee_fn).or_insert(captures.len());
        }
    }
    let mut counts = counts.into_iter().collect::<Vec<_>>();
    counts.sort_by_key(|(function, _)| function.index());
    counts
}

/// Blocks whose call the emitter returns from directly, so the C compiler can
/// reuse the frame instead of growing the stack (CIELO-60).
///
/// Eligible when the terminator is a call, nothing is released after it, and
/// its result reaches a `Return` through blocks that do nothing. A release in
/// `terminator_arc.post` is the usual disqualifier: it has to run once the call
/// comes back, and a tail call never comes back.
fn tail_call_blocks(program: &CfgProgram) -> HashSet<CfgBlockId> {
    program
        .blocks()
        .iter()
        .filter(|block| is_tail_call(program, block))
        .map(|block| block.id)
        .collect()
}

fn is_tail_call(program: &CfgProgram, block: &CfgBlock) -> bool {
    if !block.terminator_arc.post.is_empty() {
        return false;
    }
    let Some((target, value)) = call_result(program, &block.terminator) else {
        return false;
    };
    forwards_to_return(program, target, value)
}

/// The successor a call terminator hands its result to, and the value that
/// successor receives it as.
fn call_result(
    program: &CfgProgram,
    terminator: &CfgTerminator,
) -> Option<(CfgBlockId, CfgValueId)> {
    match terminator {
        CfgTerminator::Call { target, result, .. } => Some((*target, *result)),
        // A call the lowering left in edge-argument position. Only the outermost
        // node counts: in `f(x) + 1` the addition runs after `f` returns.
        CfgTerminator::Goto { target, args } if args.len() == 1 => {
            if !matches!(
                program.expr(args[0]).map(|node| &node.kind),
                Some(CfgExpr::PureCall { .. })
            ) {
                return None;
            }
            let param = *program.block(*target)?.params.first()?;
            Some((*target, param))
        }
        _ => None,
    }
}

/// Whether `block` runs nothing before returning the call's result.
///
/// `block` is the call's continuation, so it takes the result as its one
/// parameter. The `Goto`s after it only rename that value, and skipping the
/// renames is safe precisely because the function returns next: no other read
/// of the names passed over can happen.
fn forwards_to_return(program: &CfgProgram, block: CfgBlockId, value: CfgValueId) -> bool {
    let mut block = block;
    let mut value = value;
    let mut first = true;
    // A hand-built CFG can loop through empty blocks forever.
    let mut seen = HashSet::new();
    while seen.insert(block) {
        let Some(node) = program.block(block) else {
            return false;
        };
        if first && node.params.as_slice() != [value] {
            return false;
        }
        first = false;
        if !node.instructions.is_empty()
            || !node.entry_arc.is_empty()
            || !node.terminator_arc.pre.is_empty()
            || !node.terminator_arc.post.is_empty()
        {
            return false;
        }
        match &node.terminator {
            CfgTerminator::Return(expr) => return is_value(program, *expr, value),
            CfgTerminator::Goto { target, args } if args.is_empty() => block = *target,
            CfgTerminator::Goto { target, args } if args.len() == 1 => {
                let Some(renamed) = program.block(*target).and_then(|node| node.params.first())
                else {
                    return false;
                };
                if !is_value(program, args[0], value) {
                    return false;
                }
                value = *renamed;
                block = *target;
            }
            _ => return false,
        }
    }
    false
}

fn is_value(program: &CfgProgram, expression: CfgExprId, value: CfgValueId) -> bool {
    matches!(
        program.expr(expression).map(|node| &node.kind),
        Some(CfgExpr::Value(found)) if *found == value
    )
}

/// Functions that can reach themselves through direct calls.
///
/// Indirect edges — closure bodies and residual clauses, both entered through a
/// function pointer — are left out. GCC cannot inline through those either, so
/// a cycle that only closes through one is not a cycle it can collapse.
fn recursive_functions(program: &CfgProgram) -> HashSet<CfgFuncId> {
    let callees = program
        .functions
        .iter()
        .map(|function| (function.id, direct_callees(program, function)))
        .collect::<HashMap<_, _>>();
    let mut recursive = HashSet::new();
    for function in &program.functions {
        let mut seen = HashSet::new();
        let mut stack = callees[&function.id].iter().copied().collect::<Vec<_>>();
        while let Some(callee) = stack.pop() {
            if callee == function.id {
                recursive.insert(function.id);
                break;
            }
            if seen.insert(callee)
                && let Some(next) = callees.get(&callee)
            {
                stack.extend(next.iter().copied());
            }
        }
    }
    recursive
}

fn direct_callees(program: &CfgProgram, function: &CfgFunction) -> HashSet<CfgFuncId> {
    let mut callees = HashSet::new();
    for block_id in reachable_blocks(program, function.entry, &HashSet::new()) {
        let block = program.block(block_id).expect("known block");
        if let CfgTerminator::Call { callee_fn, .. } = &block.terminator {
            callees.insert(*callee_fn);
        }
        let expressions =
            block
                .terminator
                .child_exprs()
                .into_iter()
                .chain(block.instructions.iter().flat_map(|instruction| {
                    program
                        .instruction(*instruction)
                        .map(|node| node.kind.child_exprs())
                        .unwrap_or_default()
                }));
        for expression in expressions {
            walk_exprs(program, expression, &mut |_, node| {
                if let CfgExpr::PureCall { callee_fn, .. } = &node.kind {
                    callees.insert(*callee_fn);
                }
                Walk::Descend
            });
        }
    }
    callees
}

/// Reachable instructions a body may hold and still be offered for inlining.
/// Well under GCC's own `max-inline-insns-single`, since one CFG instruction
/// expands to several C statements.
const INLINE_INSTRUCTION_BUDGET: usize = 8;

/// What the declaration and the definition have to agree on.
struct Signature {
    /// `static inline` moves GCC's per-call-site budget from
    /// `max-inline-insns-auto` up to `max-inline-insns-single`, and stops a
    /// handler specialization nothing calls from tripping `-Wunused-function`.
    /// Never set on a body that opens a handler or region scope: an evidence
    /// frame is not worth duplicating at every call site.
    ///
    /// Never set on a recursive body that tail-calls. The raised budget is
    /// enough for GCC to inline such a body into itself, and the copy's two
    /// exits merge, which puts the innermost call back out of tail position and
    /// loses the sibling call the emitted `return` was for. Recursion is what
    /// makes that fatal rather than merely larger, and a body on a call cycle
    /// is referenced, so dropping `inline` cannot make it look unused.
    inline: bool,
    /// Parameters nothing in the body writes back into. The CFG reuses one
    /// value namespace for parameters, block parameters and instruction
    /// results, so a parameter an edge assigns must stay mutable.
    const_params: HashSet<CfgValueId>,
}

impl Signature {
    fn of(
        program: &CfgProgram,
        function: &CfgFunction,
        tails: &HashSet<CfgBlockId>,
        recursive: &HashSet<CfgFuncId>,
    ) -> Self {
        let blocks = reachable_blocks(program, function.entry, tails);
        let mut instructions = 0;
        let mut opens_scope = false;
        let mut tail_calls = false;
        let mut assigned = HashSet::new();
        for block_id in &blocks {
            tail_calls |= tails.contains(block_id);
            let block = program.block(*block_id).expect("known block");
            // Only the writes the emitter actually performs. A block's own
            // parameters are not among them: the entry block's parameters *are*
            // the function's, and every other block's are filled in either by a
            // `Goto` edge below or by the terminator that defines them.
            if let CfgTerminator::Goto { target, args } = &block.terminator
                && let Some(node) = program.block(*target)
            {
                assigned.extend(node.params.iter().take(args.len()).copied());
            }
            assigned.extend(block.terminator.defined_values());
            for instruction in &block.instructions {
                let Some(node) = program.instruction(*instruction) else {
                    continue;
                };
                instructions += 1;
                assigned.extend(node.kind.result());
                opens_scope |= matches!(
                    node.kind,
                    CfgInstruction::HandlerEnter { .. } | CfgInstruction::RegionEnter { .. }
                );
            }
        }
        Self {
            inline: !opens_scope
                && instructions <= INLINE_INSTRUCTION_BUDGET
                && !(tail_calls && recursive.contains(&function.id)),
            const_params: function
                .params
                .iter()
                .copied()
                .filter(|param| !assigned.contains(param))
                .collect(),
        }
    }
}

fn emit_signature(out: &mut String, function: &CfgFunction, name: &str, signature: &Signature) {
    let storage = if signature.inline {
        "static inline"
    } else {
        "static"
    };
    write!(out, "{storage} CieloValue {name}(").expect("in-memory write");
    for (idx, value) in function.params.iter().enumerate() {
        if idx > 0 {
            out.push_str(", ");
        }
        let qualifier = if signature.const_params.contains(value) {
            "const "
        } else {
            ""
        };
        write!(out, "{qualifier}CieloValue v{}", value.as_u32()).expect("in-memory write");
    }
    out.push(')');
}

#[allow(clippy::too_many_arguments)]
fn emit_function(
    out: &mut String,
    program: &CfgProgram,
    function: &CfgFunction,
    names: &HashMap<CfgFuncId, String>,
    signature: &Signature,
    interner: &Interner,
    pools: &CConstantPools,
    arc_trace: bool,
    tails: &HashSet<CfgBlockId>,
) {
    emit_signature(out, function, &names[&function.id], signature);
    out.push_str(" {\n");
    let params = function.params.iter().copied().collect::<HashSet<_>>();
    for value in reachable_values(program, function.entry, tails) {
        if !params.contains(&value) {
            writeln!(out, "    CieloValue v{} = cv_unit();", value.as_u32())
                .expect("in-memory write");
        }
    }
    let handlers = reachable_handlers(program, function.entry, tails);
    let active_handlers = active_handler_states(program, function.entry, tails);
    let placements = evidence_placements(program);
    for handler in handlers {
        writeln!(out, "    uint32_t hcap{} = 0;", handler.as_u32()).expect("in-memory write");
        match placements.get(&handler).copied().unwrap_or_default() {
            Placement::Stack => {
                writeln!(out, "    CieloEvidence hev{};", handler.as_u32())
                    .expect("in-memory write");
            }
            Placement::Arena => {
                writeln!(out, "    CieloEvidence *hev{} = NULL;", handler.as_u32())
                    .expect("in-memory write");
            }
        }
    }
    for region in reachable_regions(program, function.entry, tails) {
        if program
            .region(region)
            .is_none_or(cielo_ir::region::CfgRegion::is_fully_stack)
        {
            continue;
        }
        writeln!(out, "    CieloRegion reg{} = {{ NULL }};", region.as_u32())
            .expect("in-memory write");
    }

    let layout = structure::plan(program, function.entry, tails);
    let mut cx = EmitCx {
        program,
        names,
        tails,
        interner,
        pools,
        active_handlers,
        placements,
        arc_trace,
        temp: 0,
    };
    emit_region(out, &layout.root, &layout, 1, &mut cx);
    out.push_str("}\n");
}

fn pad(indent: usize) -> String {
    "    ".repeat(indent)
}

fn emit_region(
    out: &mut String,
    region: &Region,
    layout: &Layout,
    indent: usize,
    cx: &mut EmitCx<'_>,
) {
    let program = cx.program;
    let block = program.block(region.block).expect("known block");
    if layout.labels.contains(&region.block) {
        // The null statement is load-bearing: C11 wants a statement after a
        // label, and the next thing emitted is often a declaration.
        writeln!(out, "{}b{}: ;", pad(indent), region.block.as_u32()).expect("in-memory write");
    }
    emit_arc_ops(
        out,
        &block.entry_arc,
        indent,
        cx,
        "entry",
        region.block.as_u32(),
    );
    for instruction_id in &block.instructions {
        let instruction = program
            .instruction(*instruction_id)
            .expect("known instruction");
        emit_arc_ops(
            out,
            &instruction.arc.pre,
            indent,
            cx,
            "pre",
            instruction.id.as_u32(),
        );
        emit_instruction(out, instruction, indent, cx);
        emit_arc_ops(
            out,
            &instruction.arc.post,
            indent,
            cx,
            "post",
            instruction.id.as_u32(),
        );
    }
    emit_terminator(out, region, layout, indent, cx);
    for join in &region.joins {
        emit_region(out, join, layout, indent, cx);
    }
}

fn emit_instruction(
    out: &mut String,
    instruction: &cielo_ir::cfg::CfgInstructionNode,
    indent: usize,
    cx: &mut EmitCx<'_>,
) {
    let program = cx.program;
    let pad = pad(indent);
    match &instruction.kind {
        CfgInstruction::Let { result, value } | CfgInstruction::Eval { result, value } => {
            let expression = emit_expr(*value, cx);
            writeln!(out, "{pad}v{} = {expression};", result.as_u32()).expect("in-memory write");
        }
        CfgInstruction::HandlerEnter { handler, effect } => {
            let placement = cx.placements.get(handler).copied().unwrap_or_default();
            let (slot, address) = match placement {
                Placement::Stack => (
                    format!("hev{}", handler.as_u32()),
                    format!("&hev{}", handler.as_u32()),
                ),
                Placement::Arena => (
                    format!("(*hev{})", handler.as_u32()),
                    format!("hev{}", handler.as_u32()),
                ),
            };
            let clauses = program.clauses_of(*handler);
            let table = if clauses.is_empty() {
                "NULL".to_owned()
            } else {
                format!("cielo_clauses_h{}", handler.as_u32())
            };
            writeln!(
                out,
                "{pad}{slot} = (CieloEvidence){{ .abi_version = cielo_runtime_abi_version(), .effect = {}, .capability_id = 0, .clause_count = {}, .clauses = {table}, .captures = NULL, .reserved0 = NULL, .reserved1 = NULL }};",
                effect.as_u32(),
                clauses.len()
            )
            .expect("in-memory write");
            writeln!(
                out,
                "{pad}hcap{} = cielo_handler_push_with_evidence({}, {address});",
                handler.as_u32(),
                effect.as_u32(),
            )
            .expect("in-memory write");
        }
        CfgInstruction::HandlerExit { handler, .. } => {
            writeln!(out, "{pad}cielo_handler_pop(hcap{});", handler.as_u32())
                .expect("in-memory write");
        }
        CfgInstruction::RegionEnter { region } => {
            emit_region_open(out, program, *region, &cx.placements, indent);
        }
        CfgInstruction::RegionExit { region } => {
            if program
                .region(*region)
                .is_some_and(|region| !region.is_fully_stack())
            {
                writeln!(out, "{pad}cielo_region_close(&reg{});", region.as_u32())
                    .expect("in-memory write");
            }
        }
        CfgInstruction::StageEnter { stage } => {
            writeln!(out, "{pad}/* stage {stage:?} enter */").expect("in-memory write");
        }
        CfgInstruction::StageExit { stage } => {
            writeln!(out, "{pad}/* stage {stage:?} exit */").expect("in-memory write");
        }
        CfgInstruction::Hole => writeln!(out, "{pad}/* hole */").expect("in-memory write"),
        CfgInstruction::Error => writeln!(out, "{pad}/* error */").expect("in-memory write"),
    }
}

/// Emits whatever the edge needs to reach its target: nothing when the target
/// is laid out next, a `goto` when it is not, or the target's whole body when
/// this is the only edge into it.
fn emit_edge(out: &mut String, edge: &Edge, layout: &Layout, indent: usize, cx: &mut EmitCx<'_>) {
    match edge {
        Edge::Inline(region) => emit_region(out, region, layout, indent, cx),
        Edge::Goto(target) => {
            writeln!(out, "{}goto b{};", pad(indent), target.as_u32()).expect("in-memory write");
        }
        Edge::Fallthrough => {}
    }
}

fn emit_terminator(
    out: &mut String,
    region: &Region,
    layout: &Layout,
    indent: usize,
    cx: &mut EmitCx<'_>,
) {
    let program = cx.program;
    let block = region.block;
    let node = program.block(block).expect("known block");
    let arc = &node.terminator_arc;
    let nested = pad(indent + 1);
    let pad = pad(indent);
    emit_arc_ops(out, &arc.pre, indent, cx, "term-pre", block.as_u32());
    let edge = |index: usize| region.edges.get(index).expect("edge per successor");
    match &node.terminator {
        CfgTerminator::Return(value) => {
            let temp = cx.fresh("return");
            let value = emit_expr(*value, cx);
            writeln!(out, "{pad}CieloValue {temp} = {value};").expect("in-memory write");
            emit_arc_ops(out, &arc.post, indent, cx, "term-post", block.as_u32());
            writeln!(out, "{pad}return {temp};").expect("in-memory write");
        }
        CfgTerminator::Goto { args, .. } if cx.tails.contains(&block) => {
            let value = emit_expr(args[0], cx);
            writeln!(out, "{pad}return {value};").expect("in-memory write");
        }
        CfgTerminator::Goto { target, args } => {
            emit_edge_values(out, *target, args, indent, cx);
            emit_arc_ops(out, &arc.post, indent, cx, "term-post", block.as_u32());
            emit_edge(out, edge(0), layout, indent, cx);
        }
        CfgTerminator::Branch { cond, .. } => {
            let cond = emit_expr(*cond, cx);
            let temp = cx.fresh("cond");
            writeln!(out, "{pad}CieloValue {temp} = {cond};").expect("in-memory write");
            emit_arc_ops(out, &arc.post, indent, cx, "term-post", block.as_u32());
            writeln!(out, "{pad}if (cv_truthy({temp})) {{").expect("in-memory write");
            emit_edge(out, edge(0), layout, indent + 1, cx);
            writeln!(out, "{pad}}} else {{").expect("in-memory write");
            emit_edge(out, edge(1), layout, indent + 1, cx);
            writeln!(out, "{pad}}}").expect("in-memory write");
        }
        CfgTerminator::Match {
            scrutinee, arms, ..
        } => {
            let match_temp = cx.fresh("match");
            let scrutinee_expr = emit_expr(*scrutinee, cx);
            writeln!(out, "{pad}CieloValue {match_temp} = {scrutinee_expr};")
                .expect("in-memory write");
            for (idx, arm) in arms.iter().enumerate() {
                let keyword = if idx == 0 { "if" } else { "} else if" };
                writeln!(
                    out,
                    "{pad}{keyword} (cielo_ctor_is_variant({match_temp}, {}u)) {{ /* {} */",
                    arm.tag.as_u32(),
                    escape(&symbol_text(cx.interner, arm.tag))
                )
                .expect("in-memory write");
                // The binders are the arm target's block parameters, assigned
                // here by name rather than passed as edge arguments (CIELO-48).
                for (field, (binder, mode)) in
                    arm.binders.iter().zip(arm.projections.iter()).enumerate()
                {
                    let getter = match mode {
                        CfgProjectionMode::Move => "cielo_ctor_take_field",
                        CfgProjectionMode::MoveUnique => "cielo_ctor_take_field_unique",
                        CfgProjectionMode::Borrow | CfgProjectionMode::Copy => "cielo_ctor_field",
                    };
                    writeln!(
                        out,
                        "{nested}v{} = {getter}({match_temp}, {field});",
                        binder.as_u32()
                    )
                    .expect("in-memory write");
                    if *mode == CfgProjectionMode::Copy {
                        writeln!(out, "{nested}cielo_arc_retain(v{});", binder.as_u32())
                            .expect("in-memory write");
                    }
                }
                emit_arc_ops(out, &arc.post, indent + 1, cx, "term-post", block.as_u32());
                emit_edge(out, edge(idx), layout, indent + 1, cx);
            }
            if arms.is_empty() {
                writeln!(out, "{pad}{{").expect("in-memory write");
            } else {
                writeln!(out, "{pad}}} else {{").expect("in-memory write");
            }
            emit_arc_ops(out, &arc.post, indent + 1, cx, "term-post", block.as_u32());
            emit_edge(out, edge(arms.len()), layout, indent + 1, cx);
            writeln!(out, "{pad}}}").expect("in-memory write");
        }
        CfgTerminator::Switch {
            selector, targets, ..
        } => {
            let temp = cx.fresh("switch");
            let selector = emit_expr(*selector, cx);
            writeln!(out, "{pad}CieloValue {temp} = {selector};").expect("in-memory write");
            emit_arc_ops(out, &arc.post, indent, cx, "term-post", block.as_u32());
            // A dense case list lets the C compiler pick a jump table. Every
            // arm is planned with no fallthrough, so each case body ends in a
            // jump and none can run into the next.
            writeln!(out, "{pad}switch (cv_switch_index({temp})) {{").expect("in-memory write");
            for index in 0..targets.len() {
                writeln!(out, "{nested}case {index}: {{").expect("in-memory write");
                emit_edge(out, edge(index), layout, indent + 2, cx);
                writeln!(out, "{nested}}}").expect("in-memory write");
            }
            writeln!(out, "{nested}default: {{").expect("in-memory write");
            emit_edge(out, edge(targets.len()), layout, indent + 2, cx);
            writeln!(out, "{nested}}}").expect("in-memory write");
            writeln!(out, "{pad}}}").expect("in-memory write");
        }
        CfgTerminator::Call {
            convention,
            callee_fn,
            args,
            result,
            ..
        } => {
            let args = materialize_args(out, args, indent, cx);
            let callee = cx
                .names
                .get(callee_fn)
                .cloned()
                .unwrap_or_else(|| format!("cielo_missing_fn_{}", callee_fn.as_u32()));
            let call = format!(
                "{}({callee}({}))",
                call_wrapper(*convention),
                args.join(", ")
            );
            if cx.tails.contains(&block) {
                writeln!(out, "{pad}return {call};").expect("in-memory write");
            } else {
                writeln!(out, "{pad}v{} = {call};", result.as_u32()).expect("in-memory write");
                emit_arc_ops(out, &arc.post, indent, cx, "term-post", block.as_u32());
                emit_edge(out, edge(0), layout, indent, cx);
            }
        }
        CfgTerminator::Perform {
            effect,
            operation,
            args,
            result,
            ..
        } => {
            let args = materialize_args(out, args, indent, cx);
            let array = if args.is_empty() {
                "NULL".to_owned()
            } else {
                format!("(CieloValue[]){{{}}}", args.join(", "))
            };
            let call = if let Some(handler) = cx
                .active_handlers
                .get(&block)
                .and_then(|handlers| {
                    handlers
                        .iter()
                        .rev()
                        .find(|(_, active_effect)| active_effect == effect)
                })
                .map(|(handler, _)| *handler)
            {
                format!(
                    "cielo_perform_scoped({}, hcap{}, {}, \"{}\", {}, {array})",
                    effect.as_u32(),
                    handler.as_u32(),
                    operation.as_u32(),
                    escape(&symbol_text(cx.interner, *operation)),
                    args.len()
                )
            } else {
                format!(
                    "cielo_perform({}, {}, \"{}\", {}, {array})",
                    effect.as_u32(),
                    operation.as_u32(),
                    escape(&symbol_text(cx.interner, *operation)),
                    args.len()
                )
            };
            if let Some(result) = result {
                writeln!(out, "{pad}v{} = {call};", result.as_u32()).expect("in-memory write");
            } else {
                writeln!(out, "{pad}(void){call};").expect("in-memory write");
            }
            emit_arc_ops(out, &arc.post, indent, cx, "term-post", block.as_u32());
            emit_edge(out, edge(0), layout, indent, cx);
        }
        CfgTerminator::Unreachable => {
            writeln!(out, "{pad}return cv_unit();").expect("in-memory write");
        }
    }
}

fn emit_edge_values(
    out: &mut String,
    target: CfgBlockId,
    args: &[CfgExprId],
    indent: usize,
    cx: &mut EmitCx<'_>,
) {
    let params = &cx.program.block(target).expect("known target").params;
    let pad = pad(indent);
    let mut temps = Vec::new();
    for arg in args {
        let temp = cx.fresh("edge");
        let value = emit_expr(*arg, cx);
        writeln!(out, "{pad}CieloValue {temp} = {value};").expect("in-memory write");
        temps.push(temp);
    }
    for (param, temp) in params.iter().zip(temps) {
        writeln!(out, "{pad}v{} = {temp};", param.as_u32()).expect("in-memory write");
    }
}

fn materialize_args(
    out: &mut String,
    args: &[CfgExprId],
    indent: usize,
    cx: &mut EmitCx<'_>,
) -> Vec<String> {
    let pad = pad(indent);
    args.iter()
        .map(|arg| {
            let temp = cx.fresh("arg");
            let expression = emit_expr(*arg, cx);
            writeln!(out, "{pad}CieloValue {temp} = {expression};").expect("in-memory write");
            temp
        })
        .collect()
}

fn emit_expr(expression: CfgExprId, cx: &mut EmitCx<'_>) -> String {
    match &cx.program.expr(expression).expect("known expression").kind {
        CfgExpr::Value(value) => format!("v{}", value.as_u32()),
        CfgExpr::Literal(literal) => emit_literal(literal, cx.pools),
        CfgExpr::Unary { op, expr } => format!("{}({})", op.c_func(), emit_expr(*expr, cx)),
        CfgExpr::Field { base, index } => {
            format!("cielo_ctor_field_copy({}, {index})", emit_expr(*base, cx))
        }
        CfgExpr::Binary { op, lhs, rhs } => format!(
            "{}({}, {})",
            op.c_func(),
            emit_expr(*lhs, cx),
            emit_expr(*rhs, cx)
        ),
        CfgExpr::PureCall {
            callee_fn, args, ..
        } => {
            let name = cx
                .names
                .get(callee_fn)
                .cloned()
                .unwrap_or_else(|| format!("cielo_missing_fn_{}", callee_fn.as_u32()));
            let args = args
                .iter()
                .map(|arg| emit_expr(*arg, cx))
                .collect::<Vec<_>>();
            format!("CIELO_CALL_PURE({name}({}))", args.join(", "))
        }
        CfgExpr::BuiltinCall { builtin, args } => {
            let args = args
                .iter()
                .map(|arg| emit_expr(*arg, cx))
                .collect::<Vec<_>>();
            let array = if args.is_empty() {
                "NULL".to_owned()
            } else {
                format!("(CieloValue[]){{{}}}", args.join(", "))
            };
            format!("{}({}, {array})", builtin.c_symbol(), args.len())
        }
        CfgExpr::MakeStruct { ty, fields } => emit_ctor(*ty, SymbolId::INVALID, fields, cx),
        CfgExpr::MakeEnum {
            ty,
            variant,
            fields,
        } => emit_ctor(*ty, *variant, fields, cx),
        CfgExpr::MakeClosure {
            callee,
            callee_fn,
            captures,
        } => {
            let arity = cx
                .program
                .functions
                .get(callee_fn.index())
                .map(|function| function.params.len().saturating_sub(captures.len()))
                .unwrap_or(0);
            let adapter = closure_adapter_name(*callee_fn);
            let count = captures.len();
            let captures = captures
                .iter()
                .map(|capture| emit_expr(*capture, cx))
                .collect::<Vec<_>>();
            let array = if captures.is_empty() {
                "NULL".to_owned()
            } else {
                format!("(CieloValue[]){{{}}}", captures.join(", "))
            };
            format!(
                "cielo_make_closure(\"{}\", {adapter}, {arity}u, {count}, {array})",
                escape(&symbol_text(cx.interner, *callee))
            )
        }
        CfgExpr::CallClosure { callee, args } => {
            let callee = emit_expr(*callee, cx);
            let args = args
                .iter()
                .map(|arg| emit_expr(*arg, cx))
                .collect::<Vec<_>>();
            let array = if args.is_empty() {
                "NULL".to_owned()
            } else {
                format!("(CieloValue[]){{{}}}", args.join(", "))
            };
            format!("cielo_closure_call({callee}, {}, {array})", args.len())
        }
        CfgExpr::Error => "cv_unit()".to_owned(),
    }
}

fn emit_ctor(ty: SymbolId, variant: SymbolId, fields: &[CfgExprId], cx: &mut EmitCx<'_>) -> String {
    if let Some(key) = ctor_literal_key(cx.program, ty, variant, fields)
        && let Some(symbol) = cx.pools.ctor(&key)
    {
        return symbol.to_owned();
    }
    let fields = fields
        .iter()
        .map(|field| emit_expr(*field, cx))
        .collect::<Vec<_>>();
    let array = if fields.is_empty() {
        "NULL".to_owned()
    } else {
        format!("(CieloValue[]){{{}}}", fields.join(", "))
    };
    format!(
        "cielo_make_ctor(\"{}\", \"{}\", {}u, {}, {array})",
        escape(&symbol_text(cx.interner, ty)),
        if variant.is_valid() {
            escape(&symbol_text(cx.interner, variant))
        } else {
            String::new()
        },
        variant.as_u32(),
        fields.len()
    )
}

fn emit_literal(literal: &Literal, pools: &CConstantPools) -> String {
    match literal {
        Literal::Unit => "cv_unit()".to_owned(),
        Literal::Bool(value) => pools
            .scalar(ScalarLiteralKey::Bool(*value))
            .map(str::to_owned)
            .unwrap_or_else(|| format!("cv_bool({})", i32::from(*value))),
        Literal::Int(value) => pools
            .scalar(ScalarLiteralKey::Int(*value))
            .map(str::to_owned)
            .unwrap_or_else(|| format!("cv_int({value})")),
        Literal::Float(value) => pools
            .scalar(ScalarLiteralKey::Float(value.to_bits()))
            .map(str::to_owned)
            .unwrap_or_else(|| format!("cv_float({})", float_literal(*value))),
        Literal::Char(value) => pools
            .scalar(ScalarLiteralKey::Char(*value))
            .map(str::to_owned)
            .unwrap_or_else(|| format!("cv_char({}u)", *value as u32)),
        // `emit` pools every string in the program, so the lookup cannot miss.
        // There is no inline fallback: a CieloValue names a CieloStr, and only
        // static storage can back one.
        Literal::String(value) => pools
            .string(value)
            .expect("string literal was pooled")
            .to_owned(),
    }
}

fn emit_arc_ops(
    out: &mut String,
    ops: &[CfgArcOp],
    indent: usize,
    cx: &EmitCx<'_>,
    placement: &str,
    site: u32,
) {
    for op in ops {
        let name = match op.kind {
            CfgArcOpKind::Retain => "retain",
            CfgArcOpKind::Release => "release",
        };
        if cx.arc_trace {
            writeln!(
                out,
                "{}/* arc {placement} b{site} v{} {name} */",
                "    ".repeat(indent),
                op.value.as_u32()
            )
            .expect("in-memory write");
        }
        writeln!(
            out,
            "{}cielo_arc_{name}(v{});",
            "    ".repeat(indent),
            op.value.as_u32()
        )
        .expect("in-memory write");
    }
}

fn call_wrapper(convention: CfgCallConvention) -> &'static str {
    match convention {
        CfgCallConvention::Pure => "CIELO_CALL_PURE",
        CfgCallConvention::Direct => "CIELO_CALL_DIRECT",
        CfgCallConvention::Control => "CIELO_CALL_CONTROL",
    }
}

/// A tail-call block returns instead of reaching its successor, so a block only
/// that block reached is neither emitted nor allowed to declare locals here.
fn reachable_blocks(
    program: &CfgProgram,
    entry: CfgBlockId,
    tails: &HashSet<CfgBlockId>,
) -> Vec<CfgBlockId> {
    let mut stack = vec![entry];
    let mut seen = HashSet::new();
    while let Some(block) = stack.pop() {
        if seen.insert(block)
            && !tails.contains(&block)
            && let Some(block) = program.block(block)
        {
            stack.extend(block.terminator.successors());
        }
    }
    let mut blocks = seen.into_iter().collect::<Vec<_>>();
    blocks.sort_by_key(|block| block.index());
    blocks
}

fn reachable_handlers(
    program: &CfgProgram,
    entry: CfgBlockId,
    tails: &HashSet<CfgBlockId>,
) -> Vec<CfgHandlerId> {
    let mut handlers = HashSet::new();
    for block in reachable_blocks(program, entry, tails) {
        for instruction in &program.block(block).expect("known block").instructions {
            match program.instruction(*instruction).map(|node| &node.kind) {
                Some(CfgInstruction::HandlerEnter { handler, .. })
                | Some(CfgInstruction::HandlerExit { handler, .. }) => {
                    handlers.insert(*handler);
                }
                _ => {}
            }
        }
    }
    let mut handlers = handlers.into_iter().collect::<Vec<_>>();
    handlers.sort_by_key(|handler| handler.index());
    handlers
}

/// A region whose every slot is stack-placed has no arena, so opening it would
/// emit a `CieloRegion` local that nothing ever allocates from.
fn emit_region_open(
    out: &mut String,
    program: &CfgProgram,
    region: CfgRegionId,
    placements: &HashMap<CfgHandlerId, Placement>,
    indent: usize,
) {
    let Some(node) = program.region(region) else {
        return;
    };
    if node.is_fully_stack() {
        return;
    }
    let pad = pad(indent);
    writeln!(out, "{pad}cielo_region_open(&reg{});", region.as_u32()).expect("in-memory write");
    let RegionOwner::Handler { handler, .. } = node.owner;
    for slot in &node.slots {
        let RegionSlotKind::HandlerEvidence { .. } = slot.kind;
        if placements.get(&handler).copied().unwrap_or_default() == Placement::Stack {
            continue;
        }
        writeln!(
            out,
            "{pad}hev{} = (CieloEvidence *)cielo_region_alloc(&reg{}, sizeof(CieloEvidence));",
            handler.as_u32(),
            region.as_u32()
        )
        .expect("in-memory write");
    }
}

fn reachable_regions(
    program: &CfgProgram,
    entry: CfgBlockId,
    tails: &HashSet<CfgBlockId>,
) -> Vec<CfgRegionId> {
    let mut regions = HashSet::new();
    for block in reachable_blocks(program, entry, tails) {
        for instruction in &program.block(block).expect("known block").instructions {
            match program.instruction(*instruction).map(|node| &node.kind) {
                Some(CfgInstruction::RegionEnter { region })
                | Some(CfgInstruction::RegionExit { region }) => {
                    regions.insert(*region);
                }
                _ => {}
            }
        }
    }
    let mut regions = regions.into_iter().collect::<Vec<_>>();
    regions.sort_by_key(|region| region.index());
    regions
}

/// Placement per handler, resolved through the region the handler owns. A
/// handler with no region predates the region substrate or came from a
/// hand-built CFG, and gets the safe answer rather than a stack slot.
fn evidence_placements(program: &CfgProgram) -> HashMap<CfgHandlerId, Placement> {
    let mut placements = HashMap::new();
    for region in program.regions() {
        let RegionOwner::Handler { handler, .. } = region.owner;
        for slot in &region.slots {
            let RegionSlotKind::HandlerEvidence { .. } = slot.kind;
            placements.insert(handler, slot.placement);
        }
    }
    placements
}

fn reachable_values(
    program: &CfgProgram,
    entry: CfgBlockId,
    tails: &HashSet<CfgBlockId>,
) -> Vec<CfgValueId> {
    let mut values = HashSet::new();
    for block_id in reachable_blocks(program, entry, tails) {
        let block = program.block(block_id).expect("known block");
        values.extend(block.params.iter().copied());
        values.extend(block.entry_arc.iter().map(|op| op.value));
        values.extend(block.terminator_arc.pre.iter().map(|op| op.value));
        values.extend(block.terminator_arc.post.iter().map(|op| op.value));
        for instruction in &block.instructions {
            let instruction = program
                .instruction(*instruction)
                .expect("known instruction");
            values.extend(instruction.arc.pre.iter().map(|op| op.value));
            values.extend(instruction.arc.post.iter().map(|op| op.value));
            values.extend(instruction.kind.result());
            for operand in instruction.kind.child_exprs() {
                collect_expr_values(program, operand, &mut values);
            }
        }
        // A tail call returns its result instead of naming it, so declaring the
        // name would leave an unused local behind.
        if !tails.contains(&block_id) {
            values.extend(block.terminator.defined_values());
        }
        for operand in block.terminator.child_exprs() {
            collect_expr_values(program, operand, &mut values);
        }
    }
    let mut values = values.into_iter().collect::<Vec<_>>();
    values.sort_by_key(|value| value.index());
    values
}

fn collect_expr_values(
    program: &CfgProgram,
    expression: CfgExprId,
    values: &mut HashSet<CfgValueId>,
) {
    walk_exprs(program, expression, &mut |_, node| {
        if let CfgExpr::Value(value) = &node.kind {
            values.insert(*value);
        }
        Walk::Descend
    });
}

fn active_handler_states(
    program: &CfgProgram,
    entry: CfgBlockId,
    tails: &HashSet<CfgBlockId>,
) -> HashMap<CfgBlockId, Vec<(CfgHandlerId, EffectLabelId)>> {
    let mut entry_states = HashMap::new();
    entry_states.insert(entry, Vec::new());
    let mut terminator_states = HashMap::new();
    let mut queue = VecDeque::from([entry]);
    while let Some(block_id) = queue.pop_front() {
        let mut state = entry_states.get(&block_id).cloned().unwrap_or_default();
        let Some(block) = program.block(block_id) else {
            continue;
        };
        for instruction in &block.instructions {
            match program.instruction(*instruction).map(|node| &node.kind) {
                Some(CfgInstruction::HandlerEnter { handler, effect }) => {
                    state.push((*handler, *effect));
                }
                Some(CfgInstruction::HandlerExit { handler, .. }) => {
                    if let Some(index) = state.iter().rposition(|(active, _)| active == handler) {
                        state.remove(index);
                    }
                }
                _ => {}
            }
        }
        terminator_states.insert(block_id, state.clone());
        if tails.contains(&block_id) {
            continue;
        }
        for successor in block.terminator.successors() {
            match entry_states.get(&successor) {
                None => {
                    entry_states.insert(successor, state.clone());
                    queue.push_back(successor);
                }
                Some(existing) if existing != &state => {
                    // A valid structured handler scope has one stack per block.
                    // Keep the common prefix on malformed/merged input so codegen
                    // remains conservative instead of selecting a wrong handler.
                    let common = existing
                        .iter()
                        .zip(&state)
                        .take_while(|(lhs, rhs)| lhs == rhs)
                        .map(|(value, _)| *value)
                        .collect::<Vec<_>>();
                    if &common != existing {
                        entry_states.insert(successor, common);
                        queue.push_back(successor);
                    }
                }
                _ => {}
            }
        }
    }
    terminator_states
}

/// Maps the operation symbols this program performs onto builtins, so an
/// effect operation that reaches the end of the handler chain still resolves.
/// Direct `BuiltinCall`s bypass this table entirely.
fn emit_builtin_table(out: &mut String, program: &CfgProgram, interner: &Interner) {
    let mut entries = program
        .blocks()
        .iter()
        .filter_map(|block| match &block.terminator {
            CfgTerminator::Perform { operation, .. } => interner
                .resolve(*operation)
                .and_then(Builtin::from_name)
                .map(|builtin| (operation.as_u32(), builtin)),
            _ => None,
        })
        .collect::<Vec<_>>();
    entries.sort_unstable();
    entries.dedup();
    if entries.is_empty() {
        return;
    }
    out.push_str("#define CIELO_BUILTIN_TABLE(X)");
    for (symbol, builtin) in entries {
        write!(out, " \\\n    X({symbol}u, {})", builtin.c_symbol()).expect("in-memory write");
    }
    out.push('\n');
}

fn emit_main(out: &mut String, program: &CfgProgram, names: &HashMap<CfgFuncId, String>) {
    let Some(entry) = program.entrypoints.first().copied() else {
        return;
    };
    let Some(function) = program.functions.get(entry.index()) else {
        return;
    };
    let Some(name) = names.get(&function.id) else {
        return;
    };
    out.push_str("int main(void) {\n");
    writeln!(out, "    CieloValue _entry = {name}();").expect("in-memory write");
    out.push_str("    if (_entry.tag == CV_INT) return (int)_entry.as.i;\n");
    out.push_str("    if (_entry.tag == CV_BOOL) return _entry.as.b ? 0 : 1;\n");
    out.push_str("    return 0;\n}\n");
}

fn symbol_text(interner: &Interner, symbol: SymbolId) -> String {
    interner.resolve(symbol).unwrap_or("unknown").to_owned()
}

fn sanitize(symbol: String) -> String {
    let mut out = String::new();
    for ch in symbol.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() || out.as_bytes()[0].is_ascii_digit() {
        out.insert(0, '_');
    }
    out
}

fn escape(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_ascii_graphic() || c == ' ' => out.push(c),
            c => write!(out, "\\x{:02X}", c as u32).expect("in-memory write"),
        }
    }
    out
}

fn float_literal(value: f64) -> String {
    if value.is_nan() {
        "(0.0 / 0.0)".to_owned()
    } else if value == f64::INFINITY {
        "(1.0 / 0.0)".to_owned()
    } else if value == f64::NEG_INFINITY {
        "(-1.0 / 0.0)".to_owned()
    } else {
        let mut value = format!("{value:?}");
        if !value.contains('.') && !value.contains('e') && !value.contains('E') {
            value.push_str(".0");
        }
        value
    }
}

struct EmitCx<'a> {
    program: &'a CfgProgram,
    names: &'a HashMap<CfgFuncId, String>,
    tails: &'a HashSet<CfgBlockId>,
    interner: &'a Interner,
    pools: &'a CConstantPools,
    active_handlers: HashMap<CfgBlockId, Vec<(CfgHandlerId, EffectLabelId)>>,
    placements: HashMap<CfgHandlerId, Placement>,
    arc_trace: bool,
    temp: u32,
}

impl EmitCx<'_> {
    fn fresh(&mut self, prefix: &str) -> String {
        let id = self.temp;
        self.temp += 1;
        format!("__cielo_{prefix}_{id}")
    }
}
