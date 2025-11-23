use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Parser, ValueEnum};

use cielo::common::ids::{ExprId, SourceId, SymbolId};
use cielo::common::reporting::render_diagnostic;
use cielo::common::symbols::Interner;
use cielo::frontend::ast::{Item, Program};
use cielo::ir::core::CoreProgram;
use cielo::passes::lowering::LowerConfig;
use cielo::passes::{c_emit, handler_specialize, linearize};
use cielo::pipeline::phases::{Residualized, Stage};
use cielo::pipeline::provenance::runtime_provenance_lines;
use cielo::{Compiler, CompilerConfig};

const RUNTIME_HEADER: &str = include_str!("backend/cielo_runtime.h");

#[derive(Parser, Debug)]
#[command(name = "cielo", about = "cielo v0 compiler driver")]
struct Cli {
    #[arg(value_name = "INPUT")]
    input: Option<PathBuf>,

    #[arg(long, value_enum, value_delimiter = ',')]
    dump: Vec<DumpKind>,

    #[arg(long, help = "Print emitted C and save it to --c-out")]
    emit_c: bool,

    #[arg(long, help = "Compile saved C with --cc and run --bin-out")]
    run_c: bool,

    #[arg(long, default_value = "/tmp/cielo_out.c")]
    c_out: PathBuf,

    #[arg(long, default_value = "/tmp/cielo_out.bin")]
    bin_out: PathBuf,

    #[arg(long, default_value = "gcc")]
    cc: String,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum, Debug)]
enum DumpKind {
    Ast,
    Core,
    Functions,
    Effects,
    Sema,
    Linear,
    Anf,
    C,
}

fn main() {
    let cli = Cli::parse();
    let compiler = Compiler::new(CompilerConfig::default());
    let target = compiler.config().target;
    println!(
        "cielo bootstrap ready (target: {}-bit {:?})",
        target.word_size_bits, target.endianness
    );

    match &cli.input {
        Some(path) => run_input_case(&compiler, &cli, path),
        None => {
            if cli.emit_c || cli.run_c || !cli.dump.is_empty() {
                eprintln!("input file is required for --dump/--emit-c/--run-c");
                std::process::exit(1);
            }
            run_smoke_cases(&compiler);
        }
    }
}

fn run_input_case(compiler: &Compiler, cli: &Cli, path: &Path) {
    let source = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("failed to read {}: {err}", path.display());
            std::process::exit(1);
        }
    };

    let mut interner = Interner::new();
    let parsed = compiler.parse(&source, SourceId::from_u32(0), &mut interner);

    if should_dump(cli, DumpKind::Ast) {
        println!("=== AST ===\n{:#?}", parsed.ast());
    }
    if should_dump(cli, DumpKind::Effects) {
        dump_effects_from_ast(&parsed.ast(), &interner);
    }

    let main_symbol = interner.intern("main");
    let core = compiler
        .lower_parsed_to_core_with_config(parsed, LowerConfig::with_entrypoint(main_symbol));

    if should_dump(cli, DumpKind::Core) {
        println!("=== Core IR ===\n{:#?}", core.program());
    }
    if should_dump(cli, DumpKind::Functions) {
        dump_functions_from_core(&core.program(), &interner);
    }

    let residual = compiler.run_v0_core_pipeline(core);
    if should_dump(cli, DumpKind::Sema) {
        dump_sema_summary(&residual);
    }
    if should_dump(cli, DumpKind::Anf) {
        println!("=== ANF ===");
        println!("not implemented in v0 yet");
    }

    let source_name = path.display().to_string();
    print_case_summary("file", &source_name, &source, &residual);

    let need_c_backend = cli.emit_c
        || cli.run_c
        || should_dump(cli, DumpKind::Linear)
        || should_dump(cli, DumpKind::C);

    if residual.diagnostics().has_errors() {
        if need_c_backend {
            eprintln!("skipping C backend because diagnostics contain errors");
        }
        std::process::exit(1);
    }

    if !need_c_backend {
        return;
    }

    let specialized = handler_specialize::run(residual.clone());
    let linearized = linearize::run(specialized);
    if should_dump(cli, DumpKind::Linear) {
        println!("=== Linear IR ===\n{:#?}", linearized.linear);
    }

    let emitted = c_emit::run(linearized, &interner);
    if should_dump(cli, DumpKind::C) || cli.emit_c || cli.run_c {
        println!("=== Emitted C ===\n{}", emitted.c_source);
    }

    if cli.emit_c || cli.run_c {
        save_emitted_c(cli, &emitted.c_source);
    }
    if cli.run_c {
        compile_and_run_c(cli);
    }
}

fn should_dump(cli: &Cli, kind: DumpKind) -> bool {
    cli.dump.contains(&kind)
}

