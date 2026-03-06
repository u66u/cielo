// Pass 9/9: c_emit (linear runtime IR -> C source)
//
// Inputs:
// - LinearProgram
// - Interner (symbol names)
//
// Outputs:
// - C translation unit as UTF-8 source text
//
// Invariants:
// - Every linear function emits one C function with stable naming
// - All IR variables map to stack locals (`CieloValue vN`)
//
// Diagnostics:
// - None in v0 (unsupported constructs degrade to comments + unit values)
//
// Complexity:
// - O(total linear nodes + emitted text size)

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write;

use crate::common::gc::GcConfig;
use crate::common::ids::{EffectLabelId, LinearExprId, LinearStmtId, SymbolId, VarId};
use crate::common::symbols::Interner;
use crate::ir::core::Literal;
use crate::ir::linear::{CallConvention, LinearExpr, LinearFunction, LinearProgram, LinearStmt};
use crate::passes::arc_emit::ArcEmitPlan;
use crate::passes::arc_verify;
use crate::passes::constant_table;
use crate::passes::linearize::Linearized;
use crate::pipeline::phases::{
    ConstantEmbedStrategy, ConstantKey, ConstantTable, CtorFieldKey, CtorLiteralKey,
    ScalarLiteralKey,
};

const C_RUNTIME_HEADER: &str = include_str!("../backend/cielo_runtime.h");
const BUILTIN_PRINT_OP_NAME: &str = "print";
const C_PRELUDE_PRINT_OP_SYMBOL_MACRO: &str = "CIELO_OP_SYMBOL_PRINT";

#[derive(Clone, Debug)]
pub struct EmittedC {
    pub linearized: Linearized,
    pub c_source: String,
}

pub fn run(linearized: Linearized, interner: &Interner) -> EmittedC {
    run_with_gc_config(linearized, interner, &GcConfig::default())
}

pub fn run_with_gc_config(
    mut linearized: Linearized,
    interner: &Interner,
    gc: &GcConfig,
) -> EmittedC {
    let sema = linearized.residual.sema().clone();
    let arc_plan = if gc.arc_emission_enabled() {
        ArcEmitPlan::build(&linearized.linear)
    } else {
        ArcEmitPlan::disabled()
    };
    if gc.arc_verify_enabled() {
        let (_, diagnostics) = linearized.residual.program_and_diagnostics_mut();
        let _verify_stats = arc_verify::verify(&linearized.linear, &sema, &arc_plan, diagnostics);
    }
    let c_source = emit_c_program_with_constant_table(
        &linearized.linear,
        interner,
        &linearized.residual.residual().constant_table,
        &arc_plan,
        gc.arc_emit_trace_enabled(),
    );
    EmittedC {
        linearized,
        c_source,
    }
}

pub fn emit_c_program(program: &LinearProgram, interner: &Interner) -> String {
    let table = constant_table::build_for_linear(program);
    let arc_plan = ArcEmitPlan::disabled();
    emit_c_program_with_constant_table(program, interner, &table, &arc_plan, false)
}

fn emit_c_program_with_constant_table(
    program: &LinearProgram,
    interner: &Interner,
    table: &ConstantTable,
    arc_plan: &ArcEmitPlan,
    arc_emit_trace: bool,
) -> String {
    let mut out = String::new();
    emit_runtime_prelude(&mut out, program, interner);
    out.push_str(C_RUNTIME_HEADER);
    if !out.ends_with('\n') {
        out.push('\n');
    }

    let scalar_pool = build_scalar_const_pool(table);
    emit_scalar_const_pool(&mut out, &scalar_pool);
    if !scalar_pool.entries.is_empty() {
        out.push('\n');
    }

    let string_pool = build_string_const_pool(table);
    emit_string_const_pool(&mut out, &string_pool);
    if !string_pool.entries.is_empty() {
        out.push('\n');
    }

    let ctor_pool = build_ctor_const_pool(table);
    emit_ctor_const_pool(&mut out, &ctor_pool, interner);
    if !ctor_pool.entries.is_empty() {
        out.push('\n');
    }

    let mut fn_name_by_symbol: HashMap<SymbolId, String> = HashMap::new();
    let mut fn_name_by_index: Vec<String> = Vec::with_capacity(program.functions.len());
    for function in &program.functions {
        let c_name = format!(
            "cielo_fn_{}_{}",
            sanitize_symbol(symbol_text(interner, function.name)),
            function.id.as_u32()
        );
        fn_name_by_symbol.insert(function.name, c_name.clone());
        fn_name_by_index.push(c_name);
    }

    for (idx, function) in program.functions.iter().enumerate() {
        let name = &fn_name_by_index[idx];
        emit_fn_signature(&mut out, name, &function.params);
        out.push_str(";\n");
    }
    out.push('\n');

    for (idx, function) in program.functions.iter().enumerate() {
        let name = &fn_name_by_index[idx];
        emit_function(
            &mut out,
            program,
            function,
            name,
            &fn_name_by_symbol,
            interner,
            &string_pool,
            &scalar_pool,
            &ctor_pool,
            arc_plan,
            arc_emit_trace,
        );
        out.push('\n');
    }

    emit_c_main_wrapper(&mut out, program, &fn_name_by_index);
    out
}

