// Pass 8/8: c_emit (linear runtime IR -> C source)
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

use crate::common::ids::{LinearExprId, LinearStmtId, SymbolId, VarId};
use crate::common::symbols::Interner;
use crate::ir::core::Literal;
use crate::ir::linear::{LinearExpr, LinearFunction, LinearProgram, LinearStmt};
use crate::passes::linearize::Linearized;

const C_RUNTIME_HEADER: &str = include_str!("../backend/cielo_runtime.h");

#[derive(Clone, Debug)]
pub struct EmittedC {
    pub linearized: Linearized,
    pub c_source: String,
}

pub fn run(linearized: Linearized, interner: &Interner) -> EmittedC {
    let c_source = emit_c_program(&linearized.linear, interner);
    EmittedC {
        linearized,
        c_source,
    }
}

pub fn emit_c_program(program: &LinearProgram, interner: &Interner) -> String {
    let mut out = String::new();
    out.push_str(C_RUNTIME_HEADER);
    if !out.ends_with('\n') {
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
        );
        out.push('\n');
    }

    emit_c_main_wrapper(&mut out, program, &fn_name_by_index);
    out
}

fn emit_function(
    out: &mut String,
    program: &LinearProgram,
    function: &LinearFunction,
    c_name: &str,
    fn_name_by_symbol: &HashMap<SymbolId, String>,
    interner: &Interner,
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
        next_temp: 0,
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

    match &stmt.kind {
        LinearStmt::Return(expr) => emit_leaf(mode, emit_expr(*expr, cx), out, indent),
        LinearStmt::Let {
            binding,
            value,
            next,
        } => {
            emit_indent(out, indent);
            writeln!(out, "v{} = {};", binding.as_u32(), emit_expr(*value, cx))
                .expect("in-memory write should not fail");
            emit_stmt(*next, mode, out, indent, cx);
        }
        LinearStmt::Val {
            binding,
            value,
            next,
        } => {
            emit_stmt(*value, EmitMode::AssignVar(*binding), out, indent, cx);
            emit_stmt(*next, mode, out, indent, cx);
        }
        LinearStmt::Call {
            result,
            callee,
            args,
            next,
        } => {
            let callee_name = cx
                .fn_name_by_symbol
                .get(callee)
                .cloned()
                .unwrap_or_else(|| "cielo_fn_unknown".to_owned());
            emit_indent(out, indent);
            write!(out, "v{} = {}(", result.as_u32(), callee_name)
                .expect("in-memory write should not fail");
            for (idx, arg) in args.iter().enumerate() {
                if idx > 0 {
                    out.push_str(", ");
                }
                out.push_str(&emit_expr(*arg, cx));
            }
            out.push_str(");\n");
            emit_stmt(*next, mode, out, indent, cx);
        }
        LinearStmt::If {
            cond,
            then_branch,
            else_branch,
        } => {
            emit_indent(out, indent);
            writeln!(out, "if (cv_truthy({})) {{", emit_expr(*cond, cx))
                .expect("in-memory write should not fail");
            emit_stmt(*then_branch, mode.clone(), out, indent + 1, cx);
            emit_indent(out, indent);
            out.push_str("} else {\n");
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
            writeln!(out, "CieloValue {match_value} = {};", emit_expr(*scrutinee, cx))
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
                emit_stmt(arm.body, mode.clone(), out, indent + 1, cx);
                emit_indent(out, indent);
                out.push_str("}\n");
            }
            if let Some(default_stmt) = default {
                emit_indent(out, indent);
                out.push_str("else {\n");
                emit_stmt(*default_stmt, mode.clone(), out, indent + 1, cx);
                emit_indent(out, indent);
                out.push_str("}\n");
            } else if !arms.is_empty() {
                emit_indent(out, indent);
                out.push_str("else {\n");
                emit_leaf(mode, "cv_unit()".to_owned(), out, indent + 1);
                emit_indent(out, indent);
                out.push_str("}\n");
            } else {
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
            emit_indent(out, indent);
            if let Some(dst) = result {
                write!(out, "v{} = ", dst.as_u32()).expect("in-memory write should not fail");
            } else {
                out.push_str("(void)");
            }
            write!(
                out,
                "cielo_perform({}, \"{}\", {}, ",
                effect.as_u32(),
                op_name,
                args.len()
            )
            .expect("in-memory write should not fail");
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
            emit_stmt(*next, mode, out, indent, cx);
        }
        LinearStmt::Handle { effect, body, next } => {
            let handle_result = cx.fresh_temp("handle");
            emit_indent(out, indent);
            writeln!(out, "CieloValue {handle_result} = cv_unit();")
                .expect("in-memory write should not fail");
            emit_indent(out, indent);
            writeln!(out, "cielo_handler_push({});", effect.as_u32())
                .expect("in-memory write should not fail");
            emit_stmt(
                *body,
                EmitMode::AssignTemp(handle_result.clone()),
                out,
                indent,
                cx,
            );
            emit_indent(out, indent);
            writeln!(out, "cielo_handler_pop({});", effect.as_u32())
                .expect("in-memory write should not fail");
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
                emit_stmt(*next_stmt, mode, out, indent, cx);
            } else {
                emit_stmt(*body, mode, out, indent, cx);
            }
        }
        LinearStmt::Hole => emit_leaf(mode, "cv_unit()".to_owned(), out, indent),
        LinearStmt::Error => {
            emit_indent(out, indent);
            out.push_str("/* error node reached in linear backend */\n");
            emit_leaf(mode, "cv_unit()".to_owned(), out, indent);
        }
    }
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

