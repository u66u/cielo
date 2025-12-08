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