fn emit_runtime_prelude(out: &mut String, program: &LinearProgram, interner: &Interner) {
    if let Some(print_symbol) = builtin_print_op_symbol(program, interner) {
        writeln!(out, "#ifndef {}", C_PRELUDE_PRINT_OP_SYMBOL_MACRO)
            .expect("in-memory write should not fail");
        writeln!(
            out,
            "#define {} {}u",
            C_PRELUDE_PRINT_OP_SYMBOL_MACRO,
            print_symbol.as_u32()
        )
        .expect("in-memory write should not fail");
        writeln!(out, "#endif").expect("in-memory write should not fail");
        out.push('\n');
    }
}

fn builtin_print_op_symbol(program: &LinearProgram, interner: &Interner) -> Option<SymbolId> {
    program.stmts().iter().find_map(|stmt| match &stmt.kind {
        LinearStmt::Perform { operation, .. }
            if interner.resolve(*operation) == Some(BUILTIN_PRINT_OP_NAME) =>
        {
            Some(*operation)
        }
        _ => None,
    })
}

fn emit_function(
    out: &mut String,
    program: &LinearProgram,
    function: &LinearFunction,
    c_name: &str,
    fn_name_by_symbol: &HashMap<SymbolId, String>,
    interner: &Interner,
    string_pool: &StringConstPool,
    scalar_pool: &ScalarConstPool,
    ctor_pool: &CtorConstPool,
    arc_plan: &ArcEmitPlan,
    arc_emit_trace: bool,
) {
    emit_fn_signature(out, c_name, &function.params);
    out.push_str(" {\n");

    let mut vars = BTreeSet::new();
    let mut seen_stmts = HashSet::new();
    collect_stmt_vars(program, function.body, &mut vars, &mut seen_stmts);
    for param in &function.params {
        vars.remove(param);
    }
    for var in &vars {
        writeln!(out, "    CieloValue v{} = cv_unit();", var.as_u32())
            .expect("in-memory write should not fail");
    }
    if !vars.is_empty() {
        out.push('\n');
    }

    let mut cx = EmitCx {
        program,
        fn_name_by_symbol,
        interner,
        string_pool,
        scalar_pool,
        ctor_pool,
        arc_plan,
        arc_emit_trace,
        next_temp: 0,
        active_capabilities: Vec::new(),
    };
    emit_stmt(function.body, EmitMode::Return, out, 1, &mut cx);
    out.push_str("}\n");
}

fn emit_fn_signature(out: &mut String, c_name: &str, params: &[VarId]) {
    write!(out, "static CieloValue {}(", c_name).expect("in-memory write should not fail");
    for (idx, param) in params.iter().enumerate() {
        if idx > 0 {
            out.push_str(", ");
        }
        write!(out, "CieloValue v{}", param.as_u32()).expect("in-memory write should not fail");
    }
    out.push(')');
}

