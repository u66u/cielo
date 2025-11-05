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

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write;

use crate::common::ids::{SymbolId, VarId};
use crate::common::symbols::Interner;
use crate::ir::core::{BinaryOp, Literal, UnaryOp};
use crate::ir::linear::{LinearExpr, LinearFunction, LinearProgram, LinearStmt};
use crate::passes::linearize::Linearized;

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
    emit_c_prelude(&mut out);

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
        emit_function(&mut out, function, name, &fn_name_by_symbol, interner);
        out.push('\n');
    }

    emit_c_main_wrapper(&mut out, program, &fn_name_by_index);
    out
}

fn emit_function(
    out: &mut String,
    function: &LinearFunction,
    c_name: &str,
    fn_name_by_symbol: &HashMap<SymbolId, String>,
    interner: &Interner,
) {
    emit_fn_signature(out, c_name, &function.params);
    out.push_str(" {\n");

    let mut vars = BTreeSet::new();
    collect_stmt_vars(&function.body, &mut vars);
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
        fn_name_by_symbol,
        interner,
    };
    emit_stmt(&function.body, EmitMode::Return, out, 1, &mut cx);
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
    stmt: &LinearStmt,
    mode: EmitMode,
    out: &mut String,
    indent: usize,
    cx: &mut EmitCx<'_>,
) {
    match stmt {
        LinearStmt::Return(expr) => emit_leaf(mode, emit_expr(expr, cx), out, indent),
        LinearStmt::Let {
            binding,
            value,
            next,
        } => {
            emit_indent(out, indent);
            writeln!(out, "v{} = {};", binding.as_u32(), emit_expr(value, cx))
                .expect("in-memory write should not fail");
            emit_stmt(next, mode, out, indent, cx);
        }
        LinearStmt::Val {
            binding,
            value,
            next,
        } => {
            emit_stmt(value, EmitMode::Assign(*binding), out, indent, cx);
            emit_stmt(next, mode, out, indent, cx);
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
                out.push_str(&emit_expr(arg, cx));
            }
            out.push_str(");\n");
            emit_stmt(next, mode, out, indent, cx);
        }
        LinearStmt::If {
            cond,
            then_branch,
            else_branch,
        } => {
            emit_indent(out, indent);
            writeln!(out, "if (cv_truthy({})) {{", emit_expr(cond, cx))
                .expect("in-memory write should not fail");
            emit_stmt(then_branch, mode, out, indent + 1, cx);
            emit_indent(out, indent);
            out.push_str("} else {\n");
            emit_stmt(else_branch, mode, out, indent + 1, cx);
            emit_indent(out, indent);
            out.push_str("}\n");
        }
        LinearStmt::Match {
            scrutinee,
            arms,
            default,
        } => {
            emit_indent(out, indent);
            writeln!(out, "(void){};", emit_expr(scrutinee, cx))
                .expect("in-memory write should not fail");
            emit_indent(out, indent);
            out.push_str(
                "/* TODO(v0): real match lowering in backend; selecting default/first arm */\n",
            );
            if let Some(default_stmt) = default {
                emit_stmt(default_stmt, mode, out, indent, cx);
            } else if let Some(first) = arms.first() {
                emit_stmt(&first.body, mode, out, indent, cx);
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
            let args_rendered: Vec<String> = args.iter().map(|arg| emit_expr(arg, cx)).collect();
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
                args_rendered.len()
            )
            .expect("in-memory write should not fail");
            if args_rendered.is_empty() {
                out.push_str("NULL");
            } else {
                out.push_str("(CieloValue[]){");
                for (idx, arg) in args_rendered.iter().enumerate() {
                    if idx > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(arg);
                }
                out.push('}');
            }
            out.push_str(");\n");
            emit_stmt(next, mode, out, indent, cx);
        }
        LinearStmt::Handle { effect, body, next } => {
            emit_indent(out, indent);
            writeln!(
                out,
                "/* TODO(v0): handler runtime lowering for effect {} */",
                effect.as_u32()
            )
            .expect("in-memory write should not fail");
            if let Some(next_stmt) = next {
                emit_stmt(body, EmitMode::Discard, out, indent, cx);
                emit_stmt(next_stmt, mode, out, indent, cx);
            } else {
                emit_stmt(body, mode, out, indent, cx);
            }
        }
        LinearStmt::Stage { stage, body, next } => {
            emit_indent(out, indent);
            writeln!(out, "/* stage {:?} */", stage).expect("in-memory write should not fail");
            if next.is_some() {
                emit_stmt(body, EmitMode::Discard, out, indent, cx);
                if let Some(next_stmt) = next {
                    emit_stmt(next_stmt, mode, out, indent, cx);
                }
            } else {
                emit_stmt(body, mode, out, indent, cx);
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
        EmitMode::Assign(var) => {
            writeln!(out, "v{} = {};", var.as_u32(), value_expr)
                .expect("in-memory write should not fail");
        }
        EmitMode::Discard => {
            writeln!(out, "(void){};", value_expr).expect("in-memory write should not fail");
        }
    }
}

fn emit_expr(expr: &LinearExpr, cx: &EmitCx<'_>) -> String {
    match expr {
        LinearExpr::Var(var) => format!("v{}", var.as_u32()),
        LinearExpr::Literal(lit) => emit_literal(lit),
        LinearExpr::Unary { op, expr } => match op {
            UnaryOp::Neg => format!("cv_neg({})", emit_expr(expr, cx)),
            UnaryOp::Not => format!("cv_not({})", emit_expr(expr, cx)),
        },
        LinearExpr::Binary { op, lhs, rhs } => {
            let lhs = emit_expr(lhs, cx);
            let rhs = emit_expr(rhs, cx);
            match op {
                BinaryOp::Add => format!("cv_add({}, {})", lhs, rhs),
                BinaryOp::Sub => format!("cv_sub({}, {})", lhs, rhs),
                BinaryOp::Mul => format!("cv_mul({}, {})", lhs, rhs),
                BinaryOp::Div => format!("cv_div({}, {})", lhs, rhs),
                BinaryOp::Mod => format!("cv_mod({}, {})", lhs, rhs),
                BinaryOp::Eq => format!("cv_eq({}, {})", lhs, rhs),
                BinaryOp::Ne => format!("cv_ne({}, {})", lhs, rhs),
                BinaryOp::Lt => format!("cv_lt({}, {})", lhs, rhs),
                BinaryOp::Le => format!("cv_le({}, {})", lhs, rhs),
                BinaryOp::Gt => format!("cv_gt({}, {})", lhs, rhs),
                BinaryOp::Ge => format!("cv_ge({}, {})", lhs, rhs),
                BinaryOp::And => format!("cv_and({}, {})", lhs, rhs),
                BinaryOp::Or => format!("cv_or({}, {})", lhs, rhs),
            }
        }
        LinearExpr::PureCall { callee, args } => {
            let callee_name = cx
                .fn_name_by_symbol
                .get(callee)
                .cloned()
                .unwrap_or_else(|| "cielo_fn_unknown".to_owned());
            let args = args
                .iter()
                .map(|arg| emit_expr(arg, cx))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}({})", callee_name, args)
        }
        LinearExpr::MakeStruct { ty, fields } => {
            let ty_name = escape_c_string(symbol_text(cx.interner, *ty).as_str());
            let fields = fields
                .iter()
                .map(|arg| emit_expr(arg, cx))
                .collect::<Vec<_>>();
            format_ctor_call(&ty_name, "", fields)
        }
        LinearExpr::MakeEnum {
            ty,
            variant,
            fields,
        } => {
            let ty_name = escape_c_string(symbol_text(cx.interner, *ty).as_str());
            let variant_name = escape_c_string(symbol_text(cx.interner, *variant).as_str());
            let fields = fields
                .iter()
                .map(|arg| emit_expr(arg, cx))
                .collect::<Vec<_>>();
            format_ctor_call(&ty_name, &variant_name, fields)
        }
        LinearExpr::Error => "cv_unit()".to_owned(),
    }
}