fn emit_expr(expr_id: LinearExprId, cx: &EmitCx<'_>) -> String {
    let Some(expr) = cx.program.expr(expr_id) else {
        return "cv_unit()".to_owned();
    };

    match &expr.kind {
        LinearExpr::Var(var) => format!("v{}", var.as_u32()),
        LinearExpr::Literal(lit) => emit_literal(lit),
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
            let args = args
                .iter()
                .map(|arg| emit_expr(*arg, cx))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}({})", callee_name, args)
        }
        LinearExpr::MakeStruct { ty, fields } => {
            let ty_name = escape_c_string(symbol_text(cx.interner, *ty).as_str());
            format_ctor_call(&ty_name, "", fields, cx)
        }
        LinearExpr::MakeEnum {
            ty,
            variant,
            fields,
        } => {
            let ty_name = escape_c_string(symbol_text(cx.interner, *ty).as_str());
            let variant_name = escape_c_string(symbol_text(cx.interner, *variant).as_str());
            format_ctor_call(&ty_name, &variant_name, fields, cx)
        }
        LinearExpr::Error => "cv_unit()".to_owned(),
    }
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

fn emit_literal(lit: &Literal) -> String {
    match lit {
        Literal::Unit => "cv_unit()".to_owned(),
        Literal::Bool(value) => format!("cv_bool({})", if *value { 1 } else { 0 }),
        Literal::Int(value) => format!("cv_int({value})"),
        Literal::Float(value) => format!("cv_float({value})"),
        Literal::Char(value) => format!("cv_char({})", *value as u32),
        Literal::String(value) => format!("cv_string(\"{}\")", escape_c_string(value)),
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
        LinearStmt::Call { result, .. } => {
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

fn emit_indent(out: &mut String, level: usize) {
    for _ in 0..level {
        out.push_str("    ");
    }
}

struct EmitCx<'a> {
    program: &'a LinearProgram,
    fn_name_by_symbol: &'a HashMap<SymbolId, String>,
    interner: &'a Interner,
    next_temp: u32,
}

impl EmitCx<'_> {
    fn fresh_temp(&mut self, prefix: &str) -> String {
        let id = self.next_temp;
        self.next_temp += 1;
        format!("__cielo_{}_{}", prefix, id)
    }
}

#[derive(Clone)]
enum EmitMode {
    Return,
    AssignVar(VarId),
    AssignTemp(String),
    Discard,
}
