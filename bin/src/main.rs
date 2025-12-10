use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use cielo::common::ids::{EffectLabelId, ExprId, FuncId, HandlerId, SourceId, StmtId, TypeId};
use cielo::common::reporting::render_diagnostic;
use cielo::common::span::Span;
use cielo::common::symbols::Interner;
use cielo::ir::core::{CoreProgram, CoreTypeRef, ExprKind, PrimitiveTypeRef, StmtKind};
use cielo::ir::linear::LinearProgram;
use cielo::passes::lowering::LowerConfig;
use cielo::passes::{c_emit, handler_specialize, linearize};
use cielo::pipeline::phases::{CoreBuilt, Knownness, Parsed, Reason, Residualized, Stage};
use cielo::pipeline::provenance::runtime_provenance_lines;
use cielo::sema::effect::{CapabilityLevel, EffectFlags, EffectProperties, SortedEffectRow};
use cielo::sema::ty::Persistability;
use cielo::{Compiler, CompilerConfig};

const DEFAULT_ENTRYPOINT: &str = "main";
const DEFAULT_LIST_LIMIT: usize = 50;
const DEFAULT_PROVENANCE_LIMIT: usize = 5;

fn main() {
    let mut entrypoint = DEFAULT_ENTRYPOINT.to_owned();
    let mut input: Option<PathBuf> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print_usage();
                return;
            }
            "--entry" => {
                let Some(value) = args.next() else {
                    eprintln!("missing value for --entry");
                    std::process::exit(1);
                };
                entrypoint = value;
            }
            _ if arg.starts_with('-') => {
                eprintln!("unknown flag: {arg}");
                std::process::exit(1);
            }
            _ => {
                if input.is_some() {
                    eprintln!("only one optional input file can be provided");
                    std::process::exit(1);
                }
                input = Some(PathBuf::from(arg));
            }
        }
    }

    let mut repl = Repl::new(entrypoint);
    println!(
        "cielo-repl | entrypoint={} | type :help for commands",
        repl.entrypoint
    );

    if let Some(path) = input
        && let Err(err) = repl.load_from_path(path.as_path())
    {
        eprintln!("{err}");
    }

    if let Err(err) = repl.run() {
        eprintln!("repl error: {err}");
        std::process::exit(1);
    }
}

fn print_usage() {
    println!("cielo-repl [--entry <symbol>] [file.cielo]");
    println!("  --entry <symbol>  Entrypoint function symbol (default: main)");
}

struct Repl {
    compiler: Compiler,
    entrypoint: String,
    loaded_path: Option<PathBuf>,
    analysis: Option<Analysis>,
    show_spans: bool,
}

struct Analysis {
    source_name: String,
    source: String,
    interner: Interner,
    parsed: Parsed,
    core: CoreBuilt,
    residual: Residualized,
    linear: LinearProgram,
    c_source: String,
}

impl Repl {
    fn new(entrypoint: String) -> Self {
        Self {
            compiler: Compiler::new(CompilerConfig::default()),
            entrypoint,
            loaded_path: None,
            analysis: None,
            show_spans: false,
        }
    }