fn format_ctor_call(ty_name: &str, variant_name: &str, fields: Vec<String>) -> String {
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
            out.push_str(field);
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

fn collect_stmt_vars(stmt: &LinearStmt, out: &mut BTreeSet<VarId>) {
    match stmt {
        LinearStmt::Return(expr) => collect_expr_vars(expr, out),
        LinearStmt::Let {
            binding,
            value,
            next,
        } => {
            out.insert(*binding);
            collect_expr_vars(value, out);
            collect_stmt_vars(next, out);
        }
        LinearStmt::Val {
            binding,
            value,
            next,
        } => {
            out.insert(*binding);
            collect_stmt_vars(value, out);
            collect_stmt_vars(next, out);
        }
        LinearStmt::Call {
            result, args, next, ..
        } => {
            out.insert(*result);
            for arg in args {
                collect_expr_vars(arg, out);
            }
            collect_stmt_vars(next, out);
        }
        LinearStmt::If {
            cond,
            then_branch,
            else_branch,
        } => {
            collect_expr_vars(cond, out);
            collect_stmt_vars(then_branch, out);
            collect_stmt_vars(else_branch, out);
        }
        LinearStmt::Match {
            scrutinee,
            arms,
            default,
        } => {
            collect_expr_vars(scrutinee, out);
            for arm in arms {
                for binder in &arm.binders {
                    out.insert(*binder);
                }
                collect_stmt_vars(&arm.body, out);
            }
            if let Some(default_stmt) = default {
                collect_stmt_vars(default_stmt, out);
            }
        }
        LinearStmt::Perform {
            result, args, next, ..
        } => {
            if let Some(result) = result {
                out.insert(*result);
            }
            for arg in args {
                collect_expr_vars(arg, out);
            }
            collect_stmt_vars(next, out);
        }
        LinearStmt::Handle { body, next, .. } | LinearStmt::Stage { body, next, .. } => {
            collect_stmt_vars(body, out);
            if let Some(next_stmt) = next {
                collect_stmt_vars(next_stmt, out);
            }
        }
        LinearStmt::Hole | LinearStmt::Error => {}
    }
}

fn collect_expr_vars(expr: &LinearExpr, out: &mut BTreeSet<VarId>) {
    match expr {
        LinearExpr::Var(var) => {
            out.insert(*var);
        }
        LinearExpr::Unary { expr, .. } => collect_expr_vars(expr, out),
        LinearExpr::Binary { lhs, rhs, .. } => {
            collect_expr_vars(lhs, out);
            collect_expr_vars(rhs, out);
        }
        LinearExpr::PureCall { args, .. }
        | LinearExpr::MakeStruct { fields: args, .. }
        | LinearExpr::MakeEnum { fields: args, .. } => {
            for arg in args {
                collect_expr_vars(arg, out);
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

fn emit_c_prelude(out: &mut String) {
    out.push_str("#include <stdbool.h>\n");
    out.push_str("#include <stddef.h>\n");
    out.push_str("#include <stdint.h>\n");
    out.push('\n');
    out.push_str("typedef enum {\n");
    out.push_str("    CV_UNIT = 0,\n");
    out.push_str("    CV_BOOL = 1,\n");
    out.push_str("    CV_INT = 2,\n");
    out.push_str("    CV_FLOAT = 3,\n");
    out.push_str("    CV_CHAR = 4,\n");
    out.push_str("    CV_STRING = 5\n");
    out.push_str("} CieloTag;\n\n");
    out.push_str("typedef struct {\n");
    out.push_str("    CieloTag tag;\n");
    out.push_str("    union {\n");
    out.push_str("        bool b;\n");
    out.push_str("        int64_t i;\n");
    out.push_str("        double f;\n");
    out.push_str("        uint32_t c;\n");
    out.push_str("        const char* s;\n");
    out.push_str("    } as;\n");
    out.push_str("} CieloValue;\n\n");
    out.push_str(
        "static inline CieloValue cv_unit(void) { CieloValue v = { .tag = CV_UNIT }; return v; }\n",
    );
    out.push_str("static inline CieloValue cv_bool(int x) { CieloValue v = { .tag = CV_BOOL }; v.as.b = x != 0; return v; }\n");
    out.push_str("static inline CieloValue cv_int(int64_t x) { CieloValue v = { .tag = CV_INT }; v.as.i = x; return v; }\n");
    out.push_str("static inline CieloValue cv_float(double x) { CieloValue v = { .tag = CV_FLOAT }; v.as.f = x; return v; }\n");
    out.push_str("static inline CieloValue cv_char(uint32_t x) { CieloValue v = { .tag = CV_CHAR }; v.as.c = x; return v; }\n");
    out.push_str("static inline CieloValue cv_string(const char* s) { CieloValue v = { .tag = CV_STRING }; v.as.s = s; return v; }\n");
    out.push('\n');
    out.push_str("static inline bool cv_truthy(CieloValue v) {\n");
    out.push_str("    switch (v.tag) {\n");
    out.push_str("        case CV_BOOL: return v.as.b;\n");
    out.push_str("        case CV_INT: return v.as.i != 0;\n");
    out.push_str("        case CV_FLOAT: return v.as.f != 0.0;\n");
    out.push_str("        case CV_UNIT: return false;\n");
    out.push_str("        default: return true;\n");
    out.push_str("    }\n");
    out.push_str("}\n\n");
    out.push_str("static inline CieloValue cv_neg(CieloValue a) { return cv_int(-a.as.i); }\n");
    out.push_str(
        "static inline CieloValue cv_not(CieloValue a) { return cv_bool(!cv_truthy(a)); }\n",
    );
    out.push_str("static inline CieloValue cv_add(CieloValue a, CieloValue b) { return cv_int(a.as.i + b.as.i); }\n");
    out.push_str("static inline CieloValue cv_sub(CieloValue a, CieloValue b) { return cv_int(a.as.i - b.as.i); }\n");
    out.push_str("static inline CieloValue cv_mul(CieloValue a, CieloValue b) { return cv_int(a.as.i * b.as.i); }\n");
    out.push_str("static inline CieloValue cv_div(CieloValue a, CieloValue b) { return cv_int(b.as.i == 0 ? 0 : a.as.i / b.as.i); }\n");
    out.push_str("static inline CieloValue cv_mod(CieloValue a, CieloValue b) { return cv_int(b.as.i == 0 ? 0 : a.as.i % b.as.i); }\n");
    out.push_str("static inline CieloValue cv_eq(CieloValue a, CieloValue b) {\n");
    out.push_str("    if (a.tag != b.tag) return cv_bool(0);\n");
    out.push_str("    switch (a.tag) {\n");
    out.push_str("        case CV_UNIT: return cv_bool(1);\n");
    out.push_str("        case CV_BOOL: return cv_bool(a.as.b == b.as.b);\n");
    out.push_str("        case CV_INT: return cv_bool(a.as.i == b.as.i);\n");
    out.push_str("        case CV_FLOAT: return cv_bool(a.as.f == b.as.f);\n");
    out.push_str("        case CV_CHAR: return cv_bool(a.as.c == b.as.c);\n");
    out.push_str("        case CV_STRING: return cv_bool(a.as.s == b.as.s);\n");
    out.push_str("    }\n");
    out.push_str("    return cv_bool(0);\n");
    out.push_str("}\n");
    out.push_str("static inline CieloValue cv_ne(CieloValue a, CieloValue b) { CieloValue eq = cv_eq(a, b); return cv_bool(!eq.as.b); }\n");
    out.push_str("static inline CieloValue cv_lt(CieloValue a, CieloValue b) { return cv_bool(a.as.i < b.as.i); }\n");
    out.push_str("static inline CieloValue cv_le(CieloValue a, CieloValue b) { return cv_bool(a.as.i <= b.as.i); }\n");
    out.push_str("static inline CieloValue cv_gt(CieloValue a, CieloValue b) { return cv_bool(a.as.i > b.as.i); }\n");
    out.push_str("static inline CieloValue cv_ge(CieloValue a, CieloValue b) { return cv_bool(a.as.i >= b.as.i); }\n");
    out.push_str("static inline CieloValue cv_and(CieloValue a, CieloValue b) { return cv_bool(cv_truthy(a) && cv_truthy(b)); }\n");
    out.push_str("static inline CieloValue cv_or(CieloValue a, CieloValue b) { return cv_bool(cv_truthy(a) || cv_truthy(b)); }\n");
    out.push('\n');
    out.push_str("static CieloValue cielo_perform(uint32_t effect, const char* op, size_t argc, const CieloValue* args) {\n");
    out.push_str("    (void)effect; (void)op; (void)argc; (void)args;\n");
    out.push_str("    return cv_unit();\n");
    out.push_str("}\n");
    out.push_str("static CieloValue cielo_make_ctor(const char* ty, const char* variant, size_t argc, const CieloValue* fields) {\n");
    out.push_str("    (void)ty; (void)variant; (void)argc; (void)fields;\n");
    out.push_str("    return cv_unit();\n");
    out.push_str("}\n\n");
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
    fn_name_by_symbol: &'a HashMap<SymbolId, String>,
    interner: &'a Interner,
}

#[derive(Clone, Copy)]
enum EmitMode {
    Return,
    Assign(VarId),
    Discard,
}
