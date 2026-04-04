use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{Parser, ValueEnum};

use cielo::common::ids::{ExprId, SourceId, SymbolId};
use cielo::common::reporting::render_diagnostic;
use cielo::common::symbols::Interner;
use cielo::frontend::ast::{Item, Program};
use cielo::ir::core::CoreProgram;
use cielo::pipeline::ct_invalidation::{
    CtDepSnapshot, CtInvalidationReason, diff as diff_ct_invalidation,
    load_snapshot as load_ct_snapshot, save_snapshot as save_ct_snapshot,
    sidecar_path as ct_sidecar_path,
};
use cielo::pipeline::phases::{Residualized, Stage};
use cielo::pipeline::provenance::runtime_provenance_lines;
use cielo::pipeline::staging_diagnostics::{render_stage_b_counter_summary, staging_pass_counters};
use cielo::pipeline::staging_diff::{
    SnapshotStage, collect_snapshot, diff_snapshots, load_snapshot as load_stage_snapshot,
    save_snapshot as save_stage_snapshot,
};
use cielo::{Compiler, CompilerConfig};

const RUNTIME_HEADER: &str = cielo::RUNTIME_HEADER;

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

    #[arg(long, help = "Persist and diff staging snapshots across rebuilds")]
    staging_diff: bool,

    #[arg(long, default_value = "/tmp/cielo_staging_snapshot.tsv")]
    staging_snapshot: PathBuf,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum, Debug)]
enum DumpKind {
    Ast,
    Core,
    Functions,
    Effects,
    Sema,
    StagingReport,
    Linear,
    Cfg,
    C,
    Memory,
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

    let source_id = SourceId::from_u32(0);
    let db_source = compiler.database_source(&source, source_id);

    let mut interner = Interner::new();
    let parsed = compiler.database_parse_file(db_source, &mut interner);
    let core = compiler.database_lower_file(db_source, &mut interner);

    if should_dump(cli, DumpKind::Ast) {
        println!("=== AST ===\n{:#?}", parsed.program());
    }
    if should_dump(cli, DumpKind::Effects) {
        dump_effects_from_ast(parsed.program(), &interner);
    }
    if should_dump(cli, DumpKind::Core) {
        println!("=== Core IR ===\n{:#?}", core.program());
    }
    if should_dump(cli, DumpKind::Functions) {
        dump_functions_from_core(core.program(), &interner);
    }

    let staged = compiler.database_staged_file(db_source);
    let residual = &staged.residual;
    if should_dump(cli, DumpKind::Sema) {
        dump_sema_summary(residual);
    }
    if cli.staging_diff {
        emit_staging_diff(cli.staging_snapshot.as_path(), residual);
    }
    let source_name = path.display().to_string();
    print_case_summary("file", &source_name, &source, residual);

    if should_dump(cli, DumpKind::StagingReport) {
        print_staging_report(residual);
    }

    let need_runtime = should_dump(cli, DumpKind::Linear)
        || should_dump(cli, DumpKind::Cfg)
        || cli.emit_c
        || cli.run_c
        || should_dump(cli, DumpKind::C);
    if residual.diagnostics().has_errors() {
        if need_runtime {
            eprintln!("skipping runtime lowering because diagnostics contain errors");
        }
        std::process::exit(1);
    }
    if !need_runtime {
        if should_dump(cli, DumpKind::Memory) {
            let memory = compiler.database_memory_file(db_source);
            println!("=== Memory ===\n{memory:#?}");
        }
        return;
    }

    let runtime = compiler.database_runtime_file(db_source);
    if should_dump(cli, DumpKind::Linear) {
        println!("=== Linear IR ===\n{:#?}", runtime.linear);
    }
    if should_dump(cli, DumpKind::Cfg) {
        println!("=== CFG IR ===\n{:#?}", runtime.cfg);
    }

    if should_dump(cli, DumpKind::Memory) {
        let memory = compiler.database_memory_file(db_source);
        println!("=== Memory ===\n{memory:#?}");
    }

