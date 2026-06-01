use std::fs;
use std::path::{Path, PathBuf};

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
