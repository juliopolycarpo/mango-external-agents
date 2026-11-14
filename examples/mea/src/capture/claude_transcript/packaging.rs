//! Pins the names the Claude recorder writes against `scripts/package-fixtures.py`.
//!
//! The script decides which files under `fixtures/claude/transcripts/` reach the published
//! `mango-agent-claude` crate: the glob `transcripts/*.jsonl`, minus the recorder's diagnostic
//! suffixes. Nothing else ties the two together, so a renamed recorder output would stop a
//! committed recording from being packaged with no check failing. The recorder owns the names and
//! the script owns the selection; this test reads the script and holds each to the other.
//!
//! It parses the script's text rather than running it: `cargo test` also runs on Windows, where
//! this repository does not assume a Python interpreter.

use std::path::{Path, PathBuf};

use super::{
    PARTIAL_SUFFIX, QUALIFIED_SUFFIX, TRANSCRIPTS_DIRECTORY, UNQUALIFIED_SUFFIX, output_name,
    scenario_names,
};

/// The packaging script, read from the repository the way `field_inventory` reads the docs.
fn packaging_script() -> String {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("scripts")
        .join("package-fixtures.py");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("expected to read {}, received {error}", path.display()))
}

/// Every double-quoted string in `text`, in order. The script holds no escaped quote.
fn quoted_strings(text: &str) -> Vec<&str> {
    text.split('"').skip(1).step_by(2).collect()
}

/// The suffixes the script skips, from its `DIAGNOSTIC_SUFFIXES = (...)` assignment.
fn script_diagnostic_suffixes(script: &str) -> Vec<String> {
    let assignment = script
        .split("DIAGNOSTIC_SUFFIXES = (")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .unwrap_or_else(|| {
            panic!("expected a `DIAGNOSTIC_SUFFIXES = (...)` assignment in package-fixtures.py")
        });
    quoted_strings(assignment)
        .into_iter()
        .map(String::from)
        .collect()
}

/// The selections the script names under `transcripts/`, from anywhere in its text.
fn script_transcript_globs(script: &str) -> Vec<String> {
    quoted_strings(script)
        .into_iter()
        .filter(|selection| selection.starts_with("transcripts/"))
        .map(String::from)
        .collect()
}

/// Whether a glob of the one shape the script uses, `<directory>/*<tail>`, matches `relative`.
///
/// Like `Path.glob`, the `*` stands for one path segment: it does not cross a slash.
fn glob_matches(glob: &str, relative: &str) -> bool {
    let Some((head, tail)) = glob.split_once('*') else {
        panic!("expected a glob with one `*`, received {glob:?}");
    };
    relative
        .strip_prefix(head)
        .and_then(|rest| rest.strip_suffix(tail))
        .is_some_and(|stem| !stem.is_empty() && !stem.contains('/'))
}

/// Whether the script would package `relative`: matched by a glob and not a diagnostic.
fn is_packaged(globs: &[String], diagnostics: &[String], relative: &str) -> bool {
    globs.iter().any(|glob| glob_matches(glob, relative))
        && !diagnostics.iter().any(|suffix| relative.ends_with(suffix))
}

#[test]
fn the_script_skips_exactly_the_diagnostic_suffixes_the_recorder_writes() {
    let mut recorded = vec![PARTIAL_SUFFIX.to_owned(), UNQUALIFIED_SUFFIX.to_owned()];
    let mut skipped = script_diagnostic_suffixes(&packaging_script());
    recorded.sort();
    skipped.sort();
    assert!(
        skipped == recorded,
        "(package-fixtures.py DIAGNOSTIC_SUFFIXES): expected {recorded:?} | received: {skipped:?}"
    );
}

#[test]
fn the_script_packages_the_recorders_qualified_name_for_every_scenario() {
    let script = packaging_script();
    let globs = script_transcript_globs(&script);
    let diagnostics = script_diagnostic_suffixes(&script);
    assert!(
        !globs.is_empty(),
        "(package-fixtures.py): expected a `{TRANSCRIPTS_DIRECTORY}/...` selection | received: none"
    );
    let scenarios = scenario_names();
    assert!(
        !scenarios.is_empty(),
        "(recorder scenarios): expected at least one scenario | received: none"
    );
    for scenario in scenarios {
        let qualified = format!(
            "{TRANSCRIPTS_DIRECTORY}/{}",
            output_name(scenario, QUALIFIED_SUFFIX)
        );
        assert!(
            is_packaged(&globs, &diagnostics, &qualified),
            "(package-fixtures.py): expected {qualified} selected by {globs:?} and not skipped by {diagnostics:?} | received: not packaged"
        );
    }
}

