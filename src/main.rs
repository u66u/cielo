use std::fs;

use cielo::common::diagnostics::Severity;
use cielo::common::ids::SourceId;
use cielo::common::symbols::Interner;
use cielo::{Compiler, CompilerConfig};
use miette::{GraphicalReportHandler, LabeledSpan, MietteDiagnostic, NamedSource, Report};

fn main() {
    let compiler = Compiler::new(CompilerConfig::default());
    let target = compiler.config().target;
    println!(
        "cielo bootstrap ready (target: {}-bit {:?})",
        target.word_size_bits, target.endianness
    );

    let mut args = std::env::args().skip(1);
    if let Some(path) = args.next() {
        run_file_case(&compiler, &path);
        return;
    }

    run_smoke_cases(&compiler);
}

fn run_file_case(compiler: &Compiler, path: &str) {
    let source = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("failed to read {path}: {err}");
            std::process::exit(1);
        }
    };

    let mut interner = Interner::new();
    let residual = compiler.compile_source_v0(&source, SourceId::from_u32(0), &mut interner);
    print_case_summary("file", path, &source, &residual);

    if residual.diagnostics.has_errors() {
        std::process::exit(1);
    }
}

fn run_smoke_cases(compiler: &Compiler) {
    const CASES: [(&str, &str); 5] = [
        (
            "arith",
            r#"
fn main() -> Int {
  let x = 1 + 2;
  x
}
"#,
        ),
        (
            "effects",
            r#"
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  do Console.print("hello");
  0
}
"#,
        ),
        (
            "mixed",
            r#"
fn add(a: Int, b: Int) -> Int {
  a + b
}
fn main() -> Int {
  add(3, 4)
}
"#,
        ),
        (
            "handled",
            r#"
effect Console { fn print(s: String) -> () }
fn main() -> Int {
  let x = handle { do Console.print("hi"); 7 } with Console {
    | print(s) => 0
  };
  x
}
"#,
        ),
        (
            "staged",
            r#"
fn main() -> Int {
  let a = @runtime { 1 + 2 };
  let b = @comptime { a };
  b
}
"#,
        ),
    ];

    let mut failures = 0usize;
    for (idx, (name, source)) in CASES.iter().enumerate() {
        let mut interner = Interner::new();
        let residual = compiler.compile_source_v0(source, SourceId::new(idx), &mut interner);
        let source_name = format!("smoke/{name}.cielo");
        print_case_summary(name, &source_name, source, &residual);
        if residual.diagnostics.has_errors() {
            failures += 1;
        }
    }

    if failures > 0 {
        eprintln!("smoke failed: {failures} case(s)");
        std::process::exit(1);
    }
}

fn print_case_summary(
    name: &str,
    source_name: &str,
    source: &str,
    residual: &cielo::pipeline::phases::Residualized,
) {
    let ct_exprs = residual
        .bta
        .stage_of_expr
        .values()
        .filter(|stage| matches!(stage, cielo::pipeline::phases::Stage::Ct))
        .count();
    let rt_exprs = residual.bta.stage_of_expr.len().saturating_sub(ct_exprs);
    println!(
        "[{name}] funcs={}, exprs={}, ct={}, rt={}, diags={}",
        residual.program.functions().len(),
        residual.program.exprs().len(),
        ct_exprs,
        rt_exprs,
        residual.diagnostics.entries().len()
    );

    let handler = GraphicalReportHandler::new()
        .with_width(120)
        .with_context_lines(1)
        .without_cause_chain();

    for diag in residual.diagnostics.entries() {
        let report = diagnostic_report(diag, source_name, source);
        let mut rendered = String::new();
        if handler.render_report(&mut rendered, &*report).is_ok() {
            for line in rendered.lines() {
                println!("  {line}");
            }
        } else {
            println!(
                "  - {:?} {} @src{} bytes[{}..{}]: {}",
                diag.severity,
                diag.code,
                diag.span.source,
                diag.span.start,
                diag.span.end,
                diag.message
            );
        }
    }
}

fn diagnostic_report(
    diag: &cielo::common::diagnostics::Diagnostic,
    source_name: &str,
    source: &str,
) -> Report {
    let source_len = source.len();
    let start = (diag.span.start as usize).min(source_len);
    let end = (diag.span.end as usize).min(source_len);
    let span_len = end.saturating_sub(start);
    let label = if span_len == 0 {
        LabeledSpan::at_offset(start, diag.message.clone())
    } else {
        LabeledSpan::new_primary_with_span(Some(diag.message.clone()), (start, span_len))
    };
    let severity = match diag.severity {
        Severity::Error => miette::Severity::Error,
        Severity::Warning => miette::Severity::Warning,
        Severity::Note => miette::Severity::Advice,
    };
    let diagnostic = MietteDiagnostic::new(diag.message.clone())
        .with_code(format!("cielo::{}", diag.code))
        .with_severity(severity)
        .with_label(label);
    Report::new(diagnostic).with_source_code(NamedSource::new(source_name, source.to_owned()))
}