fn emit_stmt(
    stmt_id: LinearStmtId,
    mode: EmitMode,
    out: &mut String,
    indent: usize,
    cx: &mut EmitCx<'_>,
) {
    let Some(stmt) = cx.program.stmt(stmt_id) else {
        emit_indent(out, indent);
        out.push_str("/* missing linear stmt */\n");
        emit_leaf(mode, "cv_unit()".to_owned(), out, indent);
        return;
    };

    emit_arc_ops(stmt_id, ArcEmitPlacement::PreRetain, out, indent, cx);

    match &stmt.kind {
        LinearStmt::Return(expr) => {
            let value_expr = emit_expr(*expr, cx);
            let has_post_release = !cx.arc_plan.post_release_vars(stmt_id).is_empty();
            if has_post_release {
                let leaf_temp = cx.fresh_temp("arc_leaf");
                emit_indent(out, indent);
                writeln!(out, "CieloValue {leaf_temp} = {value_expr};")
                    .expect("in-memory write should not fail");
                emit_arc_ops(stmt_id, ArcEmitPlacement::PostRelease, out, indent, cx);
                emit_leaf(mode, leaf_temp, out, indent);
            } else {
                emit_leaf(mode, value_expr, out, indent);
            }
        }
        LinearStmt::Let {
            binding,
            value,
            next,
        } => {
            emit_indent(out, indent);
            writeln!(out, "v{} = {};", binding.as_u32(), emit_expr(*value, cx))
                .expect("in-memory write should not fail");
            emit_arc_ops(stmt_id, ArcEmitPlacement::PostRelease, out, indent, cx);
            emit_stmt(*next, mode, out, indent, cx);
        }
        LinearStmt::Val {
            binding,
            value,
            next,
        } => {
            emit_stmt(*value, EmitMode::AssignVar(*binding), out, indent, cx);
            emit_arc_ops(stmt_id, ArcEmitPlacement::PostRelease, out, indent, cx);
            emit_stmt(*next, mode, out, indent, cx);
        }
        LinearStmt::PureCall {
            result,
            callee,
            args,
            next,
        } => {
            emit_lowered_call(
                stmt_id,
                *result,
                *callee,
                args,
                *next,
                CallConvention::Pure,
                mode,
                out,
                indent,
                cx,
            );
        }
        LinearStmt::DirectCall {
            result,
            callee,
            args,
            next,
        } => {
            emit_lowered_call(
                stmt_id,
                *result,
                *callee,
                args,
                *next,
                CallConvention::Direct,
                mode,
                out,
                indent,
                cx,
            );
        }
        LinearStmt::ControlCall {
            result,
            callee,
            args,
            next,
        } => {
            emit_lowered_call(
                stmt_id,
                *result,
                *callee,
                args,
                *next,
                CallConvention::Control,
                mode,
                out,
                indent,
                cx,
            );
        }
        LinearStmt::If {
            cond,
            then_branch,
            else_branch,
        } => {
            emit_indent(out, indent);
            writeln!(out, "if (cv_truthy({})) {{", emit_expr(*cond, cx))
                .expect("in-memory write should not fail");
            emit_arc_ops(stmt_id, ArcEmitPlacement::PostRelease, out, indent + 1, cx);
            emit_stmt(*then_branch, mode.clone(), out, indent + 1, cx);
            emit_indent(out, indent);
            out.push_str("} else {\n");
            emit_arc_ops(stmt_id, ArcEmitPlacement::PostRelease, out, indent + 1, cx);
            emit_stmt(*else_branch, mode, out, indent + 1, cx);
            emit_indent(out, indent);
            out.push_str("}\n");
        }
        LinearStmt::Match {
            scrutinee,
            arms,
            default,
        } => {
            let match_value = cx.fresh_temp("match");
            emit_indent(out, indent);
            writeln!(
                out,
                "CieloValue {match_value} = {};",
                emit_expr(*scrutinee, cx)
            )
            .expect("in-memory write should not fail");
            for (idx, arm) in arms.iter().enumerate() {
                let prefix = if idx == 0 { "if" } else { "else if" };
                emit_indent(out, indent);
                writeln!(
                    out,
                    "{} (cielo_ctor_is_variant({}, \"{}\")) {{",
                    prefix,
                    match_value,
                    escape_c_string(symbol_text(cx.interner, arm.tag).as_str())
                )
                .expect("in-memory write should not fail");
                for (field_idx, binder) in arm.binders.iter().enumerate() {
                    emit_indent(out, indent + 1);
                    writeln!(
                        out,
                        "v{} = cielo_ctor_field({}, {});",
                        binder.as_u32(),
                        match_value,
                        field_idx
                    )
                    .expect("in-memory write should not fail");
                }
                emit_arc_ops(stmt_id, ArcEmitPlacement::PostRelease, out, indent + 1, cx);
                emit_stmt(arm.body, mode.clone(), out, indent + 1, cx);
                emit_indent(out, indent);
                out.push_str("}\n");
            }
            if let Some(default_stmt) = default {
                emit_indent(out, indent);
                out.push_str("else {\n");
                emit_arc_ops(stmt_id, ArcEmitPlacement::PostRelease, out, indent + 1, cx);
                emit_stmt(*default_stmt, mode.clone(), out, indent + 1, cx);
                emit_indent(out, indent);
                out.push_str("}\n");
            } else if !arms.is_empty() {
                emit_indent(out, indent);
                out.push_str("else {\n");
                emit_arc_ops(stmt_id, ArcEmitPlacement::PostRelease, out, indent + 1, cx);
                emit_leaf(mode, "cv_unit()".to_owned(), out, indent + 1);
                emit_indent(out, indent);
                out.push_str("}\n");
            } else {
                emit_arc_ops(stmt_id, ArcEmitPlacement::PostRelease, out, indent, cx);
                emit_leaf(mode, "cv_unit()".to_owned(), out, indent);
            }
        }
        LinearStmt::Perform {
            result,
            effect,
            operation,
            args,
            next,
        } => {
            let op_name = escape_c_string(symbol_text(cx.interner, *operation).as_str());
            let active_capability = cx.active_capability_for(*effect).map(str::to_owned);
            emit_indent(out, indent);
            if let Some(dst) = result {
                write!(out, "v{} = ", dst.as_u32()).expect("in-memory write should not fail");
            } else {
                out.push_str("(void)");
            }
            if let Some(capability) = active_capability {
                write!(
                    out,
                    "cielo_perform_scoped({}, {}, {}, \"{}\", {}, ",
                    effect.as_u32(),
                    capability,
                    operation.as_u32(),
                    op_name,
                    args.len()
                )
                .expect("in-memory write should not fail");
            } else {
                write!(
                    out,
                    "cielo_perform({}, {}, \"{}\", {}, ",
                    effect.as_u32(),
                    operation.as_u32(),
                    op_name,
                    args.len()
                )
                .expect("in-memory write should not fail");
            }
            if args.is_empty() {
                out.push_str("NULL");
            } else {
                out.push_str("(CieloValue[]){");
                for (idx, arg) in args.iter().enumerate() {
                    if idx > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&emit_expr(*arg, cx));
                }
                out.push('}');
            }
            out.push_str(");\n");
            emit_arc_ops(stmt_id, ArcEmitPlacement::PostRelease, out, indent, cx);
            emit_stmt(*next, mode, out, indent, cx);
        }
        LinearStmt::Handle { effect, body, next } => {
            let handle_result = cx.fresh_temp("handle");
            let handle_capability = cx.fresh_temp("capability");
            let handle_evidence = cx.fresh_temp("evidence");
            emit_indent(out, indent);
            writeln!(out, "CieloValue {handle_result} = cv_unit();")
                .expect("in-memory write should not fail");
            emit_indent(out, indent);
            writeln!(
                out,
                "CieloEvidence {handle_evidence} = {{ .abi_version = cielo_runtime_abi_version(), .effect = {}, .capability_id = 0, .clause_count = 0, .clauses = NULL, .captures = NULL, .reserved0 = NULL, .reserved1 = NULL }};",
                effect.as_u32()
            )
            .expect("in-memory write should not fail");
            emit_indent(out, indent);
            writeln!(
                out,
                "uint32_t {handle_capability} = cielo_handler_push_with_evidence({}, &{handle_evidence});",
                effect.as_u32(),
            )
            .expect("in-memory write should not fail");
            cx.push_capability(*effect, handle_capability.clone());
            emit_stmt(
                *body,
                EmitMode::AssignTemp(handle_result.clone()),
                out,
                indent,
                cx,
            );
            cx.pop_capability(*effect, handle_capability.as_str());
            emit_indent(out, indent);
            writeln!(out, "cielo_handler_pop({handle_capability});")
                .expect("in-memory write should not fail");
            emit_arc_ops(stmt_id, ArcEmitPlacement::PostRelease, out, indent, cx);
            if let Some(next_stmt) = next {
                emit_stmt(*next_stmt, mode, out, indent, cx);
            } else {
                emit_leaf(mode, handle_result, out, indent);
            }
        }
        LinearStmt::Stage { stage, body, next } => {
            emit_indent(out, indent);
            writeln!(out, "/* stage {:?} */", stage).expect("in-memory write should not fail");
            if let Some(next_stmt) = next {
                emit_stmt(*body, EmitMode::Discard, out, indent, cx);
                emit_arc_ops(stmt_id, ArcEmitPlacement::PostRelease, out, indent, cx);
                emit_stmt(*next_stmt, mode, out, indent, cx);
            } else {
                emit_stmt(*body, mode, out, indent, cx);
                emit_arc_ops(stmt_id, ArcEmitPlacement::PostRelease, out, indent, cx);
            }
        }
        LinearStmt::Hole => {
            emit_arc_ops(stmt_id, ArcEmitPlacement::PostRelease, out, indent, cx);
            emit_leaf(mode, "cv_unit()".to_owned(), out, indent);
        }
        LinearStmt::Error => {
            emit_indent(out, indent);
            out.push_str("/* error node reached in linear backend */\n");
            emit_arc_ops(stmt_id, ArcEmitPlacement::PostRelease, out, indent, cx);
            emit_leaf(mode, "cv_unit()".to_owned(), out, indent);
        }
    }
}

