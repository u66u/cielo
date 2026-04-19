use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("compiler crate lives under workspace/crates")
        .to_owned()
}

fn crate_root(name: &str) -> PathBuf {
    workspace_root().join("crates").join(name)
}

fn normal_dependencies(name: &str) -> BTreeSet<String> {
    let manifest =
        fs::read_to_string(crate_root(name).join("Cargo.toml")).expect("read crate manifest");
    let dependencies = manifest
        .split_once("[dependencies]")
        .map(|(_, rest)| rest)
        .unwrap_or("")
        .split("\n[")
        .next()
        .unwrap_or("");

    dependencies
        .lines()
        .filter_map(|line| line.split_once('=').map(|(name, _)| name.trim()))
        .filter(|name| name.starts_with("cielo-"))
        .map(str::to_owned)
        .collect()
}

fn visit_files(path: &Path, visit: &mut impl FnMut(&Path)) {
    for entry in fs::read_dir(path).expect("read source tree") {
        let path = entry.expect("read source entry").path();
        if path.is_dir() {
            visit_files(&path, visit);
        } else {
            visit(&path);
        }
    }
}

#[test]
fn memory_and_backend_depend_only_on_data_crates() {
    let expected = BTreeSet::from(["cielo-base".to_owned(), "cielo-ir".to_owned()]);

    assert_eq!(normal_dependencies("cielo-memory"), expected);
    assert_eq!(
        normal_dependencies("cielo-backend-c"),
        BTreeSet::from(["cielo-base".to_owned(), "cielo-ir".to_owned()])
    );
}

#[test]
fn salsa_is_owned_only_by_the_database() {
    let crates = fs::read_dir(workspace_root().join("crates")).expect("read crates directory");
    for entry in crates {
        let path = entry.expect("read crate entry").path();
        if !path.is_dir() || path.file_name().and_then(|name| name.to_str()) == Some("cielo-db") {
            continue;
        }

        let manifest = fs::read_to_string(path.join("Cargo.toml")).expect("read crate manifest");
        assert!(
            !manifest
                .lines()
                .any(|line| line.trim_start().starts_with("salsa")),
            "{} must not depend on Salsa",
            path.display()
        );

        visit_files(&path.join("src"), &mut |source| {
            if source.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                return;
            }
            let text = fs::read_to_string(source).expect("read Rust source");
            assert!(
                !text.contains("salsa::"),
                "{} must not expose Salsa",
                source.display()
            );
        });
    }
}

#[test]
fn production_sources_have_no_porting_residue() {
    let root = workspace_root();
    assert!(!root.join("src").exists(), "workspace root src/ returned");

    let crates = fs::read_dir(root.join("crates")).expect("read crates directory");
    for entry in crates {
        let path = entry.expect("read crate entry").path();
        if !path.is_dir() {
            continue;
        }
        let src = path.join("src");
        visit_files(&src, &mut |source| {
            let relative = source.strip_prefix(&src).expect("source below src root");
            assert!(
                !relative.components().any(|part| {
                    matches!(
                        part.as_os_str().to_str(),
                        Some("legacy" | "compat" | "tests")
                    )
                }),
                "porting residue under {}",
                source.display()
            );
            if source.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                return;
            }
            let text = fs::read_to_string(source).expect("read Rust source");
            assert!(
                !text.contains("include!("),
                "include in {}",
                source.display()
            );
            assert!(
                !text.contains("#[cfg(test)]"),
                "embedded tests in {}",
                source.display()
            );
        });
    }
}
