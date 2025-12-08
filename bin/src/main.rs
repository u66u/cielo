use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use cielo::common::ids::{EffectLabelId, ExprId, FuncId, HandlerId, SourceId, StmtId, TypeId};
use cielo::common::reporting::render_diagnostic;
use cielo::common::span::Span;
use cielo::common::symbols::Interner;
use cielo::ir::core::{CoreProgram, CoreTypeRef, ExprKind, PrimitiveTypeRef, StmtKind};
use cielo::ir::linear::LinearProgram;
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
            _ => Err(format!("unknown command: :{cmd}")),
        };
        if let Err(err) = result {
            eprintln!("{err}");
        }
        true
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
