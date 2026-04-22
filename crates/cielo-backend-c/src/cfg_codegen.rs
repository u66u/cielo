//! Direct CFG-to-C lowering.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write;

use crate::c_constants::CConstantPools;
use cielo_base::ids::{CfgBlockId, CfgExprId, CfgHandlerId, CfgValueId, EffectLabelId, SymbolId};
use cielo_base::symbols::Interner;
use cielo_ir::cfg::{
    CfgArcOp, CfgArcOpKind, CfgCallConvention, CfgExpr, CfgFunction, CfgInstruction, CfgProgram,
    CfgProjectionMode, CfgTerminator,
};
use cielo_ir::constants::{ConstantTable, CtorFieldKey, CtorLiteralKey, ScalarLiteralKey};
use cielo_ir::core::Literal;

const C_RUNTIME_HEADER: &str = include_str!("cielo_runtime.h");
const BUILTIN_PRINT_OP_NAME: &str = "print";

pub fn emit(
    program: &CfgProgram,
    interner: &Interner,
    constants: &ConstantTable,
    arc_trace: bool,
) -> String {
    let mut out = String::new();
    if let Some(symbol) = builtin_print_symbol(program, interner) {
        writeln!(out, "#define CIELO_OP_SYMBOL_PRINT {}u", symbol.as_u32())
            .expect("in-memory write");
    }
    out.push_str(C_RUNTIME_HEADER);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    let pools = CConstantPools::build(constants, interner);
    out.push_str(&pools.declarations);
    if !pools.declarations.is_empty() {
        out.push('\n');
    }

    let names = function_names(program, interner);
    for function in &program.functions {
        emit_signature(&mut out, function, &names[&function.name]);
        out.push_str(";\n");
    }
    out.push('\n');
    for function in &program.functions {
        emit_function(
            &mut out, program, function, &names, interner, &pools, arc_trace,
        );
        out.push('\n');
    }
    emit_main(&mut out, program, &names);
    out
}

fn function_names(program: &CfgProgram, interner: &Interner) -> HashMap<SymbolId, String> {
    program
        .functions
        .iter()
        .map(|function| {
            (
                function.name,
                format!(
                    "cielo_fn_{}_{}",
                    sanitize(symbol_text(interner, function.name)),
                    function.id.as_u32()
                ),
            )
        })
        .collect()
}

fn emit_signature(out: &mut String, function: &CfgFunction, name: &str) {
    write!(out, "static CieloValue {name}(").expect("in-memory write");
    for (idx, value) in function.params.iter().enumerate() {
        if idx > 0 {
            out.push_str(", ");
        }
        write!(out, "CieloValue v{}", value.as_u32()).expect("in-memory write");
    }
    out.push(')');
}

fn emit_function(
    out: &mut String,
    program: &CfgProgram,
    function: &CfgFunction,
    names: &HashMap<SymbolId, String>,
    interner: &Interner,
    pools: &CConstantPools,
    arc_trace: bool,
) {
    emit_signature(out, function, &names[&function.name]);
    out.push_str(" {\n");
    let params = function.params.iter().copied().collect::<HashSet<_>>();
    for value in reachable_values(program, function.entry) {
        if !params.contains(&value) {
            writeln!(out, "    CieloValue v{} = cv_unit();", value.as_u32())
                .expect("in-memory write");
        }
    }
    let handlers = reachable_handlers(program, function.entry);
    let active_handlers = active_handler_states(program, function.entry);
    for handler in handlers {
        writeln!(out, "    uint32_t hcap{} = 0;", handler.as_u32()).expect("in-memory write");
        writeln!(out, "    CieloEvidence hev{};", handler.as_u32()).expect("in-memory write");
    }
    writeln!(out, "    goto b{};", function.entry.as_u32()).expect("in-memory write");

    let blocks = reachable_blocks(program, function.entry);
    let mut cx = EmitCx {
        program,
        names,
        interner,
        pools,
        active_handlers,
        arc_trace,
        temp: 0,
    };
    for block_id in blocks {
        let block = program.block(block_id).expect("known block");
        writeln!(out, "b{}: ;", block.id.as_u32()).expect("in-memory write");
        emit_arc_ops(out, &block.entry_arc, 1, &cx, "entry", block.id.as_u32());
        for instruction_id in &block.instructions {
            let instruction = program
                .instruction(*instruction_id)
                .expect("known instruction");
            emit_arc_ops(
                out,
                &instruction.arc.pre,
                1,
                &cx,
                "pre",
                instruction.id.as_u32(),
            );
            match &instruction.kind {
                CfgInstruction::Let { result, value } | CfgInstruction::Eval { result, value } => {
                    let expression = emit_expr(*value, &mut cx);
                    writeln!(out, "    v{} = {expression};", result.as_u32())
                        .expect("in-memory write");
                }
                CfgInstruction::HandlerEnter { handler, effect } => {
                    writeln!(
                        out,
                        "    hev{} = (CieloEvidence){{ .abi_version = cielo_runtime_abi_version(), .effect = {}, .capability_id = 0, .clause_count = 0, .clauses = NULL, .captures = NULL, .reserved0 = NULL, .reserved1 = NULL }};",
                        handler.as_u32(),
                        effect.as_u32()
                    )
                    .expect("in-memory write");
                    writeln!(
                        out,
                        "    hcap{} = cielo_handler_push_with_evidence({}, &hev{});",
                        handler.as_u32(),
                        effect.as_u32(),
                        handler.as_u32()
                    )
                    .expect("in-memory write");
                }
                CfgInstruction::HandlerExit { handler, .. } => {
                    writeln!(out, "    cielo_handler_pop(hcap{});", handler.as_u32())
                        .expect("in-memory write");
                }
                CfgInstruction::StageEnter { stage } => {
                    writeln!(out, "    /* stage {:?} enter */", stage).expect("in-memory write");
                }
                CfgInstruction::StageExit { stage } => {
                    writeln!(out, "    /* stage {:?} exit */", stage).expect("in-memory write");
                }
                CfgInstruction::Hole => out.push_str("    /* hole */\n"),
                CfgInstruction::Error => out.push_str("    /* error */\n"),
            }
            emit_arc_ops(
                out,
                &instruction.arc.post,
                1,
                &cx,
                "post",
                instruction.id.as_u32(),
            );
        }
        emit_terminator(
            out,
            block.id,
            &block.terminator,
            &block.terminator_arc,
            &mut cx,
        );
    }
    out.push_str("}\n");
}