#[derive(Clone, Copy)]
enum ArcEmitPlacement {
    PreRetain,
    PostRelease,
}

fn emit_arc_ops(
    stmt_id: LinearStmtId,
    placement: ArcEmitPlacement,
    out: &mut String,
    indent: usize,
    cx: &EmitCx<'_>,
) -> bool {
    let vars = match placement {
        ArcEmitPlacement::PreRetain => cx.arc_plan.pre_retain_vars(stmt_id),
        ArcEmitPlacement::PostRelease => cx.arc_plan.post_release_vars(stmt_id),
    };
    if vars.is_empty() {
        return false;
    }
    for var in vars {
        emit_indent(out, indent);
        match placement {
            ArcEmitPlacement::PreRetain => {
                if cx.arc_emit_trace {
                    writeln!(
                        out,
                        "/* arc pre-retain s{} v{} */",
                        stmt_id.as_u32(),
                        var.as_u32()
                    )
                    .expect("in-memory write should not fail");
                    emit_indent(out, indent);
                }
                writeln!(out, "cielo_arc_retain(v{});", var.as_u32())
                    .expect("in-memory write should not fail");
            }
            ArcEmitPlacement::PostRelease => {
                if cx.arc_emit_trace {
                    writeln!(
                        out,
                        "/* arc post-release s{} v{} */",
                        stmt_id.as_u32(),
                        var.as_u32()
                    )
                    .expect("in-memory write should not fail");
                    emit_indent(out, indent);
                }
                writeln!(out, "cielo_arc_release(v{});", var.as_u32())
                    .expect("in-memory write should not fail");
            }
        }
    }
    true
}

