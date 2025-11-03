use std::fs;

use cielo::common::ids::SourceId;
use cielo::common::reporting::render_diagnostic;
use cielo::common::symbols::Interner;
use cielo::{Compiler, CompilerConfig};

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
    const CASES: [(&str, &str); 6] = [
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
        (
            "adt",
            r#"
enum Option { Some(Int), None }
struct Pair { a: Int, b: Int }
fn main() -> Int {
  let x = Some(1);
  let y = Pair(1, 2);
  0
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

    for diag in residual.diagnostics.entries() {
        let rendered = render_diagnostic(diag, source_name, source);
        if rendered.trim().is_empty() {
            println!(
                "  - {:?} {} @src{} bytes[{}..{}]: {}",
                diag.severity,
                diag.code,
                diag.span.source,
                diag.span.start,
                diag.span.end,
                diag.message
            );
            continue;
        }
        for line in rendered.lines() {
            println!("  {line}");
        }
    }
}