    fn run(&mut self) -> io::Result<()> {
        let stdin = io::stdin();
        loop {
            print!("cielo> ");
            io::stdout().flush()?;

            let mut line = String::new();
            if stdin.read_line(&mut line)? == 0 {
                println!();
                break;
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            if !line.starts_with(':') {
                if self.handle_shortcut(line) {
                    continue;
                }
                println!("unknown input; try :help");
                continue;
            }

            let keep_running = self.dispatch_command(line);
            if !keep_running {
                break;
            }
        }
        Ok(())
    }

    fn dispatch_command(&mut self, line: &str) -> bool {
        let trimmed = line.trim_start_matches(':').trim();
        if trimmed.is_empty() {
            return true;
        }

        let (cmd, rest) = split_command(trimmed);
        let result = match cmd {
            "help" | "h" => {
                self.print_help();
                Ok(())
            }
            "quit" | "q" | "exit" => return false,
            "open" => self.cmd_open(rest),
            "reload" => self.cmd_reload(),
            "entry" => self.cmd_entry(rest),
            "paste" => self.cmd_paste(),
            "clear" | "cls" => self.cmd_clear(),
            "spans" => self.cmd_spans(rest),
            "set" => self.cmd_set(rest),
            "loc" | "where" => self.cmd_loc(rest),
            "find" | "search" => self.cmd_find(rest),
            "source" => self.cmd_source(),
            "stats" => self.cmd_stats(),
            "diags" => self.cmd_diags(),
            "functions" | "funcs" => self.cmd_functions(),
            "effects" => self.cmd_effects(),
            "handlers" => self.cmd_handlers(),
            "residual" => self.cmd_residual(),
            "types" => self.cmd_types(rest),
            "stages" => self.cmd_stages(rest),
            "ct" => self.cmd_ct(rest),
            "rt" => self.cmd_rt(rest),
            "expr" => self.cmd_expr(rest),
            "stmt" => self.cmd_stmt(rest),
            "ast" => self.cmd_ast(),
            "core" => self.cmd_core(),
            "linear" => self.cmd_linear(),
            "c" => self.cmd_c(),
            _ => Err(format!("unknown command: :{cmd}")),
        };

        if let Err(err) = result {
            eprintln!("{err}");
        }
        true
    }

    fn handle_shortcut(&mut self, line: &str) -> bool {
        if let Some(index) = parse_prefixed_index(line, 'e') {
            if let Err(err) = self.cmd_expr(index.as_str()) {
                eprintln!("{err}");
            }
            return true;
        }
        if let Some(index) = parse_prefixed_index(line, 's') {
            if let Err(err) = self.cmd_stmt(index.as_str()) {
                eprintln!("{err}");
            }
            return true;
        }
        if Path::new(line).exists() {
            if let Err(err) = self.cmd_open(line) {
                eprintln!("{err}");
            }
            return true;
        }
        false
    }

    fn print_help(&self) {
        println!("Session");
        println!("  :open <path>         load + compile file");
        println!("  :reload              reload current file from disk");
        println!("  :paste               paste source, finish with ':end' line");
        println!("  :entry <symbol>      set entrypoint and recompile current source");
        println!("  :spans on|off|toggle show/hide spans in normal output");
        println!("  :loc <kind> <id>     show exact location (expr/stmt/func/effect/handler)");
        println!("  :find <text>         search symbols/functions/effects");
        println!("  :clear               clear terminal");
        println!("  :source              print source with line numbers");
        println!("  :stats               compile summary");
        println!("  :diags               rendered diagnostics");
        println!("Inspect");
        println!("  :functions           function signatures + effect summaries");
        println!("  :effects             effect declarations + operations + properties");
        println!("  :handlers            handler discharge table");
        println!("  :residual            residual function effect rows");
        println!("  :types [limit]       expression types and persistability");
        println!("  :stages [mode] [n]   list staged exprs (mode: all|ct|rt)");
        println!("  :ct [n]              CT cache entries");
        println!("  :rt [n]              RT exprs with reasons + provenance");
        println!("  :expr <id>           inspect a Core expression");
        println!("  :stmt <id>           inspect a Core statement");
        println!("Raw Dumps");
        println!("  :ast | :core | :linear | :c");
        println!("Shortcuts");
        println!("  e10 / s5             same as :expr 10 / :stmt 5");
        println!("  <existing path>      same as :open <path>");
        println!("Exit");
        println!("  :quit");
    }

    fn cmd_clear(&self) -> Result<(), String> {
        print!("\x1b[2J\x1b[H");
        io::stdout()
            .flush()
            .map_err(|err| format!("failed to flush stdout: {err}"))
    }

    fn cmd_open(&mut self, rest: &str) -> Result<(), String> {
        if rest.is_empty() {
            return Err("usage: :open <path>".to_owned());
        }
        self.load_from_path(Path::new(rest))
    }

    fn cmd_reload(&mut self) -> Result<(), String> {
        let Some(path) = self.loaded_path.clone() else {
            return Err("no file loaded; use :open <path> first".to_owned());
        };
        self.load_from_path(path.as_path())
    }

    fn cmd_entry(&mut self, rest: &str) -> Result<(), String> {
        if rest.is_empty() {
            return Err("usage: :entry <symbol>".to_owned());
        }
        self.entrypoint = rest.to_owned();
        println!("entrypoint set to '{}'", self.entrypoint);

        if let Some(path) = self.loaded_path.clone() {
            self.load_from_path(path.as_path())?;
            return Ok(());
        }

        let snapshot = self
            .analysis
            .as_ref()
            .map(|analysis| (analysis.source_name.clone(), analysis.source.clone()));
        if let Some((source_name, source)) = snapshot {
            self.compile_source(source_name, source)?;
            self.print_compile_summary()?;
        }
        Ok(())
    }

    fn cmd_spans(&mut self, rest: &str) -> Result<(), String> {
        let token = rest.trim();
        if token.is_empty() {
            println!("spans: {}", if self.show_spans { "on" } else { "off" });
            return Ok(());
        }
        match token {
            "on" | "true" | "1" => self.show_spans = true,
            "off" | "false" | "0" => self.show_spans = false,
            "toggle" => self.show_spans = !self.show_spans,
            _ => return Err("usage: :spans on|off|toggle".to_owned()),
        }
        println!("spans: {}", if self.show_spans { "on" } else { "off" });
        Ok(())
    }

    fn cmd_set(&mut self, rest: &str) -> Result<(), String> {
        let mut parts = rest.split_whitespace();
        let Some(key) = parts.next() else {
            return Err("usage: :set spans on|off|toggle".to_owned());
        };
        if key != "spans" {
            return Err("supported: :set spans on|off|toggle".to_owned());
        }
        let value = parts.collect::<Vec<_>>().join(" ");
        self.cmd_spans(value.as_str())
    }

    fn cmd_loc(&self, rest: &str) -> Result<(), String> {
        let analysis = self.analysis()?;
        let program = analysis.core.program();

        let mut parts = rest.split_whitespace();
        let Some(kind) = parts.next() else {
            return Err("usage: :loc <expr|stmt|func|effect|handler> <id>".to_owned());
        };
        let Some(id_token) = parts.next() else {
            return Err("usage: :loc <expr|stmt|func|effect|handler> <id>".to_owned());
        };
        if parts.next().is_some() {
            return Err("usage: :loc <expr|stmt|func|effect|handler> <id>".to_owned());
        }

        match kind {
            "expr" | "e" => {
                let expr_id = ExprId::new(parse_index(id_token, "expr")?);
                let Some(expr) = program.expr(expr_id) else {
                    return Err(format!("unknown expression id: e{}", expr_id.as_u32()));
                };
                println!(
                    "e{} {}",
                    expr_id.as_u32(),
                    format_span(expr.span, &analysis.source)
                );
            }
            "stmt" | "s" => {
                let stmt_id = StmtId::new(parse_index(id_token, "stmt")?);
                let Some(stmt) = program.stmt(stmt_id) else {
                    return Err(format!("unknown statement id: s{}", stmt_id.as_u32()));
                };
                println!(
                    "s{} {}",
                    stmt_id.as_u32(),
                    format_span(stmt.span, &analysis.source)
                );
            }
            "func" | "f" => {
                let func_id = FuncId::new(parse_index(id_token, "func")?);
                let Some(function) = program.function(func_id) else {
                    return Err(format!("unknown function id: f{}", func_id.as_u32()));
                };
                println!(
                    "f{} {} {}",
                    func_id.as_u32(),
                    self.function_name(program, func_id),
                    format_span(function.span, &analysis.source)
                );
            }
            "effect" => {
                let effect_id = EffectLabelId::new(parse_index(id_token, "effect")?);
                let Some(effect) = program.effect(effect_id) else {
                    return Err(format!("unknown effect id: e{}", effect_id.as_u32()));
                };
                println!(
                    "effect e{} {} {}",
                    effect_id.as_u32(),
                    self.symbol_name(effect.name),
                    format_span(effect.span, &analysis.source)
                );
            }
            "handler" | "h" => {
                let handler_id = HandlerId::new(parse_index(id_token, "handler")?);
                let Some(handler) = program.handlers().get(handler_id.index()) else {
                    return Err(format!("unknown handler id: h{}", handler_id.as_u32()));
                };
                println!(
                    "handler h{} {}",
                    handler_id.as_u32(),
                    format_span(handler.span, &analysis.source)
                );
            }
            _ => return Err("usage: :loc <expr|stmt|func|effect|handler> <id>".to_owned()),
        }
        Ok(())
    }

    fn cmd_find(&self, rest: &str) -> Result<(), String> {
        let needle = rest.trim();
        if needle.is_empty() {
            return Err("usage: :find <text>".to_owned());
        }
        let analysis = self.analysis()?;
        let program = analysis.core.program();
        let mut hits = 0usize;

        for (idx, function) in program.functions().iter().enumerate() {
            let name = analysis
                .interner
                .resolve(function.name)
                .unwrap_or("<invalid>");
            if contains_ci(name, needle) {
                println!("func f{} {}", idx, name);
                hits += 1;
            }
        }
        for effect in program.effects() {
            let name = analysis
                .interner
                .resolve(effect.name)
                .unwrap_or("<invalid>");
            if contains_ci(name, needle) {
                println!("effect e{} {}", effect.label.as_u32(), name);
                hits += 1;
            }
            for operation in &effect.operations {
                let op_name = analysis
                    .interner
                    .resolve(operation.name)
                    .unwrap_or("<invalid>");
                if contains_ci(op_name, needle) {
                    println!("effect-op e{} {}.{}", effect.label.as_u32(), name, op_name);
                    hits += 1;
                }
            }
        }

        if hits == 0 {
            println!("(no matches)");
        }
        Ok(())
    }

    fn cmd_paste(&mut self) -> Result<(), String> {
        println!("paste mode; finish input with ':end' on its own line");
        let stdin = io::stdin();
        let mut source = String::new();
        loop {
            print!(".. ");
            io::stdout()
                .flush()
                .map_err(|err| format!("failed to flush stdout: {err}"))?;

            let mut line = String::new();
            let read = stdin
                .read_line(&mut line)
                .map_err(|err| format!("failed to read stdin: {err}"))?;
            if read == 0 {
                break;
            }
            if line.trim() == ":end" {
                break;
            }
            source.push_str(&line);
        }

        self.loaded_path = None;
        self.compile_source("<paste>".to_owned(), source)?;
        self.print_compile_summary()
    }

    fn cmd_source(&self) -> Result<(), String> {
        let analysis = self.analysis()?;
        for (idx, line) in analysis.source.lines().enumerate() {
            println!("{:>4} | {}", idx + 1, line);
        }
        Ok(())
    }

    fn cmd_stats(&self) -> Result<(), String> {
        let analysis = self.analysis()?;
        let program = analysis.core.program();
        let sema = analysis.residual.sema();
        let bta = analysis.residual.bta();
        let ct = analysis.residual.ct();
        let diagnostics = analysis.residual.diagnostics().entries();

        let mut ct_exprs = 0usize;
        let mut rt_exprs = 0usize;
        for stage in bta.stage_of_expr.values() {
            match stage {
                Stage::Ct => ct_exprs += 1,
                Stage::Rt(_) => rt_exprs += 1,
            }
        }

        let mut known_unknown = 0usize;
        let mut known_local = 0usize;
        let mut known_persistable = 0usize;
        for known in bta.knownness_of_expr.values() {
            match known {
                Knownness::Unknown => known_unknown += 1,
                Knownness::KnownLocal => known_local += 1,
                Knownness::KnownPersistable => known_persistable += 1,
            }
        }

        let effectful_stmts = sema
            .effects_of_stmt
            .iter()
            .filter(|effects| !effects.is_empty())
            .count();

        let mut errors = 0usize;
        let mut warnings = 0usize;
        let mut notes = 0usize;
        for diag in diagnostics {
            match diag.severity {
                cielo::common::diagnostics::Severity::Error => errors += 1,
                cielo::common::diagnostics::Severity::Warning => warnings += 1,
                cielo::common::diagnostics::Severity::Note => notes += 1,
            }
        }

        println!("source: {}", analysis.source_name);
        println!("entrypoint: {}", self.entrypoint);
        println!(
            "core: funcs={} effects={} handlers={} structs={} enums={} exprs={} stmts={}",
            program.functions().len(),
            program.effects().len(),
            program.handlers().len(),
            program.structs().len(),
            program.enums().len(),
            program.exprs().len(),
            program.stmts().len()
        );

        if program.entrypoints().is_empty() {
            println!("entrypoints: []");
        } else {
            let entries = program
                .entrypoints()
                .iter()
                .map(|func| format!("{}(f{})", self.function_name(program, *func), func.as_u32()))
                .collect::<Vec<_>>();
            println!("entrypoints: [{}]", entries.join(", "));
        }

        println!(
            "staging: ct_exprs={} rt_exprs={} ct_cache={} branch_decisions={}",
            ct_exprs,
            rt_exprs,
            ct.ct_cache.len(),
            ct.branch_decisions.len()
        );
        println!(
            "knownness: unknown={} local={} persistable={}",
            known_unknown, known_local, known_persistable
        );
        println!(
            "effects: effectful_stmts={}/{}",
            effectful_stmts,
            program.stmts().len()
        );
        println!(
            "diagnostics: total={} errors={} warnings={} notes={}",
            diagnostics.len(),
            errors,
            warnings,
            notes
        );
        println!(
            "linear: funcs={} exprs={} stmts={}",
            analysis.linear.functions.len(),
            analysis.linear.exprs().len(),
            analysis.linear.stmts().len()
        );
        Ok(())
    }

    fn cmd_diags(&self) -> Result<(), String> {
        let analysis = self.analysis()?;
        let diagnostics = analysis.residual.diagnostics().entries();
        if diagnostics.is_empty() {
            println!("(no diagnostics)");
            return Ok(());
        }

        for diag in diagnostics {
            let rendered = render_diagnostic(diag, &analysis.source_name, &analysis.source);
            if rendered.trim().is_empty() {
                println!(
                    "{:?} {} @{}: {}",
                    diag.severity,
                    diag.code,
                    format_span(diag.span, &analysis.source),
                    diag.message
                );
                continue;
            }
            print!("{rendered}");
            if !rendered.ends_with('\n') {
                println!();
            }
        }
        Ok(())
    }

    fn cmd_functions(&self) -> Result<(), String> {
        let analysis = self.analysis()?;
        let program = analysis.core.program();
        if program.functions().is_empty() {
            println!("(no functions)");
            return Ok(());
        }

        for (idx, function) in program.functions().iter().enumerate() {
            let func_id = FuncId::new(idx);
            let params = function
                .params
                .iter()
                .enumerate()
                .map(|(i, var)| {
                    let ty = function
                        .param_types
                        .get(i)
                        .map(|ty| format_core_type_ref(ty, &analysis.interner))
                        .unwrap_or_else(|| "?".to_owned());
                    format!("v{}: {}", var.as_u32(), ty)
                })
                .collect::<Vec<_>>()
                .join(", ");
            let ret = format_core_type_ref(&function.return_type, &analysis.interner);
            let declared_effects =
                format_effect_row(&function.declared_effects, program, &analysis.interner);
            let body_effects = analysis
                .residual
                .sema()
                .effects_of_stmt
                .get(function.body.index())
                .cloned()
                .unwrap_or_else(SortedEffectRow::empty);
            let body_effects = format_effect_row(&body_effects, program, &analysis.interner);
            let residual_effects = analysis
                .residual
                .residual()
                .function_effect_summary
                .get(&func_id)
                .cloned()
                .unwrap_or_else(SortedEffectRow::empty);
            let residual_effects =
                format_effect_row(&residual_effects, program, &analysis.interner);
            let is_entry = program.entrypoints().iter().any(|entry| *entry == func_id);

            println!(
                "f{} {}({}) -> {} | entry={} ct_only={}{}",
                func_id.as_u32(),
                self.function_name(program, func_id),
                params,
                ret,
                is_entry,
                function.ct_only,
                self.maybe_span_suffix(function.span, &analysis.source)
            );
            println!("  declared effects: {declared_effects}");
            println!("  body effects:     {body_effects}");
            println!("  residual effects: {residual_effects}");
        }
        Ok(())
    }

    fn cmd_effects(&self) -> Result<(), String> {
        let analysis = self.analysis()?;
        let program = analysis.core.program();
        if program.effects().is_empty() {
            println!("(no effects)");
            return Ok(());
        }

        for effect in program.effects() {
            println!(
                "e{} {} | {}{}",
                effect.label.as_u32(),
                self.symbol_name(effect.name),
                format_effect_properties(effect.properties),
                self.maybe_span_suffix(effect.span, &analysis.source)
            );
            if effect.operations.is_empty() {
                println!("  ops: (none)");
                continue;
            }
            for operation in &effect.operations {
                let params = operation
                    .param_types
                    .iter()
                    .map(|ty| format_core_type_ref(ty, &analysis.interner))
                    .collect::<Vec<_>>()
                    .join(", ");
                let ret = format_core_type_ref(&operation.return_type, &analysis.interner);
                println!(
                    "  op {}({}) -> {}{}",
                    self.symbol_name(operation.name),
                    params,
                    ret,
                    self.maybe_span_suffix(operation.span, &analysis.source)
                );
            }
        }
        Ok(())
    }

    fn cmd_handlers(&self) -> Result<(), String> {
        let analysis = self.analysis()?;
        let program = analysis.core.program();
        if program.handlers().is_empty() {
            println!("(no handlers)");
            return Ok(());
        }

        for (idx, handler) in program.handlers().iter().enumerate() {
            let handler_id = HandlerId::new(idx);
            let effect_name = program
                .effect(handler.effect)
                .map(|effect| self.symbol_name(effect.name).to_owned())
                .unwrap_or_else(|| format!("e{}", handler.effect.as_u32()));
            let discharge = analysis.residual.bta().handler_discharge.get(&handler_id);
            let discharge_text = match discharge {
                Some(discharge) if discharge.dischargeable => "dischargeable".to_owned(),
                Some(discharge) => format!(
                    "runtime ({})",
                    discharge
                        .reason
                        .map(format_reason)
                        .unwrap_or_else(|| "unknown".to_owned())
                ),
                None => "unknown".to_owned(),
            };
            let clause_names = handler
                .clauses
                .iter()
                .map(|clause| self.symbol_name(clause.operation).to_owned())
                .collect::<Vec<_>>();
            println!(
                "h{} effect={} return=v{} clauses={} discharge={}{}",
                handler_id.as_u32(),
                effect_name,
                handler.return_param.as_u32(),
                handler.clauses.len(),
                discharge_text,
                self.maybe_span_suffix(handler.span, &analysis.source)
            );
            if clause_names.is_empty() {
                println!("  ops: (none)");
            } else {
                println!("  ops: {}", clause_names.join(", "));
            }
        }
        Ok(())
    }

    fn cmd_residual(&self) -> Result<(), String> {
        let analysis = self.analysis()?;
        let program = analysis.core.program();
        if analysis
            .residual
            .residual()
            .function_effect_summary
            .is_empty()
        {
            println!("(empty residual effect summary)");
            return Ok(());
        }

        let mut rows = analysis
            .residual
            .residual()
            .function_effect_summary
            .iter()
            .map(|(func_id, row)| (*func_id, row.clone()))
            .collect::<Vec<_>>();
        rows.sort_by_key(|(func_id, _)| func_id.index());

        for (func_id, row) in rows {
            println!(
                "f{} {} -> {}",
                func_id.as_u32(),
                self.function_name(program, func_id),
                format_effect_row(&row, program, &analysis.interner)
            );
        }
        Ok(())
    }

    fn cmd_types(&self, rest: &str) -> Result<(), String> {
        let analysis = self.analysis()?;
        let limit = parse_optional_limit(rest, DEFAULT_LIST_LIMIT)?;
        let sema = analysis.residual.sema();
        let program = analysis.core.program();
        let mut shown = 0usize;

        for idx in 0..program.exprs().len() {
            if shown >= limit {
                break;
            }
            let expr_id = ExprId::new(idx);
            let Some(ty) = sema.type_of_expr.get(idx).copied().flatten() else {
                continue;
            };
            let persistability = sema
                .persistability_of_type
                .get(ty.index())
                .copied()
                .unwrap_or(Persistability::NonPersistable);
            let span = program
                .expr(expr_id)
                .map(|expr| expr.span)
                .unwrap_or_default();
            println!(
                "e{} type={} persistability={}{}",
                expr_id.as_u32(),
                format_type_id(ty),
                format_persistability(persistability),
                self.maybe_span_suffix(span, &analysis.source)
            );
            shown += 1;
        }

        if shown == 0 {
            println!("(no typed expressions)");
        } else if shown >= limit {
            println!("... truncated to {limit} rows");
        }
        Ok(())
    }

    fn cmd_stages(&self, rest: &str) -> Result<(), String> {
        let analysis = self.analysis()?;
        let (mode, limit) = parse_stage_args(rest)?;
        let program = analysis.core.program();
        let sema = analysis.residual.sema();
        let bta = analysis.residual.bta();

        let mut rows = Vec::new();
        for idx in 0..program.exprs().len() {
            let expr_id = ExprId::new(idx);
            let Some(stage) = bta.stage_of_expr.get(&expr_id).copied() else {
                continue;
            };
            if !mode.matches(stage) {
                continue;
            }
            rows.push((expr_id, stage));
        }

        if rows.is_empty() {
            println!("(no expressions matched)");
            return Ok(());
        }

        let mut shown = 0usize;
        for (expr_id, stage) in rows {
            if shown >= limit {
                break;
            }
            let ty = sema
                .type_of_expr
                .get(expr_id.index())
                .copied()
                .flatten()
                .map(format_type_id)
                .unwrap_or_else(|| "?".to_owned());
            let knownness = bta
                .knownness_of_expr
                .get(&expr_id)
                .copied()
                .map(format_knownness)
                .unwrap_or("unknown");
            let extra = match stage {
                Stage::Ct => analysis
                    .residual
                    .ct()
                    .ct_cache
                    .get(&expr_id)
                    .map(|value| format!("value={value:?}"))
                    .unwrap_or_else(|| "value=<not-folded>".to_owned()),
                Stage::Rt(reason) => format!("reason={}", format_reason(reason)),
            };
            println!(
                "e{} {} type={} knownness={} kind={} {}",
                expr_id.as_u32(),
                format_stage(stage),
                ty,
                knownness,
                format_expr_brief(program, &analysis.interner, expr_id),
                extra
            );
            shown += 1;
        }

        if shown >= limit {
            println!("... truncated to {limit} rows");
        }
        Ok(())
    }

    fn cmd_ct(&self, rest: &str) -> Result<(), String> {
        let analysis = self.analysis()?;
        let limit = parse_optional_limit(rest, DEFAULT_LIST_LIMIT)?;
        let program = analysis.core.program();
        let mut entries = analysis
            .residual
            .ct()
            .ct_cache
            .iter()
            .map(|(expr_id, value)| (*expr_id, value.clone()))
            .collect::<Vec<_>>();
        entries.sort_by_key(|(expr_id, _)| expr_id.index());

        if entries.is_empty() {
            println!("(ct cache empty)");
            return Ok(());
        }

        for (idx, (expr_id, value)) in entries.iter().enumerate() {
            if idx >= limit {
                println!("... truncated to {limit} rows");
                break;
            }
            let span = program
                .expr(*expr_id)
                .map(|expr| expr.span)
                .unwrap_or_default();
            println!(
                "e{} value={value:?} kind={}{}",
                expr_id.as_u32(),
                format_expr_brief(program, &analysis.interner, *expr_id),
                self.maybe_span_suffix(span, &analysis.source)
            );
        }
        Ok(())
    }

    fn cmd_rt(&self, rest: &str) -> Result<(), String> {
        let analysis = self.analysis()?;
        let limit = parse_optional_limit(rest, DEFAULT_LIST_LIMIT)?;
        let program = analysis.core.program();
        let bta = analysis.residual.bta();

        let mut runtime_exprs = bta
            .stage_of_expr
            .iter()
            .filter_map(|(expr_id, stage)| match stage {
                Stage::Ct => None,
                Stage::Rt(reason) => Some((*expr_id, *reason)),
            })
            .collect::<Vec<_>>();
        runtime_exprs.sort_by_key(|(expr_id, _)| expr_id.index());

        if runtime_exprs.is_empty() {
            println!("(no runtime expressions)");
            return Ok(());
        }

        for (idx, (expr_id, reason)) in runtime_exprs.iter().enumerate() {
            if idx >= limit {
                println!("... truncated to {limit} rows");
                break;
            }
            let span = program
                .expr(*expr_id)
                .map(|expr| expr.span)
                .unwrap_or_default();
            println!(
                "e{} reason={} kind={}{}",
                expr_id.as_u32(),
                format_reason(*reason),
                format_expr_brief(program, &analysis.interner, *expr_id),
                self.maybe_span_suffix(span, &analysis.source)
            );

            let provenance =
                runtime_provenance_lines(program, bta, *expr_id, DEFAULT_PROVENANCE_LIMIT.max(1));
            if provenance.is_empty() {
                println!("  provenance: (none)");
            } else {
                for line in provenance {
                    println!("  {line}");
                }
            }
        }
        Ok(())
    }

    fn cmd_expr(&self, rest: &str) -> Result<(), String> {
        if rest.is_empty() {
            return Err("usage: :expr <id>".to_owned());
        }
        let id = parse_index(rest, "expr")?;
        let expr_id = ExprId::new(id);

        let analysis = self.analysis()?;
        let program = analysis.core.program();
        let sema = analysis.residual.sema();
        let bta = analysis.residual.bta();
        let ct = analysis.residual.ct();

        let Some(expr) = program.expr(expr_id) else {
            return Err(format!("unknown expression id: e{}", expr_id.as_u32()));
        };

        println!(
            "e{} {}",
            expr_id.as_u32(),
            format_expr_brief(program, &analysis.interner, expr_id)
        );
        if self.show_spans {
            println!("span: {}", format_span(expr.span, &analysis.source));
        }
        println!("raw kind: {:?}", expr.kind);

        let type_text = sema
            .type_of_expr
            .get(expr_id.index())
            .copied()
            .flatten()
            .map(format_type_id)
            .unwrap_or_else(|| "?".to_owned());
        println!("type: {type_text}");

        if let Some(ty) = sema.type_of_expr.get(expr_id.index()).copied().flatten() {
            let persistability = sema
                .persistability_of_type
                .get(ty.index())
                .copied()
                .unwrap_or(Persistability::NonPersistable);
            println!("persistability: {}", format_persistability(persistability));
        }

        let expr_effects = sema
            .effects_of_expr
            .get(expr_id.index())
            .cloned()
            .unwrap_or_else(SortedEffectRow::empty);
        println!(
            "expr-effects: {}",
            format_effect_row(&expr_effects, program, &analysis.interner)
        );

        if let Some(stage) = bta.stage_of_expr.get(&expr_id).copied() {
            println!("stage: {}", format_stage(stage));
            if let Stage::Rt(_) = stage {
                let provenance =
                    runtime_provenance_lines(program, bta, expr_id, DEFAULT_PROVENANCE_LIMIT);
                if provenance.is_empty() {
                    println!("runtime provenance: (none)");
                } else {
                    println!("runtime provenance:");
                    for line in provenance {
                        println!("  {line}");
                    }
                }
            }
        } else {
            println!("stage: <missing>");
        }

        if let Some(knownness) = bta.knownness_of_expr.get(&expr_id).copied() {
            println!("knownness: {}", format_knownness(knownness));
        } else {
            println!("knownness: <missing>");
        }

        if let Some(value) = ct.ct_cache.get(&expr_id) {
            println!("ct-value: {value:?}");
        }

        let uses = find_expr_uses(program, expr_id);
        if uses.is_empty() {
            println!("used-by stmts: []");
        } else {
            let use_text = uses
                .iter()
                .map(|stmt| format!("s{}", stmt.as_u32()))
                .collect::<Vec<_>>()
                .join(", ");
            println!("used-by stmts: [{use_text}]");
        }
        Ok(())
    }

    fn cmd_stmt(&self, rest: &str) -> Result<(), String> {
        if rest.is_empty() {
            return Err("usage: :stmt <id>".to_owned());
        }
        let id = parse_index(rest, "stmt")?;
        let stmt_id = StmtId::new(id);

        let analysis = self.analysis()?;
        let program = analysis.core.program();
        let sema = analysis.residual.sema();

        let Some(stmt) = program.stmt(stmt_id) else {
            return Err(format!("unknown statement id: s{}", stmt_id.as_u32()));
        };

        println!(
            "s{} {}",
            stmt_id.as_u32(),
            format_stmt_brief(stmt.kind.clone(), &analysis.interner, program)
        );
        if self.show_spans {
            println!("span: {}", format_span(stmt.span, &analysis.source));
        }
        println!("raw kind: {:?}", stmt.kind);
        let effects = sema
            .effects_of_stmt
            .get(stmt_id.index())
            .cloned()
            .unwrap_or_else(SortedEffectRow::empty);
        println!(
            "stmt-effects: {}",
            format_effect_row(&effects, program, &analysis.interner)
        );
        let children_stmts = stmt
            .child_stmts()
            .iter()
            .map(|id| format!("s{}", id.as_u32()))
            .collect::<Vec<_>>();
        let children_exprs = stmt
            .child_exprs()
            .iter()
            .map(|id| format!("e{}", id.as_u32()))
            .collect::<Vec<_>>();
        println!("child-stmts: [{}]", children_stmts.join(", "));
        println!("child-exprs: [{}]", children_exprs.join(", "));
        Ok(())
    }

    fn cmd_ast(&self) -> Result<(), String> {
        let analysis = self.analysis()?;
        println!("{:#?}", analysis.parsed.ast());
        Ok(())
    }

    fn cmd_core(&self) -> Result<(), String> {
        let analysis = self.analysis()?;
        println!("{:#?}", analysis.core.program());
        Ok(())
    }

    fn cmd_linear(&self) -> Result<(), String> {
        let analysis = self.analysis()?;
        println!("{:#?}", analysis.linear);
        Ok(())
    }

    fn cmd_c(&self) -> Result<(), String> {
        let analysis = self.analysis()?;
        println!("{}", analysis.c_source);
        Ok(())
    }

    fn load_from_path(&mut self, path: &Path) -> Result<(), String> {
        let source = fs::read_to_string(path)
            .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        let source_name = path.display().to_string();
        self.compile_source(source_name, source)?;
        self.loaded_path = Some(path.to_path_buf());
        self.print_compile_summary()
    }

    fn compile_source(&mut self, source_name: String, source: String) -> Result<(), String> {
        let mut interner = Interner::new();
        let parsed = self
            .compiler
            .parse(&source, SourceId::from_u32(0), &mut interner);
        let entry_symbol = interner.intern(self.entrypoint.as_str());
        let core = self.compiler.lower_parsed_to_core_with_config(
            parsed.clone(),
            LowerConfig::with_entrypoint(entry_symbol),
        );
        let residual = self.compiler.run_v0_core_pipeline(core.clone());

        let emitted = c_emit::run(
            linearize::run(handler_specialize::run(residual.clone())),
            &interner,
        );
        let linear = emitted.linearized.linear;
        let c_source = emitted.c_source;

        self.analysis = Some(Analysis {
            source_name,
            source,
            interner,
            parsed,
            core,
            residual,
            linear,
            c_source,
        });
        Ok(())
    }

    fn print_compile_summary(&self) -> Result<(), String> {
        let analysis = self.analysis()?;
        let mut ct_exprs = 0usize;
        let mut rt_exprs = 0usize;
        for stage in analysis.residual.bta().stage_of_expr.values() {
            match stage {
                Stage::Ct => ct_exprs += 1,
                Stage::Rt(_) => rt_exprs += 1,
            }
        }

        println!(
            "loaded {} | funcs={} exprs={} stmts={} ct={} rt={} diags={}",
            analysis.source_name,
            analysis.core.program().functions().len(),
            analysis.core.program().exprs().len(),
            analysis.core.program().stmts().len(),
            ct_exprs,
            rt_exprs,
            analysis.residual.diagnostics().entries().len()
        );
        Ok(())
    }

    fn analysis(&self) -> Result<&Analysis, String> {
        self.analysis
            .as_ref()
            .ok_or_else(|| "no source loaded (use :open <path> or :paste)".to_owned())
    }

    fn symbol_name(&self, symbol: cielo::common::ids::SymbolId) -> &str {
        self.analysis
            .as_ref()
            .and_then(|analysis| analysis.interner.resolve(symbol))
            .unwrap_or("<invalid>")
    }

    fn function_name(&self, program: &CoreProgram, func_id: FuncId) -> String {
        program
            .function(func_id)
            .and_then(|function| {
                self.analysis
                    .as_ref()
                    .and_then(|analysis| analysis.interner.resolve(function.name))
            })
            .unwrap_or("<invalid>")
            .to_owned()
    }

    fn maybe_span_suffix(&self, span: Span, source: &str) -> String {
        if self.show_spans {
            format!(" | span={}", format_span(span, source))
        } else {
            String::new()
        }
    }
}

#[derive(Clone, Copy)]
enum StageFilter {
    All,
    Ct,
    Rt,
}

impl StageFilter {
    fn parse(token: &str) -> Option<Self> {
        match token {
            "all" => Some(Self::All),
            "ct" => Some(Self::Ct),
            "rt" => Some(Self::Rt),
            _ => None,
        }
    }