fn emit_terminator(
    out: &mut String,
    block: CfgBlockId,
    terminator: &CfgTerminator,
    arc: &cielo_ir::cfg::CfgArcOps,
    cx: &mut EmitCx<'_>,
) {
    emit_arc_ops(out, &arc.pre, 1, cx, "term-pre", block.as_u32());
    match terminator {
        CfgTerminator::Return(value) => {
            let temp = cx.fresh("return");
            let value = emit_expr(*value, cx);
            writeln!(out, "    CieloValue {temp} = {value};").expect("in-memory write");
            emit_arc_ops(out, &arc.post, 1, cx, "term-post", block.as_u32());
            writeln!(out, "    return {temp};").expect("in-memory write");
        }
        CfgTerminator::Goto { target, args } => {
            emit_edge_values(out, *target, args, cx);
            emit_arc_ops(out, &arc.post, 1, cx, "term-post", block.as_u32());
            writeln!(out, "    goto b{};", target.as_u32()).expect("in-memory write");
        }
        CfgTerminator::Branch {
            cond,
            then_target,
            else_target,
        } => {
            let cond = emit_expr(*cond, cx);
            let temp = cx.fresh("cond");
            writeln!(out, "    CieloValue {temp} = {cond};").expect("in-memory write");
            emit_arc_ops(out, &arc.post, 1, cx, "term-post", block.as_u32());
            writeln!(
                out,
                "    if (cv_truthy({temp})) goto b{}; else goto b{};",
                then_target.as_u32(),
                else_target.as_u32()
            )
            .expect("in-memory write");
        }
        CfgTerminator::Match {
            scrutinee,
            arms,
            default,
        } => {
            let match_temp = cx.fresh("match");
            let scrutinee_expr = emit_expr(*scrutinee, cx);
            writeln!(out, "    CieloValue {match_temp} = {scrutinee_expr};")
                .expect("in-memory write");
            for (idx, arm) in arms.iter().enumerate() {
                let keyword = if idx == 0 { "if" } else { "else if" };
                writeln!(
                    out,
                    "    {keyword} (cielo_ctor_is_variant({match_temp}, {}u)) {{ /* {} */",
                    arm.tag.as_u32(),
                    escape(&symbol_text(cx.interner, arm.tag))
                )
                .expect("in-memory write");
                for (field, (binder, mode)) in
                    arm.binders.iter().zip(arm.projections.iter()).enumerate()
                {
                    let getter = if *mode == CfgProjectionMode::Move {
                        "cielo_ctor_take_field"
                    } else {
                        "cielo_ctor_field"
                    };
                    writeln!(
                        out,
                        "        v{} = {getter}({match_temp}, {field});",
                        binder.as_u32()
                    )
                    .expect("in-memory write");
                    if *mode == CfgProjectionMode::Copy {
                        writeln!(out, "        cielo_arc_retain(v{});", binder.as_u32())
                            .expect("in-memory write");
                    }
                }
                emit_arc_ops(out, &arc.post, 2, cx, "term-post", block.as_u32());
                writeln!(out, "        goto b{};", arm.target.as_u32()).expect("in-memory write");
                out.push_str("    }\n");
            }
            if !arms.is_empty() {
                out.push_str("    else {\n");
            } else {
                out.push_str("    {\n");
            }
            emit_arc_ops(out, &arc.post, 2, cx, "term-post", block.as_u32());
            writeln!(out, "        goto b{};", default.as_u32()).expect("in-memory write");
            out.push_str("    }\n");
        }
        CfgTerminator::Call {
            convention,
            callee,
            args,
            result,
            target,
        } => {
            let args = materialize_args(out, args, cx);
            let callee = cx
                .names
                .get(callee)
                .cloned()
                .unwrap_or_else(|| format!("cielo_missing_fn_{}", callee.as_u32()));
            let call = format!(
                "{}({callee}({}))",
                call_wrapper(*convention),
                args.join(", ")
            );
            writeln!(out, "    v{} = {call};", result.as_u32()).expect("in-memory write");
            emit_arc_ops(out, &arc.post, 1, cx, "term-post", block.as_u32());
            writeln!(out, "    goto b{};", target.as_u32()).expect("in-memory write");
        }
        CfgTerminator::Perform {
            effect,
            operation,
            args,
            result,
            target,
        } => {
            let args = materialize_args(out, args, cx);
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
                writeln!(out, "    v{} = {call};", result.as_u32()).expect("in-memory write");
            } else {
                writeln!(out, "    (void){call};").expect("in-memory write");
            }
            emit_arc_ops(out, &arc.post, 1, cx, "term-post", block.as_u32());
            writeln!(out, "    goto b{};", target.as_u32()).expect("in-memory write");
        }
        CfgTerminator::Unreachable => out.push_str("    return cv_unit();\n"),
    }
}

