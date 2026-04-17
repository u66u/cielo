use std::path::PathBuf;
use std::process::Command;

fn example() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../cielo-compiler/examples/v1_test.cielo")
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_cielo"))
        .args(args)
        .output()
        .expect("driver should start")
}

#[test]
fn driver_selects_unmanaged_memory() {
    let path = example();
    let output = Command::new(env!("CARGO_BIN_EXE_cielo"))
        .arg(path)
        .args(["--memory", "unmanaged", "--dump", "memory"])
        .output()
        .expect("driver should start");

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("memory: Unmanaged"), "{stdout}");
    assert!(stdout.contains("=== Memory ==="), "{stdout}");
}

#[test]
fn driver_selects_optimized_arc_by_default() {
    let path = example();
    let output = Command::new(env!("CARGO_BIN_EXE_cielo"))
        .arg(path)
        .args(["--dump", "memory"])
        .output()
        .expect("driver should start");

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("memory: ReferenceCounting"), "{stdout}");
    assert!(stdout.contains("=== Memory ==="), "{stdout}");
}

#[test]
fn driver_accepts_raw_arc_without_exposing_future_collectors() {
    let output = Command::new(env!("CARGO_BIN_EXE_cielo"))
        .arg(example())
        .args(["--memory", "arc-raw"])
        .output()
        .expect("driver should start");

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("memory: ReferenceCounting"), "{stdout}");
}

#[test]
fn help_lists_only_implemented_memory_choices() {
    let output = run(&["--help"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("unmanaged"), "{stdout}");
    assert!(stdout.contains("arc-raw"), "{stdout}");
    assert!(stdout.contains("arc-optimized"), "{stdout}");
    assert!(!stdout.contains("region"), "{stdout}");
    assert!(!stdout.contains("mark-sweep"), "{stdout}");
}