    fn matches(self, stage: Stage) -> bool {
        match (self, stage) {
            (Self::All, _) => true,
            (Self::Ct, Stage::Ct) => true,
            (Self::Rt, Stage::Rt(_)) => true,
            _ => false,
        }
    }
}

fn parse_stage_args(rest: &str) -> Result<(StageFilter, usize), String> {
    if rest.is_empty() {
        return Ok((StageFilter::All, DEFAULT_LIST_LIMIT));
    }

    let mut tokens = rest.split_whitespace();
    let first = tokens.next().unwrap_or_default();
    let second = tokens.next();
    let third = tokens.next();
    if third.is_some() {
        return Err("usage: :stages [all|ct|rt] [limit]".to_owned());
    }

    if let Some(mode) = StageFilter::parse(first) {
        let limit = if let Some(text) = second {
            parse_limit(text)?
        } else {
            DEFAULT_LIST_LIMIT
        };
        return Ok((mode, limit));
    }

    if second.is_some() {
        return Err("usage: :stages [all|ct|rt] [limit]".to_owned());
    }

    Ok((StageFilter::All, parse_limit(first)?))
}

fn parse_optional_limit(rest: &str, default: usize) -> Result<usize, String> {
    if rest.trim().is_empty() {
        return Ok(default);
    }
    parse_limit(rest.trim())
}

fn parse_limit(text: &str) -> Result<usize, String> {
    let value = text
        .trim()
        .parse::<usize>()
        .map_err(|_| format!("invalid limit: {text}"))?;
    if value == 0 {
        return Err("limit must be > 0".to_owned());
    }
    Ok(value)
}

fn split_command(input: &str) -> (&str, &str) {
    if let Some(idx) = input.find(char::is_whitespace) {
        (&input[..idx], input[idx..].trim())
    } else {
        (input, "")
    }
}

fn parse_index(text: &str, what: &str) -> Result<usize, String> {
    let token = text.split_whitespace().next().unwrap_or_default();
    let token = token
        .strip_prefix('e')
        .or_else(|| token.strip_prefix('s'))
        .or_else(|| token.strip_prefix('f'))
        .or_else(|| token.strip_prefix('h'))
        .unwrap_or(token);
    token
        .parse::<usize>()
        .map_err(|_| format!("invalid {what} id: {text}"))
}

fn parse_prefixed_index(text: &str, prefix: char) -> Option<String> {
    let trimmed = text.trim();
    let value = trimmed.strip_prefix(prefix)?;
    if value.is_empty() {
        return None;
    }
    if value.chars().all(|ch| ch.is_ascii_digit()) {
        Some(value.to_owned())
    } else {
        None
    }
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

fn format_core_type_ref(ty: &CoreTypeRef, interner: &Interner) -> String {
    match ty {
        CoreTypeRef::Unit => "()".to_owned(),
        CoreTypeRef::Primitive(PrimitiveTypeRef::Bool) => "Bool".to_owned(),
        CoreTypeRef::Primitive(PrimitiveTypeRef::Int) => "Int".to_owned(),
        CoreTypeRef::Primitive(PrimitiveTypeRef::Float) => "Float".to_owned(),
        CoreTypeRef::Primitive(PrimitiveTypeRef::Char) => "Char".to_owned(),
        CoreTypeRef::Primitive(PrimitiveTypeRef::String) => "String".to_owned(),
        CoreTypeRef::Named(symbol) => interner.resolve(*symbol).unwrap_or("<invalid>").to_owned(),
        CoreTypeRef::Unknown => "?".to_owned(),
    }
}

fn format_effect_row(row: &SortedEffectRow, program: &CoreProgram, interner: &Interner) -> String {
    if row.is_empty() {
        return "[]".to_owned();
    }
    let items = row
        .iter()
        .map(|effect_id| {
            if let Some(effect) = program.effect(effect_id) {
                format!(
                    "{}(e{})",
                    interner.resolve(effect.name).unwrap_or("<invalid>"),
                    effect_id.as_u32()
                )
            } else {
                format!("e{}", effect_id.as_u32())
            }
        })
        .collect::<Vec<_>>();
    format!("[{}]", items.join(", "))
}

fn format_effect_properties(properties: EffectProperties) -> String {
    format!(
        "cap={} flags={} ct_eligible={}",
        format_capability(properties.level),
        format_effect_flags(properties.flags),
        properties.is_ct_eligible()
    )
}

fn format_capability(level: CapabilityLevel) -> &'static str {
    match level {
        CapabilityLevel::Pure => "pure",
        CapabilityLevel::Diverge => "diverge",
        CapabilityLevel::Alloc => "alloc",
        CapabilityLevel::LocalState => "local-state",
        CapabilityLevel::SharedState => "shared-state",
        CapabilityLevel::Io => "io",
        CapabilityLevel::Ffi => "ffi",
    }
}

fn format_effect_flags(flags: EffectFlags) -> String {
    let mut out = Vec::new();
    if flags.contains(EffectFlags::DISCARDABLE) {
        out.push("discardable");
    }
    if flags.contains(EffectFlags::COMMUTATIVE) {
        out.push("commutative");
    }
    if flags.contains(EffectFlags::LOCAL_STATE) {
        out.push("local_state");
    }
    if flags.contains(EffectFlags::SHARED_STATE) {
        out.push("shared_state");
    }
    if flags.contains(EffectFlags::OPAQUE_FOR_STAGING) {
        out.push("opaque_for_staging");
    }
    if flags.contains(EffectFlags::CT_ONLY) {
        out.push("ct_only");
    }
    if out.is_empty() {
        "none".to_owned()
    } else {
        out.join("|")
    }
}

fn format_stage(stage: Stage) -> String {
    match stage {
        Stage::Ct => "CT".to_owned(),
        Stage::Rt(reason) => format!("RT({})", format_reason(reason)),
    }
}

fn format_reason(reason: Reason) -> String {
    match reason {
        Reason::UnclassifiedRuntime => "unclassified-runtime".to_owned(),
        Reason::Parameter { func, index } => {
            format!("parameter-f{}-#{}", func.as_u32(), index + 1)
        }
        Reason::DependsOnVar(var) => format!("depends-on-v{}", var.as_u32()),
        Reason::EffectNotDischarged(effect) => {
            format!("effect-e{}-not-discharged", effect.as_u32())
        }
        Reason::HandlerIsRuntime(handler) => format!("handler-h{}-runtime", handler.as_u32()),
        Reason::BranchOnRuntime(expr) => format!("branch-on-e{}", expr.as_u32()),
        Reason::NotPersistable(ty) => format!("type-t{}-not-persistable", ty.as_u32()),
        Reason::UserForcedRuntime => "forced-runtime".to_owned(),
        Reason::CtOnlyWithRuntimeArgs(func) => {
            format!("ct-only-f{}-called-with-rt-args", func.as_u32())
        }
    }
}

fn format_knownness(knownness: Knownness) -> &'static str {
    match knownness {
        Knownness::Unknown => "unknown",
        Knownness::KnownLocal => "known-local",
        Knownness::KnownPersistable => "known-persistable",
    }
}