fn emit_edge_values(out: &mut String, target: CfgBlockId, args: &[CfgExprId], cx: &mut EmitCx<'_>) {
    let params = &cx.program.block(target).expect("known target").params;
    let mut temps = Vec::new();
    for arg in args {
        let temp = cx.fresh("edge");
        let value = emit_expr(*arg, cx);
        writeln!(out, "    CieloValue {temp} = {value};").expect("in-memory write");
        temps.push(temp);
    }
    for (param, temp) in params.iter().zip(temps) {
        writeln!(out, "    v{} = {temp};", param.as_u32()).expect("in-memory write");
    }
}

fn materialize_args(out: &mut String, args: &[CfgExprId], cx: &mut EmitCx<'_>) -> Vec<String> {
    args.iter()
        .map(|arg| {
            let temp = cx.fresh("arg");
            let expression = emit_expr(*arg, cx);
            writeln!(out, "    CieloValue {temp} = {expression};").expect("in-memory write");
            temp
        })
        .collect()
}

fn emit_expr(expression: CfgExprId, cx: &mut EmitCx<'_>) -> String {
    match &cx.program.expr(expression).expect("known expression").kind {
        CfgExpr::Value(value) => format!("v{}", value.as_u32()),
        CfgExpr::Literal(literal) => emit_literal(literal, cx.pools),
        CfgExpr::Unary { op, expr } => format!("{}({})", op.c_func(), emit_expr(*expr, cx)),
        CfgExpr::Binary { op, lhs, rhs } => format!(
            "{}({}, {})",
            op.c_func(),
            emit_expr(*lhs, cx),
            emit_expr(*rhs, cx)
        ),
        CfgExpr::PureCall { callee, args } => {
            let name = cx
                .names
                .get(callee)
                .cloned()
                .unwrap_or_else(|| format!("cielo_missing_fn_{}", callee.as_u32()));
            let args = args
                .iter()
                .map(|arg| emit_expr(*arg, cx))
                .collect::<Vec<_>>();
            format!("CIELO_CALL_PURE({name}({}))", args.join(", "))
        }
        CfgExpr::MakeStruct { ty, fields } => emit_ctor(*ty, SymbolId::INVALID, fields, cx),
        CfgExpr::MakeEnum {
            ty,
            variant,
            fields,
        } => emit_ctor(*ty, *variant, fields, cx),
        CfgExpr::Error => "cv_unit()".to_owned(),
    }
}

fn emit_ctor(ty: SymbolId, variant: SymbolId, fields: &[CfgExprId], cx: &mut EmitCx<'_>) -> String {
    if let Some(key) = ctor_key(cx.program, ty, variant, fields)
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
        Literal::String(value) => pools
            .string(value)
            .map(|symbol| format!("cv_string({symbol})"))
            .unwrap_or_else(|| format!("cv_string(\"{}\")", escape(value))),
    }
}

fn ctor_key(
    program: &CfgProgram,
    ty: SymbolId,
    variant: SymbolId,
    fields: &[CfgExprId],
) -> Option<CtorLiteralKey> {
    let fields = fields
        .iter()
        .map(|field| ctor_field_key(program, *field))
        .collect::<Option<Vec<_>>>()?;
    Some(CtorLiteralKey {
        ty,
        variant,
        fields,
    })
}

