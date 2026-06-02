use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use cielo::{Compiler, CompilerConfig, MemoryPreset, MemoryProfile};
use cielo_base::SourceId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Example {
    file_name: &'static str,
    expected_exit: i32,
}

const EXAMPLES: &[Example] = &[
    Example {
        file_name: "v0_adt.cielo",
        expected_exit: 0,
    },
    Example {
        file_name: "v0_arith_stage.cielo",
        expected_exit: 42,
    },
    Example {
        file_name: "v0_effect_call_and_stage.cielo",
        expected_exit: 3,
    },
    Example {
        file_name: "v0_effect_handle.cielo",
        expected_exit: 0,
    },
    Example {
        file_name: "v1_clause_calls_helper.cielo",
        expected_exit: 6,
    },
    Example {
        file_name: "v1_closure.cielo",
        expected_exit: 26,
    },
    Example {
        file_name: "v1_generics.cielo",
        expected_exit: 42,
    },
    Example {
        file_name: "v1_named_handler.cielo",
        expected_exit: 10,
    },
    Example {
        file_name: "v1_resume_control.cielo",
        expected_exit: 10,
    },
    Example {
        file_name: "v1_resume_tail.cielo",
        expected_exit: 10,
    },
    Example {
        file_name: "v1_test.cielo",
        expected_exit: 1,
    },
];

const MEMORY_PRESETS: &[(&str, MemoryPreset)] = &[
    ("unmanaged", MemoryPreset::Unmanaged),
    ("arc-raw", MemoryPreset::ArcRaw),
    ("arc-optimized", MemoryPreset::ArcOptimized),
    ("arc-no-verify", MemoryPreset::ArcNoVerify),
];

fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../cielo-compiler/examples")
}

fn shipped_examples(dir: &Path) -> Vec<String> {
    let mut files = fs::read_dir(dir)
        .expect("read compiler examples directory")
        .map(|entry| entry.expect("read compiler example entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "cielo")
        })
        .map(|path| {
            path.file_name()
                .expect("example path should have a file name")
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn assert_manifest_covers_examples() {
    let shipped = shipped_examples(examples_dir().as_path());
    let mut manifested = EXAMPLES
        .iter()
        .map(|example| example.file_name.to_owned())
        .collect::<Vec<_>>();
    manifested.sort();

    assert_eq!(
        manifested, shipped,
        "example manifest must cover every shipped .cielo file"
    );
}

#[test]
fn example_manifest_covers_every_shipped_example() {
    assert_manifest_covers_examples();
}

struct NativeWorkspace {
    path: PathBuf,
}

impl NativeWorkspace {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should follow Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "cielo_driver_examples_{}_{}",
            std::process::id(),
            stamp
        ));
        fs::create_dir(&path).expect("create native example workspace");
        fs::write(path.join("cielo_runtime.h"), cielo::RUNTIME_HEADER)
            .expect("write C runtime header");
        Self { path }
    }
}

impl Drop for NativeWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn c_compiler_command() -> OsString {
    std::env::var_os("CC").unwrap_or_else(|| OsString::from("cc"))
}

fn c_compiler_available(cc: &OsString) -> bool {
    Command::new(cc)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn compile_and_run(cc: &OsString, workspace: &Path, name: &str, c_source: &str) -> Output {
    let c_path = workspace.join(format!("{name}.c"));
    let binary_path = workspace.join(format!("{name}.bin"));
    fs::write(&c_path, c_source).expect("write emitted C");

    let build = Command::new(cc)
        .args(["-std=c11", "-O2"])
        .arg(&c_path)
        .arg("-o")
        .arg(&binary_path)
        .output()
        .expect("invoke C compiler");
    assert!(
        build.status.success(),
        "emitted C for {name} failed to compile:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );

    Command::new(&binary_path)
        .output()
        .expect("run compiled example")
}

#[test]
fn every_example_executes_under_every_public_memory_preset() {
    assert_manifest_covers_examples();

    let cc = c_compiler_command();
    if !c_compiler_available(&cc) {
        eprintln!("skipping executable example smoke matrix: no C compiler found");
        return;
    }

    let workspace = NativeWorkspace::new();
    let examples_dir = examples_dir();
    let mut completed = 0;

    for &(preset_name, preset) in MEMORY_PRESETS {
        let compiler = Compiler::new(CompilerConfig {
            memory: MemoryProfile::from_preset(preset),
            ..CompilerConfig::default()
        });

        for (index, example) in EXAMPLES.iter().enumerate() {
            let path = examples_dir.join(example.file_name);
            let source_text = fs::read_to_string(&path).expect("read example source");
            let source = compiler.source_at(
                path.to_string_lossy().as_ref(),
                &source_text,
                SourceId::from_u32(index as u32),
            );
            let emitted = compiler.emit(source);
            let diagnostics = &emitted.memory.runtime.runtime.diagnostics;
            assert!(
                !diagnostics.has_errors(),
                "{}/{} produced compiler errors: {:?}",
                example.file_name,
                preset_name,
                diagnostics.entries()
            );

            let stem = example
                .file_name
                .strip_suffix(".cielo")
                .expect("manifest should contain .cielo files");
            let run = compile_and_run(
                &cc,
                &workspace.path,
                &format!("{stem}_{preset_name}"),
                &emitted.c_source,
            );
            assert_eq!(
                run.status.code(),
                Some(example.expected_exit),
                "{}/{} returned wrong status\nstdout:\n{}\nstderr:\n{}",
                example.file_name,
                preset_name,
                String::from_utf8_lossy(&run.stdout),
                String::from_utf8_lossy(&run.stderr)
            );
            completed += 1;
        }
    }

    assert_eq!(completed, EXAMPLES.len() * MEMORY_PRESETS.len());
    eprintln!("executable example smoke matrix: {completed} combinations passed");
}
