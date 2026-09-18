//! Holds `docs/vendor-fields.md` to the tree it describes.
//!
//! The inventory is only worth keeping if a reader can trust its last column. A row that names a
//! test which no longer exists is worse than a row with no test at all: it claims a decision is
//! pinned when nothing pins it. So every test the table names is checked to exist, and every row
//! that says a field was dropped is checked to say why.
//!
//! It lives in `mea` because `mea` is where this repository keeps its drift checks, and because
//! nothing published depends on it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The repository root, from this crate's own manifest.
fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("expected the repository root above examples/mea")
}

fn inventory() -> String {
    let path = repository_root().join("docs/vendor-fields.md");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("expected to read {}, received {error}", path.display()))
}

/// Every data row of every table in the inventory, as its cells.
///
/// A data row is a line starting with `|` that is not the `---` rule under a header and does not
/// repeat the header's own column names. The narrow "how to read a row" table above the data has
/// two columns and is dropped by the width filter.
fn rows(document: &str) -> Vec<Vec<String>> {
    document
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('|'))
        .filter(|line| !line.contains("---"))
        .map(|line| {
            line.trim_matches('|')
                .split('|')
                .map(|cell| cell.trim().to_owned())
                .collect::<Vec<_>>()
        })
        .filter(|cells| cells.len() >= 5)
        .filter(|cells| cells.last().is_some_and(|last| last != "Evidence"))
        .collect()
}

/// Every `something::tests::name` the document names, as the bare function names.
fn named_tests(document: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for chunk in document.split("::tests::").skip(1) {
        let name: String = chunk
            .chars()
            .take_while(|character| character.is_ascii_alphanumeric() || *character == '_')
            .collect();
        if !name.is_empty() {
            names.insert(name);
        }
    }
    names
}

/// Every Rust source file in the workspace, excluding build output.
fn sources(directory: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            sources(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

fn workspace_source_text() -> String {
    let root = repository_root();
    let mut files = Vec::new();
    sources(&root.join("crates"), &mut files);
    sources(&root.join("examples"), &mut files);
    files
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A row naming a test that no longer exists claims a decision is pinned when nothing pins it.
#[test]
fn every_test_the_inventory_names_exists_in_the_tree() {
    let sources = workspace_source_text();
    let mut missing = Vec::new();
    for name in named_tests(&inventory()) {
        if !sources.contains(&format!("fn {name}(")) {
            missing.push(name);
        }
    }
    assert!(
        missing.is_empty(),
        "expected every test named in docs/vendor-fields.md to exist, received none for: {missing:?}"
    );
}

/// The whole point of the table is the omissions, so an omission with no reason is a row that says
/// nothing. "The current interface has no card for it" is not a reason and this is what stops a row
/// from quietly becoming one.
#[test]
fn every_dropped_or_refused_field_states_a_reason() {
    let mut silent = Vec::new();
    for row in rows(&inventory()) {
        let [field, .., disposition, reason, _evidence] = row.as_slice() else {
            continue;
        };
        if !matches!(disposition.as_str(), "omit" | "refuse") {
            continue;
        }
        if reason.trim().is_empty() || reason.trim() == "—" {
            silent.push(field.clone());
        }
    }
    assert!(
        silent.is_empty(),
        "expected every omitted or refused field to state a reason, received none for: {silent:?}"
    );
}

/// A disposition outside the four the document defines is a row nobody can act on.
#[test]
fn every_row_carries_one_of_the_four_dispositions() {
    let mut unknown = Vec::new();
    for row in rows(&inventory()) {
        let [field, .., disposition, _reason, _evidence] = row.as_slice() else {
            continue;
        };
        if !matches!(
            disposition.as_str(),
            "preserve" | "redact" | "omit" | "refuse"
        ) {
            unknown.push(format!("{field}: {disposition}"));
        }
    }
    assert!(
        unknown.is_empty(),
        "expected preserve, redact, omit or refuse, received: {unknown:?}"
    );
}