fn emit_leaf(mode: EmitMode, value_expr: String, out: &mut String, indent: usize) {
    emit_indent(out, indent);
    match mode {
        EmitMode::Return => {
            writeln!(out, "return {};", value_expr).expect("in-memory write should not fail");
        }
        EmitMode::AssignVar(var) => {
            writeln!(out, "v{} = {};", var.as_u32(), value_expr)
                .expect("in-memory write should not fail");
        }
        EmitMode::AssignTemp(name) => {
            writeln!(out, "{} = {};", name, value_expr).expect("in-memory write should not fail");
        }
        EmitMode::Discard => {
            writeln!(out, "(void){};", value_expr).expect("in-memory write should not fail");
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_lowered_call(
    stmt_id: LinearStmtId,
    result: VarId,
    callee: SymbolId,
    args: &[LinearExprId],
    next: LinearStmtId,
    convention: CallConvention,
    mode: EmitMode,
    out: &mut String,
    indent: usize,
    cx: &mut EmitCx<'_>,
) {
    let callee_name = cx
        .fn_name_by_symbol
        .get(&callee)
        .cloned()
        .unwrap_or_else(|| "cielo_fn_unknown".to_owned());
    let wrapper = call_wrapper(convention);
    let call_expr = format_callee_call(callee_name.as_str(), args, cx);
    if matches!(convention, CallConvention::Control) {
        emit_indent(out, indent);
        out.push_str("/* control-call convention: selective CPS hook */\n");
    }
    emit_indent(out, indent);
    writeln!(out, "v{} = {}({});", result.as_u32(), wrapper, call_expr)
        .expect("in-memory write should not fail");
    emit_arc_ops(stmt_id, ArcEmitPlacement::PostRelease, out, indent, cx);
    emit_stmt(next, mode, out, indent, cx);
}

fn emit_expr(expr_id: LinearExprId, cx: &EmitCx<'_>) -> String {
    let Some(expr) = cx.program.expr(expr_id) else {
        return "cv_unit()".to_owned();
    };

    match &expr.kind {
        LinearExpr::Var(var) => format!("v{}", var.as_u32()),
        LinearExpr::Literal(lit) => emit_literal(lit, cx.string_pool, cx.scalar_pool),
        LinearExpr::Unary { op, expr } => format!("{}({})", op.c_func(), emit_expr(*expr, cx)),
        LinearExpr::Binary { op, lhs, rhs } => {
            let lhs = emit_expr(*lhs, cx);
            let rhs = emit_expr(*rhs, cx);
            format!("{}({}, {})", op.c_func(), lhs, rhs)
        }
        LinearExpr::PureCall { callee, args } => {
            let callee_name = cx
                .fn_name_by_symbol
                .get(callee)
                .cloned()
                .unwrap_or_else(|| "cielo_fn_unknown".to_owned());
            let call_expr = format_callee_call(callee_name.as_str(), args, cx);
            format!("{}({call_expr})", call_wrapper(CallConvention::Pure))
        }
        LinearExpr::MakeStruct { ty, fields } => {
            if let Some(key) = constant_table::ctor_key_from_linear_expr(
                cx.program,
                *ty,
                SymbolId::INVALID,
                fields,
            ) {
                if let Some(symbol) = cx.ctor_pool.symbol_for(&key) {
                    return symbol.to_owned();
                }
            }
            let ty_name = escape_c_string(symbol_text(cx.interner, *ty).as_str());
            format_ctor_call(&ty_name, "", fields, cx)
        }
        LinearExpr::MakeEnum {
            ty,
            variant,
            fields,
        } => {
            if let Some(key) =
                constant_table::ctor_key_from_linear_expr(cx.program, *ty, *variant, fields)
            {
                if let Some(symbol) = cx.ctor_pool.symbol_for(&key) {
                    return symbol.to_owned();
                }
            }
            let ty_name = escape_c_string(symbol_text(cx.interner, *ty).as_str());
            let variant_name = escape_c_string(symbol_text(cx.interner, *variant).as_str());
            format_ctor_call(&ty_name, &variant_name, fields, cx)
        }
        LinearExpr::Error => "cv_unit()".to_owned(),
    }
}

fn call_wrapper(convention: CallConvention) -> &'static str {
    match convention {
        CallConvention::Pure => "CIELO_CALL_PURE",
        CallConvention::Direct => "CIELO_CALL_DIRECT",
        CallConvention::Control => "CIELO_CALL_CONTROL",
    }
}

fn format_callee_call(callee_name: &str, args: &[LinearExprId], cx: &EmitCx<'_>) -> String {
    let args = args
        .iter()
        .map(|arg| emit_expr(*arg, cx))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{callee_name}({args})")
}

fn format_ctor_call(
    ty_name: &str,
    variant_name: &str,
    fields: &[LinearExprId],
    cx: &EmitCx<'_>,
) -> String {
    let mut out = String::new();
    write!(
        out,
        "cielo_make_ctor(\"{}\", \"{}\", {}, ",
        ty_name,
        variant_name,
        fields.len()
    )
    .expect("in-memory write should not fail");
    if fields.is_empty() {
        out.push_str("NULL");
    } else {
        out.push_str("(CieloValue[]){");
        for (idx, field) in fields.iter().enumerate() {
            if idx > 0 {
                out.push_str(", ");
            }
            out.push_str(&emit_expr(*field, cx));
        }
        out.push('}');
    }
    out.push(')');
    out
}

fn emit_literal(
    lit: &Literal,
    string_pool: &StringConstPool,
    scalar_pool: &ScalarConstPool,
) -> String {
    match lit {
        Literal::Unit => "cv_unit()".to_owned(),
        Literal::Bool(value) => scalar_pool
            .symbol_for(ScalarLiteralKey::Bool(*value))
            .map_or_else(
                || format!("cv_bool({})", if *value { 1 } else { 0 }),
                |symbol| symbol.to_owned(),
            ),
        Literal::Int(value) => scalar_pool
            .symbol_for(ScalarLiteralKey::Int(*value))
            .map_or_else(|| format!("cv_int({value})"), |symbol| symbol.to_owned()),
        Literal::Float(value) => scalar_pool
            .symbol_for(ScalarLiteralKey::Float(value.to_bits()))
            .map_or_else(
                || format!("cv_float({})", format_float_literal(*value)),
                |symbol| symbol.to_owned(),
            ),
        Literal::Char(value) => scalar_pool
            .symbol_for(ScalarLiteralKey::Char(*value))
            .map_or_else(
                || format!("cv_char({})", *value as u32),
                |symbol| symbol.to_owned(),
            ),
        Literal::String(value) => {
            if let Some(symbol) = string_pool.symbol_for(value) {
                format!("cv_string({symbol})")
            } else {
                format!("cv_string(\"{}\")", escape_c_string(value))
            }
        }
    }
}

fn collect_stmt_vars(
    program: &LinearProgram,
    stmt_id: LinearStmtId,
    out: &mut BTreeSet<VarId>,
    seen_stmts: &mut HashSet<LinearStmtId>,
) {
    if !seen_stmts.insert(stmt_id) {
        return;
    }
    let Some(stmt) = program.stmt(stmt_id) else {
        return;
    };

    match &stmt.kind {
        LinearStmt::Let { binding, .. } | LinearStmt::Val { binding, .. } => {
            out.insert(*binding);
        }
        LinearStmt::PureCall { result, .. }
        | LinearStmt::DirectCall { result, .. }
        | LinearStmt::ControlCall { result, .. } => {
            out.insert(*result);
        }
        LinearStmt::Perform { result, .. } => {
            if let Some(result) = result {
                out.insert(*result);
            }
        }
        LinearStmt::Match { arms, .. } => {
            for arm in arms {
                for binder in &arm.binders {
                    out.insert(*binder);
                }
            }
        }
        LinearStmt::Return(_)
        | LinearStmt::If { .. }
        | LinearStmt::Handle { .. }
        | LinearStmt::Stage { .. }
        | LinearStmt::Hole
        | LinearStmt::Error => {}
    }

    for expr in stmt.child_exprs() {
        collect_expr_vars(program, expr, out, &mut HashSet::new());
    }
    for child in stmt.child_stmts() {
        collect_stmt_vars(program, child, out, seen_stmts);
    }
}

fn collect_expr_vars(
    program: &LinearProgram,
    expr_id: LinearExprId,
    out: &mut BTreeSet<VarId>,
    seen_exprs: &mut HashSet<LinearExprId>,
) {
    if !seen_exprs.insert(expr_id) {
        return;
    }
    let Some(expr) = program.expr(expr_id) else {
        return;
    };
    match &expr.kind {
        LinearExpr::Var(var) => {
            out.insert(*var);
        }
        LinearExpr::Unary { expr, .. } => collect_expr_vars(program, *expr, out, seen_exprs),
        LinearExpr::Binary { lhs, rhs, .. } => {
            collect_expr_vars(program, *lhs, out, seen_exprs);
            collect_expr_vars(program, *rhs, out, seen_exprs);
        }
        LinearExpr::PureCall { args, .. }
        | LinearExpr::MakeStruct { fields: args, .. }
        | LinearExpr::MakeEnum { fields: args, .. } => {
            for arg in args {
                collect_expr_vars(program, *arg, out, seen_exprs);
            }
        }
        LinearExpr::Literal(_) | LinearExpr::Error => {}
    }
}

fn emit_c_main_wrapper(out: &mut String, program: &LinearProgram, fn_names: &[String]) {
    out.push_str("int main(void) {\n");
    let Some(&entry_fn) = program.entrypoints.first() else {
        out.push_str("    return 0;\n}\n");
        return;
    };
    let Some(function) = program.functions.get(entry_fn.index()) else {
        out.push_str("    return 0;\n}\n");
        return;
    };
    let Some(callee_name) = fn_names.get(entry_fn.index()) else {
        out.push_str("    return 0;\n}\n");
        return;
    };

    out.push_str("    CieloValue _entry = ");
    out.push_str(callee_name);
    out.push('(');
    for idx in 0..function.params.len() {
        if idx > 0 {
            out.push_str(", ");
        }
        out.push_str("cv_unit()");
    }
    out.push_str(");\n");
    out.push_str("    if (_entry.tag == CV_INT) return (int)_entry.as.i;\n");
    out.push_str("    if (_entry.tag == CV_BOOL) return _entry.as.b ? 0 : 1;\n");
    out.push_str("    return 0;\n");
    out.push_str("}\n");
}

fn symbol_text(interner: &Interner, symbol: SymbolId) -> String {
    interner.resolve(symbol).unwrap_or("sym").to_owned()
}

fn sanitize_symbol(symbol: String) -> String {
    let mut out = String::with_capacity(symbol.len());
    for ch in symbol.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "sym".to_owned()
    } else {
        out
    }
}

fn escape_c_string(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_ascii_graphic() || c == ' ' => out.push(c),
            c => {
                write!(out, "\\x{:02X}", c as u32).expect("in-memory write should not fail");
            }
        }
    }
    out
}

