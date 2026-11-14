//! `mea turn --json-schema FILE`: the refusals `run` prints, and whole turns over a scripted
//! `claude` that prove the schema reaches the vendor command line.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mango_agent_claude::testing::{
    DEFAULT_HELP as HELP_WITHOUT_JSON_SCHEMA, FakeClaudeCli, HELP_WITH_JSON_SCHEMA, Run,
};
use mango_external_agents::{EnvSource, HostContext};

use super::{Options, run, turn_registry, turn_with};

fn host_over(launcher: Arc<FakeClaudeCli>) -> HostContext {
    HostContext::builder()
        .launcher(launcher)
        .cwd(std::env::temp_dir())
        .environment(EnvSource::from_pairs([("PATH", "/usr/bin")]))
        .client_info("mea-test", "0.0.0")
        .build()
        .expect("expected a fake host context")
}

fn schema_file(directory: &tempfile::TempDir, text: &str) -> PathBuf {
    let path = directory.path().join("answer.schema.json");
    std::fs::write(&path, text).expect("expected the schema file to be written");
    path
}

fn turn_options(path: &Path) -> Options {
    Options::parse_for(
        "turn",
        &[
            "--harness",
            "claude",
            "--json-schema",
            &path.display().to_string(),
            "Reply with the word ok.",
        ]
        .map(String::from),
    )
    .expect("expected valid claude turn arguments")
}

/// What a schema in a file reaches the vendor as: the compact text, behind `--json-schema`.
#[tokio::test]
async fn a_turn_passes_the_schema_file_to_the_vendor_command_line() {
    let directory = tempfile::tempdir().expect("expected a scratch directory");
    let path = schema_file(
        &directory,
        "{\n  \"type\": \"object\",\n  \"properties\": { \"word\": { \"type\": \"string\" } }\n}\n",
    );
    let options = turn_options(&path);
    let cli = Arc::new(
        FakeClaudeCli::new()
            .with_help(HELP_WITH_JSON_SCHEMA)
            .with_turn(Run::answering_schema(&serde_json::json!({"word": "ok"})))
            .refusing_unscripted_turns(),
    );

    turn_with(
        &turn_registry(&options).expect("expected a registry with the schema"),
        &host_over(Arc::clone(&cli)),
        &options,
    )
    .await
    .unwrap_or_else(|error| panic!("expected the schema turn to complete, received {error}"));

    let argvs = cli.turn_argvs();
    assert_eq!(
        argvs.len(),
        1,
        "expected exactly one turn launch | received: {argvs:?}"
    );
    let at = argvs[0]
        .iter()
        .position(|word| word == "--json-schema")
        .unwrap_or_else(|| panic!("expected --json-schema | received: {:?}", argvs[0]));
    let argument = argvs[0].get(at + 1).unwrap_or_else(|| {
        panic!(
            "expected a value after --json-schema | received: {:?}",
            argvs[0]
        )
    });
    // Compared as JSON: the member order of a compact schema is the serialiser's, not this test's.
    let sent: serde_json::Value = serde_json::from_str(argument).unwrap_or_else(|error| {
        panic!("expected JSON after the flag | received: {argument:?} ({error})")
    });
    assert_eq!(
        sent,
        serde_json::json!({"type": "object", "properties": {"word": {"type": "string"}}}),
        "expected the file's schema right after the flag"
    );
    assert!(
        !argument.contains(['\n', ' ']),
        "expected the compact form, not the file's own layout | received: {argument:?}"
    );
}

#[tokio::test]
async fn a_turn_without_the_flag_puts_no_schema_on_the_command_line() {
    let options = Options::parse_for(
        "turn",
        &["--harness", "claude", "Reply with the word ok."].map(String::from),
    )
    .expect("expected valid claude turn arguments");
    let cli = Arc::new(
        FakeClaudeCli::new()
            .with_help(HELP_WITH_JSON_SCHEMA)
            .with_turn(Run::answering_schema(&serde_json::json!({"word": "ok"})))
            .refusing_unscripted_turns(),
    );

    turn_with(
        &turn_registry(&options).expect("expected the plain registry"),
        &host_over(Arc::clone(&cli)),
        &options,
    )
    .await
    .unwrap_or_else(|error| panic!("expected the turn to complete, received {error}"));

    let argvs = cli.turn_argvs();
    assert_eq!(
        argvs.len(),
        1,
        "expected exactly one turn launch | received: {argvs:?}"
    );
    assert!(
        !argvs[0].iter().any(|word| word == "--json-schema"),
        "expected no --json-schema without the flag | received: {:?}",
        argvs[0]
    );
}