    if cli.emit_c || cli.run_c || should_dump(cli, DumpKind::C) {
        let emitted = compiler.database_emitted_file(db_source);
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
}

fn print_staging_report(residual: &Residualized) {
    println!("=== Staging Report ===");
    let rollups = cielo::pipeline::provenance::staging_root_causes(residual.program(), residual.bta());
    let ct_count = residual
        .bta()
        .stage_of_expr
        .values()
        .filter(|stage| matches!(stage, Stage::Ct))
        .count();
    let total = residual.program().exprs().len();
    println!(
        "{}/{} expressions evaluated at compile time ({:.1}%)\n",
        ct_count,
        total,
        (ct_count as f64 / total as f64) * 100.0
    );
    println!(
        "{}\n",
        render_stage_b_counter_summary(staging_pass_counters(residual))
    );
    println!("Top RT root causes:");
    for (index, rollup) in rollups.iter().enumerate().take(10) {
        println!(
            "  {}. `{}` → taints {} expressions",
            index + 1,
            rollup.description,
            rollup.taint_count
        );
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
    let known_local = residual
        .bta()
        .knownness_of_expr
        .values()
        .filter(|known| matches!(known, cielo::pipeline::phases::Knownness::KnownLocal))
        .count();
    let known_persistable = residual
        .bta()
        .knownness_of_expr
        .values()
        .filter(|known| matches!(known, cielo::pipeline::phases::Knownness::KnownPersistable))
        .count();
    let eval = residual.ct().eval_stats;
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
    println!("known local={known_local}, known persistable={known_persistable}");
    println!(
        "ct-eval iter={}, attempts={}, hits={}, folds(lit/unary/bin)={}/{}/{}, host-float-folds={}, misses(dep/unsupported)={}/{}",
        eval.iterations,
        eval.eval_attempts,
        eval.cache_hits,
        eval.folded_literals,
        eval.folded_unary,
        eval.folded_binary,
        eval.folded_float_host,
        eval.miss_missing_inputs,
        eval.miss_unsupported
    );
    if eval.folded_float_host > 0 {
        println!(
            "ct-eval caveat: float folds currently use host FP behavior (strict target emulation deferred)"
        );
    }
    println!(
        "{}",
        render_stage_b_counter_summary(staging_pass_counters(residual))
    );

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

fn emit_staging_diff(snapshot_path: &Path, residual: &Residualized) {
    let current = collect_snapshot(residual.program(), residual.bta());
    let previous = load_stage_snapshot(snapshot_path).unwrap_or_default();
    let changes = diff_snapshots(previous.as_slice(), current.as_slice());

    println!("=== Staging Diff ===");
    if changes.is_empty() {
        println!("(no stage changes)");
    } else {
        for change in &changes {
            println!(
                "- {}: {} -> {}",
                change.stable_id,
                stage_text(change.before),
                stage_text(change.after),
            );
            if !change.before_reason.is_empty() || !change.after_reason.is_empty() {
                println!(
                    "  reason: {} -> {}",
                    if change.before_reason.is_empty() {
                        "<none>"
                    } else {
                        change.before_reason.as_str()
                    },
                    if change.after_reason.is_empty() {
                        "<none>"
                    } else {
                        change.after_reason.as_str()
                    }
                );
            }
        }
    }

    if let Err(err) = save_stage_snapshot(snapshot_path, current.as_slice()) {
        eprintln!(
            "failed to persist staging snapshot {}: {err}",
            snapshot_path.display()
        );
    }

    let dep_path = ct_sidecar_path(snapshot_path);
    let previous_ct = load_ct_snapshot(dep_path.as_path()).unwrap_or_default();
    let mut current_ct = CtDepSnapshot {
        cache_key: residual.ct().cache_key.clone(),
        file_deps: residual.ct().file_deps.clone(),
    };
    current_ct.file_deps.sort_by(|lhs, rhs| {
        lhs.path
            .cmp(&rhs.path)
            .then(lhs.content_hash.cmp(&rhs.content_hash))
    });
    let invalidations = diff_ct_invalidation(&previous_ct, &current_ct);

    println!("=== ComptimeReadFiles Invalidation ===");
    if invalidations.is_empty() {
        println!("(no invalidations)");
    } else {
        for reason in &invalidations {
            println!("- {}", invalidation_text(reason));
        }
    }

    if let Err(err) = save_ct_snapshot(dep_path.as_path(), &current_ct) {
        eprintln!(
            "failed to persist ct dependency snapshot {}: {err}",
            dep_path.display()
        );
    }
}

fn stage_text(stage: SnapshotStage) -> &'static str {
    stage.display_text()
}

fn invalidation_text(reason: &CtInvalidationReason) -> String {
    reason.describe()
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
        let residual = compiler.compile_source(source, SourceId::new(idx), &mut interner);
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