fn format_type_id(ty: TypeId) -> String {
    format!("t{}", ty.as_u32())
}

fn format_persistability(persistability: Persistability) -> &'static str {
    match persistability {
        Persistability::Trivial => "trivial",
        Persistability::Serializable => "serializable",
        Persistability::NonPersistable => "non-persistable",
    }
}

fn format_expr_brief(program: &CoreProgram, interner: &Interner, expr_id: ExprId) -> String {
    let Some(expr) = program.expr(expr_id) else {
        return "<missing-expr>".to_owned();
    };
    match &expr.kind {
        ExprKind::Var(var) => format!("var v{}", var.as_u32()),
        ExprKind::Literal(lit) => format!("literal {lit:?}"),
        ExprKind::Unary { op, expr } => format!("{op:?}(e{})", expr.as_u32()),
        ExprKind::Binary { op, lhs, rhs } => {
            format!("{op:?}(e{}, e{})", lhs.as_u32(), rhs.as_u32())
        }
        ExprKind::PureCall { callee, args } => {
            let fn_name = program
                .function(*callee)
                .and_then(|f| interner.resolve(f.name))
                .unwrap_or("<invalid>");
            let args = args
                .iter()
                .map(|arg| format!("e{}", arg.as_u32()))
                .collect::<Vec<_>>()
                .join(", ");
            format!("call {}(f{}, {})", fn_name, callee.as_u32(), args)
        }
        ExprKind::MakeStruct { ty, fields } => {
            let name = interner.resolve(*ty).unwrap_or("<invalid>");
            let fields = fields
                .iter()
                .map(|field| format!("e{}", field.as_u32()))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}({})", name, fields)
        }
        ExprKind::MakeEnum {
            ty,
            variant,
            fields,
        } => {
            let ty_name = interner.resolve(*ty).unwrap_or("<invalid>");
            let variant_name = interner.resolve(*variant).unwrap_or("<invalid>");
            let fields = fields
                .iter()
                .map(|field| format!("e{}", field.as_u32()))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{ty_name}::{variant_name}({fields})")
        }
        ExprKind::Error(_) => "error".to_owned(),
    }
}