fn format_float_literal(value: f64) -> String {
    let mut text = format!("{value:?}");
    if !text.contains('.') && !text.contains('e') && !text.contains('E') {
        text.push_str(".0");
    }
    text
}

fn emit_indent(out: &mut String, level: usize) {
    for _ in 0..level {
        out.push_str("    ");
    }
}

struct EmitCx<'a> {
    program: &'a LinearProgram,
    fn_name_by_symbol: &'a HashMap<SymbolId, String>,
    interner: &'a Interner,
    string_pool: &'a StringConstPool,
    scalar_pool: &'a ScalarConstPool,
    ctor_pool: &'a CtorConstPool,
    arc_plan: &'a ArcEmitPlan,
    arc_emit_trace: bool,
    next_temp: u32,
    active_capabilities: Vec<(u32, String)>,
}

impl EmitCx<'_> {
    fn fresh_temp(&mut self, prefix: &str) -> String {
        let id = self.next_temp;
        self.next_temp += 1;
        format!("__cielo_{}_{}", prefix, id)
    }

    fn push_capability(&mut self, effect: EffectLabelId, binding: String) {
        self.active_capabilities.push((effect.as_u32(), binding));
    }

    fn pop_capability(&mut self, effect: EffectLabelId, binding: &str) {
        if let Some(idx) = self
            .active_capabilities
            .iter()
            .rposition(|(active_effect, name)| *active_effect == effect.as_u32() && name == binding)
        {
            self.active_capabilities.remove(idx);
        }
    }

    fn active_capability_for(&self, effect: EffectLabelId) -> Option<&str> {
        self.active_capabilities
            .iter()
            .rfind(|(active_effect, _)| *active_effect == effect.as_u32())
            .map(|(_, name)| name.as_str())
    }
}