#[test]
fn the_script_leaves_every_scenarios_diagnostics_out_of_the_crate() {
    let script = packaging_script();
    let globs = script_transcript_globs(&script);
    let diagnostics = script_diagnostic_suffixes(&script);
    for scenario in scenario_names() {
        for suffix in [PARTIAL_SUFFIX, UNQUALIFIED_SUFFIX] {
            let diagnostic = format!("{TRANSCRIPTS_DIRECTORY}/{}", output_name(scenario, suffix));
            assert!(
                !is_packaged(&globs, &diagnostics, &diagnostic),
                "(package-fixtures.py): expected {diagnostic} skipped as a recorder diagnostic | received: packaged"
            );
        }
    }
}

#[test]
fn output_name_joins_the_scenario_and_the_suffix() {
    assert_eq!(output_name("turn", QUALIFIED_SUFFIX), "turn.jsonl");
    assert_eq!(output_name("turn", PARTIAL_SUFFIX), "turn.partial.jsonl");
    assert_eq!(
        output_name("turn", UNQUALIFIED_SUFFIX),
        "turn.unqualified.jsonl"
    );
}

#[test]
fn quoted_strings_returns_each_literal_and_not_the_text_between() {
    assert_eq!(
        quoted_strings(r#"A = ("a.x", "b.y"), "c""#),
        ["a.x", "b.y", "c"]
    );
    assert!(quoted_strings("no literals").is_empty());
}

#[test]
fn script_diagnostic_suffixes_reads_the_tuple() {
    let script = "X = 1\nDIAGNOSTIC_SUFFIXES = (\".a.jsonl\", \".b.jsonl\")\ndef f(): pass\n";
    assert_eq!(script_diagnostic_suffixes(script), [".a.jsonl", ".b.jsonl"]);
}

#[test]
#[should_panic(expected = "expected a `DIAGNOSTIC_SUFFIXES = (...)` assignment")]
fn script_diagnostic_suffixes_names_the_missing_assignment() {
    script_diagnostic_suffixes("SELECTIONS = {}");
}

#[test]
fn script_transcript_globs_keeps_only_transcript_selections() {
    let script =
        r#"{"claude": ["help/a.txt", "transcripts/*.jsonl", "contract"], "codex": ["**/*.jsonl"]}"#;
    assert_eq!(script_transcript_globs(script), ["transcripts/*.jsonl"]);
}

#[test]
fn glob_matches_one_segment_with_the_tail() {
    let glob = "transcripts/*.jsonl";
    assert!(glob_matches(glob, "transcripts/read-turn.jsonl"));
    assert!(glob_matches(glob, "transcripts/a.partial.jsonl"));
    assert!(!glob_matches(glob, "transcripts/read-turn.json"));
    assert!(!glob_matches(glob, "transcripts/nested/read-turn.jsonl"));
    assert!(!glob_matches(glob, "other/read-turn.jsonl"));
    assert!(!glob_matches(glob, "transcripts/.jsonl"));
}

#[test]
#[should_panic(expected = "expected a glob with one `*`")]
fn glob_matches_rejects_a_glob_without_a_star() {
    glob_matches("transcripts/fixed.jsonl", "transcripts/fixed.jsonl");
}

#[test]
fn is_packaged_needs_a_glob_match_and_no_diagnostic_suffix() {
    let globs = vec![String::from("transcripts/*.jsonl")];
    let diagnostics = vec![String::from(".partial.jsonl")];
    assert!(is_packaged(&globs, &diagnostics, "transcripts/a.jsonl"));
    assert!(!is_packaged(
        &globs,
        &diagnostics,
        "transcripts/a.partial.jsonl"
    ));
    assert!(!is_packaged(&globs, &diagnostics, "transcripts/a.txt"));
}
