use std::collections::HashMap;
use std::fmt::Write;

use cielo_base::symbols::Interner;
use cielo_ir::constants::{
    ConstantEmbedStrategy, ConstantKey, ConstantTable, CtorFieldKey, CtorLiteralKey,
    ScalarLiteralKey,
};

#[derive(Clone, Debug, Default)]
pub struct CConstantPools {
    pub declarations: String,
    scalars: HashMap<ScalarLiteralKey, String>,
    strings: HashMap<String, String>,
    ctors: HashMap<CtorLiteralKey, String>,
}

impl CConstantPools {
    pub fn build(table: &ConstantTable, interner: &Interner) -> Self {
        let mut result = Self::default();
        let mut scalar_index = 0usize;
        let mut string_index = 0usize;
        let mut ctor_index = 0usize;
        let mut nested_index = 0usize;
        for entry in &table.entries {
            match (&entry.key, entry.strategy) {
                (ConstantKey::Scalar(key), ConstantEmbedStrategy::StaticConst) => {
                    let symbol = format!("cielo_const_v_{scalar_index}");
                    scalar_index += 1;
                    let initializer = scalar_initializer(*key);
                    writeln!(
                        result.declarations,
                        "static const CieloValue {symbol} = {initializer};"
                    )
                    .expect("in-memory write");
                    result.scalars.insert(*key, symbol);
                }
                (ConstantKey::String(value), ConstantEmbedStrategy::StaticConst) => {
                    let symbol = format!("cielo_const_s_{string_index}");
                    string_index += 1;
                    writeln!(
                        result.declarations,
                        "static const char* {symbol} = \"{}\";",
                        escape(value)
                    )
                    .expect("in-memory write");
                    result.strings.insert(value.clone(), symbol);
                }
                (ConstantKey::Ctor(key), ConstantEmbedStrategy::Pooled) => {
                    let value_symbol = format!("cielo_const_ctor_v_{ctor_index}");
                    let ctor_symbol = format!("cielo_const_ctor_{ctor_index}");
                    let fields_symbol = format!("cielo_const_ctor_fields_{ctor_index}");
                    ctor_index += 1;
                    let mut fields = Vec::new();
                    for field in &key.fields {
                        fields.push(render_field(
                            field,
                            interner,
                            &mut nested_index,
                            &mut result.declarations,
                        ));
                    }
                    let fields_ref = if fields.is_empty() {
                        "NULL".to_owned()
                    } else {
                        writeln!(
                            result.declarations,
                            "static CieloValue {fields_symbol}[] = {{{}}};",
                            fields.join(", ")
                        )
                        .expect("in-memory write");
                        fields_symbol
                    };
                    writeln!(
                        result.declarations,
                        "static CieloCtor {ctor_symbol} = {{ .arc = CIELO_ARC_IMMORTAL_HEADER, .ty = \"{}\", .variant = \"{}\", .argc = {}, .fields = {fields_ref} }};",
                        escape(interner.resolve(key.ty).unwrap_or("unknown")),
                        if key.variant.is_valid() {
                            escape(interner.resolve(key.variant).unwrap_or("unknown"))
                        } else {
                            String::new()
                        },
                        key.fields.len()
                    )
                    .expect("in-memory write");
                    writeln!(
                        result.declarations,
                        "static const CieloValue {value_symbol} = {{ .tag = CV_CTOR, .as.ctor = &{ctor_symbol} }};"
                    )
                    .expect("in-memory write");
                    result.ctors.insert(key.clone(), value_symbol);
                }
                _ => {}
            }
        }
        result
    }

    pub fn scalar(&self, key: ScalarLiteralKey) -> Option<&str> {
        self.scalars.get(&key).map(String::as_str)
    }

    pub fn string(&self, value: &str) -> Option<&str> {
        self.strings.get(value).map(String::as_str)
    }

    pub fn ctor(&self, key: &CtorLiteralKey) -> Option<&str> {
        self.ctors.get(key).map(String::as_str)
    }
}

fn render_field(
    field: &CtorFieldKey,
    interner: &Interner,
    nested_index: &mut usize,
    declarations: &mut String,
) -> String {
    match field {
        CtorFieldKey::Unit => "{ .tag = CV_UNIT }".to_owned(),
        CtorFieldKey::Bool(value) => format!(
            "{{ .tag = CV_BOOL, .as.b = {} }}",
            if *value { "true" } else { "false" }
        ),
        CtorFieldKey::Int(value) => format!("{{ .tag = CV_INT, .as.i = {value} }}"),
        CtorFieldKey::Float(bits) => format!(
            "{{ .tag = CV_FLOAT, .as.f = {} }}",
            float_literal(f64::from_bits(*bits))
        ),
        CtorFieldKey::Char(value) => {
            format!("{{ .tag = CV_CHAR, .as.c = {}u }}", *value as u32)
        }
        CtorFieldKey::String(value) => {
            format!("{{ .tag = CV_STRING, .as.s = \"{}\" }}", escape(value))
        }
        CtorFieldKey::Ctor(key) => {
            let id = *nested_index;
            *nested_index += 1;
            let fields_symbol = format!("cielo_const_ctor_nested_fields_{id}");
            let ctor_symbol = format!("cielo_const_ctor_nested_{id}");
            let mut fields = Vec::new();
            for field in &key.fields {
                fields.push(render_field(field, interner, nested_index, declarations));
            }
            let fields_ref = if fields.is_empty() {
                "NULL".to_owned()
            } else {
                writeln!(
                    declarations,
                    "static CieloValue {fields_symbol}[] = {{{}}};",
                    fields.join(", ")
                )
                .expect("in-memory write");
                fields_symbol
            };
            writeln!(
                declarations,
                "static CieloCtor {ctor_symbol} = {{ .arc = CIELO_ARC_IMMORTAL_HEADER, .ty = \"{}\", .variant = \"{}\", .argc = {}, .fields = {fields_ref} }};",
                escape(interner.resolve(key.ty).unwrap_or("unknown")),
                escape(interner.resolve(key.variant).unwrap_or("unknown")),
                key.fields.len()
            )
            .expect("in-memory write");
            format!("{{ .tag = CV_CTOR, .as.ctor = &{ctor_symbol} }}")
        }
    }
}

fn scalar_initializer(key: ScalarLiteralKey) -> String {
    match key {
        ScalarLiteralKey::Bool(value) => format!(
            "{{ .tag = CV_BOOL, .as.b = {} }}",
            if value { "true" } else { "false" }
        ),
        ScalarLiteralKey::Int(value) => format!("{{ .tag = CV_INT, .as.i = {value} }}"),
        ScalarLiteralKey::Float(bits) => format!(
            "{{ .tag = CV_FLOAT, .as.f = {} }}",
            float_literal(f64::from_bits(bits))
        ),
        ScalarLiteralKey::Char(value) => {
            format!("{{ .tag = CV_CHAR, .as.c = {}u }}", value as u32)
        }
    }
}

fn escape(raw: &str) -> String {
    let mut out = String::new();
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
    let mut text = format!("{value:?}");
    if !text.contains('.') && !text.contains('e') && !text.contains('E') {
        text.push_str(".0");
    }
    text
}