#[derive(Clone)]
enum EmitMode {
    Return,
    AssignVar(VarId),
    AssignTemp(String),
    Discard,
}

#[derive(Clone, Debug)]
struct StringConstEntry {
    symbol: String,
    value: String,
}

#[derive(Clone, Debug, Default)]
struct StringConstPool {
    entries: Vec<StringConstEntry>,
    by_value: HashMap<String, usize>,
}

impl StringConstPool {
    fn symbol_for(&self, value: &str) -> Option<&str> {
        self.by_value
            .get(value)
            .and_then(|idx| self.entries.get(*idx))
            .map(|entry| entry.symbol.as_str())
    }
}

#[derive(Clone, Debug)]
struct ScalarConstEntry {
    symbol: String,
    key: ScalarLiteralKey,
}

#[derive(Clone, Debug, Default)]
struct ScalarConstPool {
    entries: Vec<ScalarConstEntry>,
    by_key: HashMap<ScalarLiteralKey, usize>,
}

impl ScalarConstPool {
    fn symbol_for(&self, key: ScalarLiteralKey) -> Option<&str> {
        self.by_key
            .get(&key)
            .and_then(|idx| self.entries.get(*idx))
            .map(|entry| entry.symbol.as_str())
    }
}

#[derive(Clone, Debug)]
struct CtorConstEntry {
    value_symbol: String,
    ctor_symbol: String,
    fields_symbol: Option<String>,
    key: CtorLiteralKey,
}

#[derive(Clone, Debug, Default)]
struct CtorConstPool {
    entries: Vec<CtorConstEntry>,
    by_key: HashMap<CtorLiteralKey, usize>,
}

impl CtorConstPool {
    fn symbol_for(&self, key: &CtorLiteralKey) -> Option<&str> {
        self.by_key
            .get(key)
            .and_then(|idx| self.entries.get(*idx))
            .map(|entry| entry.value_symbol.as_str())
    }
}

fn build_scalar_const_pool(table: &ConstantTable) -> ScalarConstPool {
    let mut pool = ScalarConstPool::default();
    for entry in &table.entries {
        if !matches!(entry.strategy, ConstantEmbedStrategy::StaticConst) {
            continue;
        }
        let ConstantKey::Scalar(key) = &entry.key else {
            continue;
        };
        let idx = pool.entries.len();
        pool.entries.push(ScalarConstEntry {
            symbol: format!("cielo_const_v_{idx}"),
            key: *key,
        });
        pool.by_key.insert(*key, idx);
    }
    pool
}

fn build_ctor_const_pool(table: &ConstantTable) -> CtorConstPool {
    let mut pool = CtorConstPool::default();
    for entry in &table.entries {
        if !matches!(entry.strategy, ConstantEmbedStrategy::Pooled) {
            continue;
        }
        let ConstantKey::Ctor(key) = &entry.key else {
            continue;
        };
        let idx = pool.entries.len();
        let fields_symbol =
            (!key.fields.is_empty()).then(|| format!("cielo_const_ctor_fields_{idx}"));
        pool.entries.push(CtorConstEntry {
            value_symbol: format!("cielo_const_ctor_v_{idx}"),
            ctor_symbol: format!("cielo_const_ctor_{idx}"),
            fields_symbol,
            key: key.to_owned(),
        });
        pool.by_key.insert(key.to_owned(), idx);
    }
    pool
}

fn build_string_const_pool(table: &ConstantTable) -> StringConstPool {
    let mut pool = StringConstPool::default();
    for entry in &table.entries {
        if !matches!(entry.strategy, ConstantEmbedStrategy::StaticConst) {
            continue;
        }
        let ConstantKey::String(value) = &entry.key else {
            continue;
        };
        let idx = pool.entries.len();
        pool.entries.push(StringConstEntry {
            symbol: format!("cielo_const_s_{idx}"),
            value: value.to_owned(),
        });
        pool.by_value.insert(value.to_owned(), idx);
    }
    pool
}

fn emit_scalar_const_pool(out: &mut String, pool: &ScalarConstPool) {
    for entry in &pool.entries {
        let init = match entry.key {
            ScalarLiteralKey::Bool(value) => format!(
                "{{ .tag = CV_BOOL, .as.b = {} }}",
                if value { "true" } else { "false" }
            ),
            ScalarLiteralKey::Int(value) => format!("{{ .tag = CV_INT, .as.i = {value} }}"),
            ScalarLiteralKey::Char(value) => {
                format!("{{ .tag = CV_CHAR, .as.c = {}u }}", value as u32)
            }
            ScalarLiteralKey::Float(bits) => {
                let value = f64::from_bits(bits);
                format!(
                    "{{ .tag = CV_FLOAT, .as.f = {} }}",
                    format_float_literal(value)
                )
            }
        };
        writeln!(out, "static const CieloValue {} = {};", entry.symbol, init)
            .expect("in-memory write should not fail");
    }
}

fn emit_string_const_pool(out: &mut String, pool: &StringConstPool) {
    for entry in &pool.entries {
        writeln!(
            out,
            "static const char* {} = \"{}\";",
            entry.symbol,
            escape_c_string(entry.value.as_str())
        )
        .expect("in-memory write should not fail");
    }
}