fn save_emitted_c(cli: &Cli, c_source: &str) {
    if let Some(parent) = cli.c_out.parent()
        && !parent.as_os_str().is_empty()
        && let Err(err) = fs::create_dir_all(parent)
    {
        eprintln!(
            "failed to create output directory {}: {err}",
            parent.display()
        );
        std::process::exit(1);
    }
    if let Err(err) = fs::write(&cli.c_out, c_source) {
        eprintln!("failed to write C source {}: {err}", cli.c_out.display());
        std::process::exit(1);
    }

    let header_path = cli
        .c_out
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("cielo_runtime.h");
    if let Err(err) = fs::write(&header_path, RUNTIME_HEADER) {
        eprintln!(
            "failed to write runtime header {}: {err}",
            header_path.display()
        );
        std::process::exit(1);
    }

    println!("saved C source: {}", cli.c_out.display());
    println!("saved runtime header: {}", header_path.display());
}

fn compile_and_run_c(cli: &Cli) {
    let compile_status = match Command::new(&cli.cc)
        .arg("-std=c11")
        .arg(&cli.c_out)
        .arg("-o")
        .arg(&cli.bin_out)
        .status()
    {
        Ok(status) => status,
        Err(err) => {
            eprintln!("failed to invoke {}: {err}", cli.cc);
            std::process::exit(1);
        }
    };
    if !compile_status.success() {
        let code = compile_status.code().unwrap_or(1);
        eprintln!("{} failed with status {}", cli.cc, code);
        std::process::exit(code);
    }
    println!("compiled binary: {}", cli.bin_out.display());

    let run_status = match Command::new(&cli.bin_out).status() {
        Ok(status) => status,
        Err(err) => {
            eprintln!("failed to run {}: {err}", cli.bin_out.display());
            std::process::exit(1);
        }
    };
    let code = run_status.code().unwrap_or(-1);
    println!("program exit status: {code}");
}

fn dump_effects_from_ast(ast: &Program, interner: &Interner) {
    println!("=== Registered Effects ===");
    let mut any = false;
    for item in &ast.items {
        if let Item::Effect(effect) = item {
            any = true;
            println!(
                "- {} (ops={})",
                symbol_name(interner, effect.name),
                effect.operations.len()
            );
            for operation in &effect.operations {
                println!(
                    "  op {}(params={})",
                    symbol_name(interner, operation.name),
                    operation.params.len()
                );
            }
        }
    }
    if !any {
        println!("(none)");
    }
}

fn dump_functions_from_core(program: &CoreProgram, interner: &Interner) {
    println!("=== Registered Functions ===");
    if program.functions().is_empty() {
        println!("(none)");
        return;
    }
    for (idx, function) in program.functions().iter().enumerate() {
        println!(
            "- #{} {}(params={}, effects={:?}, entry={})",
            idx,
            symbol_name(interner, function.name),
            function.params.len(),
            function.declared_effects,
            program.entrypoints().iter().any(|id| id.index() == idx)
        );
    }
}

fn dump_sema_summary(residual: &Residualized) {
    let typed_exprs = residual
        .sema()
        .type_of_expr
        .iter()
        .filter(|slot| slot.is_some())
        .count();
    let effectful_stmts = residual
        .sema()
        .effects_of_stmt
        .iter()
        .filter(|row| !row.is_empty())
        .count();
    let ct_exprs = residual
        .bta()
        .stage_of_expr
        .values()
        .filter(|stage| matches!(stage, Stage::Ct))
        .count();
    let rt_exprs = residual.bta().stage_of_expr.len().saturating_sub(ct_exprs);
    println!("=== Sema/BTA Summary ===");
    println!(
        "typed_exprs={typed_exprs}/{}",
        residual.program().exprs().len()
    );
    println!(
        "effectful_stmts={effectful_stmts}/{}",
        residual.program().stmts().len()
    );
    println!("stage ct={ct_exprs}, rt={rt_exprs}");

    if rt_exprs == 0 {
        return;
    }

    println!("runtime provenance (sample):");
    let mut shown = 0usize;
    for idx in 0..residual.program().exprs().len() {
        let expr_id = ExprId::new(idx);
        if !matches!(
            residual.bta().stage_of_expr.get(&expr_id),
            Some(Stage::Rt(_))
        ) {
            continue;
        }
        let chain = runtime_provenance_lines(residual.program(), residual.bta(), expr_id, 5);
        if chain.is_empty() {
            continue;
        }
        println!("  e{}:", expr_id.as_u32());
        for line in chain {
            println!("    {line}");
        }
        shown += 1;
        if shown >= 3 {
            break;
        }
    }
}

fn symbol_name(interner: &Interner, symbol: SymbolId) -> String {
    interner.resolve(symbol).unwrap_or("<invalid>").to_owned()
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
        if residual.diagnostics().has_errors() {
            failures += 1;
        }
    }

    if failures > 0 {
        eprintln!("smoke failed: {failures} case(s)");
        std::process::exit(1);
    }
}

fn print_case_summary(name: &str, source_name: &str, source: &str, residual: &Residualized) {
    let ct_exprs = residual
        .bta()
        .stage_of_expr
        .values()
        .filter(|stage| matches!(stage, Stage::Ct))
        .count();
    let rt_exprs = residual.bta().stage_of_expr.len().saturating_sub(ct_exprs);
    println!(
        "[{name}] funcs={}, exprs={}, ct={}, rt={}, diags={}",
        residual.program().functions().len(),
        residual.program().exprs().len(),
        ct_exprs,
        rt_exprs,
        residual.diagnostics().entries().len()
    );

    for diag in residual.diagnostics().entries() {
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
