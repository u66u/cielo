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