/// The CLI's help is the authority on whether it takes the flag, and the harness's refusal prints
/// as it is.
#[tokio::test]
async fn a_build_without_the_flag_is_refused_by_the_harness_in_its_own_words() {
    let directory = tempfile::tempdir().expect("expected a scratch directory");
    let path = schema_file(&directory, r#"{"type":"object"}"#);
    let options = turn_options(&path);
    // No turn is scripted: a launch past the help check is a refusal that names its argv.
    let cli = Arc::new(
        FakeClaudeCli::new()
            .with_help(HELP_WITHOUT_JSON_SCHEMA)
            .refusing_unscripted_turns(),
    );

    let error = turn_with(
        &turn_registry(&options).expect("expected a registry with the schema"),
        &host_over(Arc::clone(&cli)),
        &options,
    )
    .await
    .expect_err("expected a build that does not declare --json-schema to be refused");

    assert_eq!(
        error,
        "expected a Claude build whose --help declares --json-schema, received a build that declares no such flag (version 2.1.270)"
    );
    assert!(
        cli.turn_argvs().is_empty(),
        "expected no turn to be launched | received: {:?}",
        cli.turn_argvs()
    );
}

#[test]
fn the_harness_refusal_of_a_schema_that_is_not_an_object_prints_unchanged() {
    let directory = tempfile::tempdir().expect("expected a scratch directory");
    let path = schema_file(&directory, "[1, 2]");
    let received = turn_registry(&turn_options(&path)).expect_err("expected the schema refused");
    let harness_words = mango_agent_claude::ClaudeHarness::new()
        .with_json_schema("[1, 2]")
        .err()
        .expect("expected the harness to refuse an array")
        .to_string();
    assert_eq!(received, harness_words);
}

#[test]
fn an_unreadable_schema_file_stops_the_registry_with_the_path_it_received() {
    let directory = tempfile::tempdir().expect("expected a scratch directory");
    let missing = directory.path().join("missing.schema.json");
    let received =
        turn_registry(&turn_options(&missing)).expect_err("expected the missing file refused");
    assert!(
        received.starts_with("(mea turn): expected --json-schema as a readable UTF-8 file")
            && received.contains(&format!("{missing:?}")),
        "expected the refusal to name the path | received: {received}"
    );
}

/// Each command the flag does not belong to refuses it through `run`, before any probe.
#[tokio::test]
async fn run_refuses_a_schema_where_it_would_not_be_used() {
    let path = std::env::temp_dir().join("answer.schema.json");
    let typed = path.display().to_string();
    let cases: [(Vec<&str>, String); 4] = [
        (
            vec!["turn", "--harness", "codex", "--json-schema", &typed, "hi"],
            format!(
                "(mea turn): expected --json-schema with --harness claude | received --json-schema {path:?} with --harness codex"
            ),
        ),
        (
            vec!["doctor", "--json-schema", &typed],
            format!(
                "(mea doctor): expected no --json-schema; doctor probes the local CLI and never opens a session | received --json-schema {path:?}"
            ),
        ),
        (
            vec!["discover", "--json-schema", &typed],
            format!(
                "(mea discover): expected no --json-schema; discover probes the local CLI and never opens a session | received --json-schema {path:?}"
            ),
        ),
        (
            vec!["capture", "--harness", "claude", "--json-schema", &typed],
            format!(
                "(mea capture): expected --json-schema only with --harness claude --transcripts --scenario structured-output | received --json-schema {path:?} with --harness claude"
            ),
        ),
    ];
    for (words, expected) in cases {
        let args: Vec<String> = words.iter().map(|word| (*word).to_owned()).collect();
        let error = run(&args).await.expect_err("expected the schema refused");
        assert_eq!(error, expected, "expected {words:?} refused with its value");
    }
}

#[tokio::test]
async fn a_schema_turn_without_an_answer_fails_with_the_harness_code_and_message() {
    let directory = tempfile::tempdir().expect("expected a scratch directory");
    let path = schema_file(&directory, r#"{"type":"object"}"#);
    let options = turn_options(&path);
    let cli = Arc::new(
        FakeClaudeCli::new()
            .with_help(HELP_WITH_JSON_SCHEMA)
            .with_turn(Run::leaving_schema_unanswered(
                "I could not satisfy the schema.",
            ))
            .refusing_unscripted_turns(),
    );

    let error = turn_with(
        &turn_registry(&options).expect("expected a registry with the schema"),
        &host_over(Arc::clone(&cli)),
        &options,
    )
    .await
    .expect_err("expected a turn that ended without an answer to fail");

    // The error event line above the summary carries the message too (pinned in `turn.rs`); the
    // summary repeats it, so a script that reads only the summary sees the failure and its cause.
    assert!(
        error.starts_with(
            "claude-structured-output-missing: vendor failure: Claude Code ended a --json-schema turn without an answer"
        ),
        "expected the code and the vendor's message in the summary | received: {error}"
    );
}