fn emit_ctor_const_pool(out: &mut String, pool: &CtorConstPool, interner: &Interner) {
    let mut nested_counter = 0usize;
    for entry in &pool.entries {
        if let Some(fields_symbol) = &entry.fields_symbol {
            let mut nested_decls = String::new();
            let mut field_initializers = Vec::with_capacity(entry.key.fields.len());
            for field in &entry.key.fields {
                let rendered = ctor_field_value_initializer(field, interner, &mut nested_counter);
                nested_decls.push_str(rendered.declarations.as_str());
                field_initializers.push(rendered.initializer);
            }
            out.push_str(nested_decls.as_str());

            write!(out, "static CieloValue {fields_symbol}[] = {{")
                .expect("in-memory write should not fail");
            for (idx, initializer) in field_initializers.iter().enumerate() {
                if idx > 0 {
                    out.push_str(", ");
                }
                out.push_str(initializer.as_str());
            }
            out.push_str("};\n");
        }

        let ty_name = escape_c_string(symbol_text(interner, entry.key.ty).as_str());
        let variant_name = if entry.key.variant.is_valid() {
            escape_c_string(symbol_text(interner, entry.key.variant).as_str())
        } else {
            String::new()
        };
        let fields_ref = entry.fields_symbol.as_deref().unwrap_or("NULL");

        writeln!(
            out,
            "static CieloCtor {} = {{ .arc = CIELO_ARC_IMMORTAL_HEADER, .ty = \"{}\", .variant = \"{}\", .argc = {}, .fields = {} }};",
            entry.ctor_symbol,
            ty_name,
            variant_name,
            entry.key.fields.len(),
            fields_ref
        )
        .expect("in-memory write should not fail");
        writeln!(
            out,
            "static const CieloValue {} = {{ .tag = CV_CTOR, .as.ctor = &{} }};",
            entry.value_symbol, entry.ctor_symbol
        )
        .expect("in-memory write should not fail");
    }
}

struct RenderedCtorField {
    declarations: String,
    initializer: String,
}

fn ctor_field_value_initializer(
    field: &CtorFieldKey,
    interner: &Interner,
    nested_counter: &mut usize,
) -> RenderedCtorField {
    match field {
        CtorFieldKey::Unit => RenderedCtorField {
            declarations: String::new(),
            initializer: "{ .tag = CV_UNIT }".to_owned(),
        },
        CtorFieldKey::Bool(value) => RenderedCtorField {
            declarations: String::new(),
            initializer: format!(
                "{{ .tag = CV_BOOL, .as.b = {} }}",
                if *value { "true" } else { "false" }
            ),
        },
        CtorFieldKey::Int(value) => RenderedCtorField {
            declarations: String::new(),
            initializer: format!("{{ .tag = CV_INT, .as.i = {value} }}"),
        },
        CtorFieldKey::Char(value) => RenderedCtorField {
            declarations: String::new(),
            initializer: format!("{{ .tag = CV_CHAR, .as.c = {}u }}", *value as u32),
        },
        CtorFieldKey::Float(bits) => {
            let value = f64::from_bits(*bits);
            RenderedCtorField {
                declarations: String::new(),
                initializer: format!(
                    "{{ .tag = CV_FLOAT, .as.f = {} }}",
                    format_float_literal(value)
                ),
            }
        }
        CtorFieldKey::String(value) => RenderedCtorField {
            declarations: String::new(),
            initializer: format!(
                "{{ .tag = CV_STRING, .as.s = \"{}\" }}",
                escape_c_string(value)
            ),
        },
        CtorFieldKey::Ctor(key) => {
            let nested_id = *nested_counter;
            *nested_counter = nested_counter.saturating_add(1);

            let ctor_symbol = format!("cielo_const_ctor_nested_{nested_id}");
            let fields_symbol = (!key.fields.is_empty())
                .then(|| format!("cielo_const_ctor_nested_fields_{nested_id}"));
            let mut declarations = String::new();

            if let Some(fields_symbol) = &fields_symbol {
                let mut nested_field_initializers = Vec::with_capacity(key.fields.len());
                for nested_field in &key.fields {
                    let nested_rendered =
                        ctor_field_value_initializer(nested_field, interner, nested_counter);
                    declarations.push_str(nested_rendered.declarations.as_str());
                    nested_field_initializers.push(nested_rendered.initializer);
                }

                write!(declarations, "static CieloValue {fields_symbol}[] = {{")
                    .expect("in-memory write should not fail");
                for (idx, initializer) in nested_field_initializers.iter().enumerate() {
                    if idx > 0 {
                        declarations.push_str(", ");
                    }
                    declarations.push_str(initializer.as_str());
                }
                declarations.push_str("};\n");
            }

            let ty_name = escape_c_string(symbol_text(interner, key.ty).as_str());
            let variant_name = if key.variant.is_valid() {
                escape_c_string(symbol_text(interner, key.variant).as_str())
            } else {
                String::new()
            };
            let fields_ref = fields_symbol.as_deref().unwrap_or("NULL");
            writeln!(
                declarations,
                "static CieloCtor {ctor_symbol} = {{ .arc = CIELO_ARC_IMMORTAL_HEADER, .ty = \"{}\", .variant = \"{}\", .argc = {}, .fields = {} }};",
                ty_name,
                variant_name,
                key.fields.len(),
                fields_ref
            )
            .unwrap();

            RenderedCtorField {
                declarations,
                initializer: format!("{{ .tag = CV_CTOR, .as.ctor = &{ctor_symbol} }}"),
            }
        }
    }
}