fn ctor_field_key(program: &CfgProgram, expression: CfgExprId) -> Option<CtorFieldKey> {
    match &program.expr(expression)?.kind {
        CfgExpr::Literal(literal) => CtorFieldKey::from_literal(literal),
        CfgExpr::MakeStruct { ty, fields } => Some(CtorFieldKey::Ctor(Box::new(ctor_key(
            program,
            *ty,
            SymbolId::INVALID,
            fields,
        )?))),
        CfgExpr::MakeEnum {
            ty,
            variant,
            fields,
        } => Some(CtorFieldKey::Ctor(Box::new(ctor_key(
            program, *ty, *variant, fields,
        )?))),
        _ => None,
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

fn reachable_blocks(program: &CfgProgram, entry: CfgBlockId) -> Vec<CfgBlockId> {
    let mut stack = vec![entry];
    let mut seen = HashSet::new();
    while let Some(block) = stack.pop() {
        if seen.insert(block)
            && let Some(block) = program.block(block)
        {
            stack.extend(block.terminator.successors());
        }
    }
    let mut blocks = seen.into_iter().collect::<Vec<_>>();
    blocks.sort_by_key(|block| block.index());
    blocks
}

fn reachable_handlers(program: &CfgProgram, entry: CfgBlockId) -> Vec<CfgHandlerId> {
    let mut handlers = HashSet::new();
    for block in reachable_blocks(program, entry) {
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

fn reachable_values(program: &CfgProgram, entry: CfgBlockId) -> Vec<CfgValueId> {
    let mut values = HashSet::new();
    for block_id in reachable_blocks(program, entry) {
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
            match &instruction.kind {
                CfgInstruction::Let { result, value } | CfgInstruction::Eval { result, value } => {
                    values.insert(*result);
                    collect_expr_values(program, *value, &mut values);
                }
                _ => {}
            }
        }
        match &block.terminator {
            CfgTerminator::Return(value) => collect_expr_values(program, *value, &mut values),
            CfgTerminator::Goto { args, .. }
            | CfgTerminator::Call { args, .. }
            | CfgTerminator::Perform { args, .. } => {
                for arg in args {
                    collect_expr_values(program, *arg, &mut values);
                }
                match &block.terminator {
                    CfgTerminator::Call { result, .. } => {
                        values.insert(*result);
                    }
                    CfgTerminator::Perform {
                        result: Some(result),
                        ..
                    } => {
                        values.insert(*result);
                    }
                    _ => {}
                }
            }
            CfgTerminator::Branch { cond, .. } => collect_expr_values(program, *cond, &mut values),
            CfgTerminator::Match {
                scrutinee, arms, ..
            } => {
                collect_expr_values(program, *scrutinee, &mut values);
                values.extend(arms.iter().flat_map(|arm| arm.binders.iter()).copied());
            }
            CfgTerminator::Unreachable => {}
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
    let Some(expression) = program.expr(expression) else {
        return;
    };
    match &expression.kind {
        CfgExpr::Value(value) => {
            values.insert(*value);
        }
        CfgExpr::Unary { expr, .. } => collect_expr_values(program, *expr, values),
        CfgExpr::Binary { lhs, rhs, .. } => {
            collect_expr_values(program, *lhs, values);
            collect_expr_values(program, *rhs, values);
        }
        CfgExpr::PureCall { args, .. }
        | CfgExpr::MakeStruct { fields: args, .. }
        | CfgExpr::MakeEnum { fields: args, .. } => {
            for arg in args {
                collect_expr_values(program, *arg, values);
            }
        }
        CfgExpr::Literal(_) | CfgExpr::Error => {}
    }
}

fn active_handler_states(
    program: &CfgProgram,
    entry: CfgBlockId,
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

fn builtin_print_symbol(program: &CfgProgram, interner: &Interner) -> Option<SymbolId> {
    program
        .blocks()
        .iter()
        .find_map(|block| match &block.terminator {
            CfgTerminator::Perform { operation, .. }
                if interner.resolve(*operation) == Some(BUILTIN_PRINT_OP_NAME) =>
            {
                Some(*operation)
            }
            _ => None,
        })
}

fn emit_main(out: &mut String, program: &CfgProgram, names: &HashMap<SymbolId, String>) {
    let Some(entry) = program.entrypoints.first().copied() else {
        return;
    };
    let Some(function) = program.functions.get(entry.index()) else {
        return;
    };
    let Some(name) = names.get(&function.name) else {
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
    names: &'a HashMap<SymbolId, String>,
    interner: &'a Interner,
    pools: &'a CConstantPools,
    active_handlers: HashMap<CfgBlockId, Vec<(CfgHandlerId, EffectLabelId)>>,
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