fn format_stmt_brief(stmt: StmtKind, interner: &Interner, program: &CoreProgram) -> String {
    match stmt {
        StmtKind::Return(expr) => format!("return e{}", expr.as_u32()),
        StmtKind::Let {
            binding,
            value,
            next,
        } => {
            format!(
                "let v{} = e{}; goto s{}",
                binding.as_u32(),
                value.as_u32(),
                next.as_u32()
            )
        }
        StmtKind::Val {
            binding,
            value,
            next,
        } => {
            format!(
                "val v{} = s{}; goto s{}",
                binding.as_u32(),
                value.as_u32(),
                next.as_u32()
            )
        }
        StmtKind::Call {
            result,
            callee,
            args,
            next,
            ..
        } => {
            let name = program
                .function(callee)
                .and_then(|function| interner.resolve(function.name))
                .unwrap_or("<invalid>");
            let args = args
                .iter()
                .map(|arg| format!("e{}", arg.as_u32()))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "v{} = call {}(f{}, {}); goto s{}",
                result.as_u32(),
                name,
                callee.as_u32(),
                args,
                next.as_u32()
            )
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            format!(
                "if e{} then s{} else s{}",
                cond.as_u32(),
                then_branch.as_u32(),
                else_branch.as_u32()
            )
        }
        StmtKind::Match {
            scrutinee,
            arms,
            default,
        } => {
            let mut names = arms
                .iter()
                .map(|arm| {
                    format!(
                        "{} -> s{}",
                        interner.resolve(arm.tag).unwrap_or("<invalid>"),
                        arm.body.as_u32()
                    )
                })
                .collect::<Vec<_>>();
            if let Some(default_stmt) = default {
                names.push(format!("_ -> s{}", default_stmt.as_u32()));
            }
            format!("match e{} {{ {} }}", scrutinee.as_u32(), names.join(", "))
        }
        StmtKind::Perform {
            result,
            effect,
            operation,
            args,
            next,
        } => {
            let args = args
                .iter()
                .map(|arg| format!("e{}", arg.as_u32()))
                .collect::<Vec<_>>()
                .join(", ");
            let op = interner.resolve(operation).unwrap_or("<invalid>");
            if let Some(result) = result {
                format!(
                    "v{} = perform e{}.{op}({args}); goto s{}",
                    result.as_u32(),
                    effect.as_u32(),
                    next.as_u32()
                )
            } else {
                format!(
                    "perform e{}.{op}({args}); goto s{}",
                    effect.as_u32(),
                    next.as_u32()
                )
            }
        }
        StmtKind::Resume {
            result,
            resume,
            arg,
            next,
        } => {
            format!(
                "v{} = resume v{} with e{}; goto s{}",
                result.as_u32(),
                resume.as_u32(),
                arg.as_u32(),
                next.as_u32()
            )
        }
        StmtKind::Handle {
            handler,
            body,
            next,
        } => {
            if let Some(next) = next {
                format!(
                    "handle h{} {{ s{} }}; goto s{}",
                    handler.as_u32(),
                    body.as_u32(),
                    next.as_u32()
                )
            } else {
                format!("handle h{} {{ s{} }}", handler.as_u32(), body.as_u32())
            }
        }
        StmtKind::Stage { stage, body, next } => {
            let stage = format!("{stage:?}");
            if let Some(next) = next {
                format!("{stage} {{ s{} }}; goto s{}", body.as_u32(), next.as_u32())
            } else {
                format!("{stage} {{ s{} }}", body.as_u32())
            }
        }
        StmtKind::Hole { ty } => format!("hole: {}", format_type_id(ty)),
        StmtKind::Error(_) => "error".to_owned(),
    }
}

fn format_span(span: Span, source: &str) -> String {
    if !span.source.is_valid() {
        return "<synthetic>".to_owned();
    }
    let (start_line, start_col) = offset_to_line_col(source, span.start);
    let (end_line, end_col) = offset_to_line_col(source, span.end);
    format!(
        "{}..{} (L{}:C{}..L{}:C{})",
        span.start, span.end, start_line, start_col, end_line, end_col
    )
}

fn offset_to_line_col(source: &str, offset: u32) -> (usize, usize) {
    let target = (offset as usize).min(source.len());
    let mut line = 1usize;
    let mut col = 1usize;

    for (idx, ch) in source.char_indices() {
        if idx >= target {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

fn find_expr_uses(program: &CoreProgram, target: ExprId) -> Vec<StmtId> {
    let mut uses = Vec::new();
    for (idx, stmt) in program.stmts().iter().enumerate() {
        if stmt.child_exprs().iter().any(|expr_id| *expr_id == target) {
            uses.push(StmtId::new(idx));
        }
    }
    uses
}
