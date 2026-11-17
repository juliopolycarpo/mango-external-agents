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
///
/// Not canonicalised, and the segments are joined one at a time. On Windows `canonicalize` returns
/// a `\\?\` verbatim path, and Win32 does not normalise a forward slash underneath one — so a
/// `join("docs/vendor-fields.md")` on a canonicalised root fails to open a file that is plainly
/// there. A relative `../..` opens everywhere.
fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

fn inventory() -> String {
    let path = repository_root().join("docs").join("vendor-fields.md");
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

/// Every test the evidence column names, as the bare function names.
///
/// Read from the last cell of each data row rather than from the whole document, so a type path in
/// a destination cell is never mistaken for a test. Inside it, an all-lowercase backticked `::`
/// path ending in a snake_case name is a test — `reducer::tests::a_name`, `replay::a_name`,
/// `harness_over_a_fake_cli::a_turn::a_name`. The case rule is what separates those from
/// `SessionState::set_commands`, a method somebody cites rather than a test anybody runs: a type
/// name is capitalised and a test path never is.
fn named_tests(document: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for row in rows(document) {
        let Some(evidence) = row.last() else {
            continue;
        };
        for token in evidence.split('`').skip(1).step_by(2) {
            let segments: Vec<&str> = token.split("::").collect();
            if segments.len() < 2 {
                continue;
            }
            // A type name is capitalised and a test path never is, which is what separates
            // `replay::a_name` from `SessionState::set_commands`.
            let is_test_path = token.chars().all(|character| {
                character.is_ascii_lowercase()
                    || character.is_ascii_digit()
                    || matches!(character, '_' | ':')
            });
            let Some(name) = segments.last().filter(|name| name.contains('_')) else {
                continue;
            };
            if is_test_path {
                names.insert((*name).to_owned());
            }
        }
    }
    names
}

/// Every Rust source file in the workspace, excluding build output.
///
/// An unreadable directory is a failure, not an empty result: a renamed `crates/` would otherwise
/// leave the inventory check scanning nothing and passing.
fn sources(directory: &Path, found: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(directory).unwrap_or_else(|error| {
        panic!(
            "expected {} to be a readable directory (layout: <root>/crates and <root>/examples hold the workspace sources) | received {error}",
            directory.display()
        )
    });
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

/// Far below the real count (about 280), far above what a half-renamed tree would still hold.
const MINIMUM_SCANNED_SOURCE_FILES: usize = 100;

fn workspace_source_text() -> String {
    let root = repository_root();
    let mut files = Vec::new();
    sources(&root.join("crates"), &mut files);
    sources(&root.join("examples"), &mut files);
    assert_scanned_enough(files.len());
    files
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_scanned_enough(scanned: usize) {
    assert!(
        scanned >= MINIMUM_SCANNED_SOURCE_FILES,
        "expected at least {MINIMUM_SCANNED_SOURCE_FILES} .rs files under crates/ and examples/ | received {scanned}"
    );
}

#[test]
#[should_panic(
    expected = "expected at least 100 .rs files under crates/ and examples/ | received 3"
)]
fn a_tree_with_too_few_sources_fails_the_inventory() {
    assert_scanned_enough(3);
}

#[test]
#[should_panic(expected = "to be a readable directory")]
fn a_missing_source_directory_fails_the_inventory() {
    let missing = repository_root().join("crates-renamed-away");
    sources(&missing, &mut Vec::new());
}

#[test]
fn the_workspace_scan_finds_the_real_tree() {
    let mut files = Vec::new();
    sources(&repository_root().join("crates"), &mut files);
    assert!(
        files.iter().any(|path| path.ends_with("permission.rs")),
        "expected the scan of crates/ to include permission.rs | received {} files",
        files.len()
    );
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
