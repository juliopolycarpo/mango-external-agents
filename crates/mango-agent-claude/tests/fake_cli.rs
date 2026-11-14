//! The fake `claude` itself: what it answers, what it records and what it refuses, so a test built
//! on it fails for the reason it states and not because the fake drifted.

use mango_agent_claude::testing::{
    CREDENTIAL_EXPIRY_CANARY, DEFAULT_HELP, DEFAULT_VERSION, FakeClaudeCli, HELP_WITH_JSON_SCHEMA,
    Run, SIGNED_IN, SIGNED_OUT, is_probe, value_after,
};
use mango_external_agents::{Error, LaunchSpec, ManagedProcess, ProcessLauncher};

fn launch(words: &[&str]) -> LaunchSpec {
    LaunchSpec {
        argv: words.iter().map(|word| (*word).to_owned()).collect(),
        cwd: std::env::temp_dir(),
        env: std::collections::BTreeMap::new(),
        stdin: true,
        hide_window: false,
    }
}

/// Every line the child printed, read the way the harness reads stdout.
async fn printed(mut process: ManagedProcess) -> Vec<String> {
    let mut text = Vec::new();
    while let Some(chunk) = process
        .stdout
        .next_chunk()
        .await
        .expect("expected the fake stdout to be readable")
    {
        text.extend(chunk);
    }
    String::from_utf8(text)
        .expect("expected the fake to print UTF-8")
        .lines()
        .map(str::to_owned)
        .collect()
}

async fn spawned_and_printed(cli: &FakeClaudeCli, words: &[&str]) -> Vec<String> {
    let process = cli
        .spawn(launch(words))
        .await
        .unwrap_or_else(|error| panic!("expected {words:?} to launch | received: {error}"));
    printed(process).await
}

fn refusal(result: Result<ManagedProcess, Error>, words: &[&str]) -> String {
    match result {
        Ok(_) => panic!("expected {words:?} to be refused | received: a running child"),
        Err(error) => error.to_string(),
    }
}

#[tokio::test]
async fn a_new_fake_answers_the_three_probes_as_a_signed_in_build() {
    let cli = FakeClaudeCli::new();

    assert_eq!(
        spawned_and_printed(&cli, &["claude", "--version"]).await,
        vec![DEFAULT_VERSION.to_owned()]
    );
    assert_eq!(
        spawned_and_printed(&cli, &["claude", "--help"])
            .await
            .join("\n"),
        DEFAULT_HELP
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert_eq!(
        spawned_and_printed(&cli, &["claude", "auth", "status"]).await,
        vec![SIGNED_IN.to_owned()]
    );
}

#[tokio::test]
async fn help_version_and_auth_answers_are_the_ones_it_was_given() {
    let cli = FakeClaudeCli::new()
        .with_version("2.1.300 (Claude Code)")
        .with_help(HELP_WITH_JSON_SCHEMA)
        .with_auth(SIGNED_OUT);

    assert_eq!(
        spawned_and_printed(&cli, &["claude", "--version"]).await,
        vec![String::from("2.1.300 (Claude Code)")]
    );
    let help = spawned_and_printed(&cli, &["claude", "--help"]).await;
    assert!(
        help.iter().any(|line| line.contains("--json-schema")),
        "expected a help text that declares --json-schema | received {} lines without it",
        help.len()
    );
    assert_eq!(
        spawned_and_printed(&cli, &["claude", "auth", "status"]).await,
        vec![SIGNED_OUT.to_owned()]
    );
}

#[tokio::test]
async fn the_default_help_predates_the_json_schema_flag() {
    assert!(
        !DEFAULT_HELP.contains("--json-schema"),
        "expected the default help to be a build without --json-schema"
    );
    assert!(
        HELP_WITH_JSON_SCHEMA.contains("--json-schema"),
        "expected HELP_WITH_JSON_SCHEMA to declare --json-schema"
    );
}

#[tokio::test]
async fn set_help_changes_the_next_help_probe() {
    let cli = FakeClaudeCli::new();
    cli.set_help("Usage: claude [options]");

    assert_eq!(
        spawned_and_printed(&cli, &["claude", "--help"]).await,
        vec![String::from("Usage: claude [options]")]
    );
}

#[tokio::test]
async fn launches_keep_every_argv_and_turn_argvs_keep_only_turns() {
    let cli = FakeClaudeCli::new().with_turn(Run::replaying(r#"{"type":"result"}"#));
    spawned_and_printed(&cli, &["claude", "--version"]).await;
    spawned_and_printed(&cli, &["claude", "auth", "status"]).await;
    spawned_and_printed(&cli, &["claude", "--print", "--json-schema", "{}"]).await;

    let launched: Vec<Vec<String>> = cli.launches().into_iter().map(|spec| spec.argv).collect();
    assert_eq!(
        launched.len(),
        3,
        "expected 3 launches | received {launched:?}"
    );
    assert_eq!(
        cli.turn_argvs(),
        vec![vec!["claude", "--print", "--json-schema", "{}"]],
        "expected only the turn | received {:?}",
        cli.turn_argvs()
    );
}

#[tokio::test]
async fn queued_turns_play_in_order_and_each_prints_its_own_lines() {
    let cli = FakeClaudeCli::new()
        .with_turn(Run::replaying("first"))
        .with_turn(Run::replaying("second"));

    assert_eq!(
        spawned_and_printed(&cli, &["claude", "--print"]).await,
        vec!["first"]
    );
    assert_eq!(
        spawned_and_printed(&cli, &["claude", "--print"]).await,
        vec!["second"]
    );
}

#[tokio::test]
async fn an_unscripted_turn_is_refused_by_default() {
    let cli = FakeClaudeCli::new();
    let words = ["claude", "--print"];

    let message = refusal(cli.spawn(launch(&words)).await, &words);

    assert!(
        message.contains("a turn with no Run queued"),
        "expected a default fake to refuse a turn with no Run queued | received: {message}"
    );
}

#[tokio::test]
async fn a_completing_run_answers_with_an_empty_result() {
    let cli = FakeClaudeCli::new().with_turn(Run::completing());

    let lines = spawned_and_printed(&cli, &["claude", "--print"]).await;

    assert_eq!(lines, vec![r#"{"type":"result","is_error":false}"#]);
}

#[tokio::test]
async fn a_refused_turn_keeps_its_argv_out_of_the_error_and_on_the_fake() {
    let cli = FakeClaudeCli::new();
    let words = ["claude", "--print", "--model", "sk-ant-canary-argument"];

    let message = refusal(cli.spawn(launch(&words)).await, &words);

    assert!(
        message.contains("a turn with no Run queued") && message.contains("4 arguments"),
        "expected the refusal to name the unscripted turn and its argument count | received: {message}"
    );
    assert!(
        !message.contains("sk-ant-canary-argument"),
        "expected Error::Launch to carry no argv | received: {message}"
    );
    let refused: Vec<Vec<String>> = cli
        .refused_launches()
        .into_iter()
        .map(|spec| spec.argv)
        .collect();
    assert_eq!(
        refused,
        vec![words.map(String::from).to_vec()],
        "expected refused_launches() to hold the received argv"
    );
    assert!(
        cli.launches().is_empty(),
        "expected a refused launch to stay out of launches() | received {:?}",
        cli.launches().len()
    );
}

#[tokio::test]
async fn a_refused_launch_is_readable_by_flag_names_without_its_values() {
    let cli = FakeClaudeCli::new();
    let words = [
        "claude",
        "--print",
        "--model",
        "sk-ant-canary-argument",
        "--resume=sk-ant-canary-equals",
        "--effort",
        "canary prompt text",
    ];

    refusal(cli.spawn(launch(&words)).await, &words);

    assert_eq!(
        cli.refused_launch_shapes(),
        vec![
            "claude with 7 arguments: flags --print, --model, --resume, --effort; 2 values redacted"
                .to_owned()
        ],
        "expected refused_launch_shapes() to name the flags and count the rest"
    );
}

#[tokio::test]
async fn a_dash_led_value_is_counted_and_never_named_as_a_flag() {
    let cli = FakeClaudeCli::new();
    let words = [
        "claude",
        "--model",
        "-----BEGIN canary key",
        "--",
        "-9canary",
    ];

    refusal(cli.spawn(launch(&words)).await, &words);

    let shapes = cli.refused_launch_shapes();
    assert_eq!(
        shapes,
        vec!["claude with 5 arguments: flags --model; 3 values redacted".to_owned()],
        "expected only flag-shaped words named | received: {shapes:?}"
    );
}

#[tokio::test]
async fn a_flag_shaped_value_after_a_known_flag_is_counted_and_never_named() {
    let cli = FakeClaudeCli::new();
    let words = [
        "claude",
        "--model",
        "-sk-ant-canary-value",
        "--session-id=--canary-equals-value",
    ];

    refusal(cli.spawn(launch(&words)).await, &words);

    let shapes = cli.refused_launch_shapes();
    assert_eq!(
        shapes,
        vec!["claude with 4 arguments: flags --model, --session-id; 1 values redacted".to_owned()],
        "expected a flag-shaped value to be counted, not named | received: {shapes:?}"
    );
}

#[tokio::test]
async fn a_flag_the_library_never_emits_is_counted_and_never_named() {
    let cli = FakeClaudeCli::new();
    let words = [
        "claude",
        "--print",
        "--dangerously-skip-permissions",
        "-p",
        "--canary-flag=value",
    ];

    refusal(cli.spawn(launch(&words)).await, &words);

    let shapes = cli.refused_launch_shapes();
    assert_eq!(
        shapes,
        vec!["claude with 5 arguments: flags --print; 3 values redacted".to_owned()],
        "expected only the flags the turn argv builder emits to be named | received: {shapes:?}"
    );
}

#[tokio::test]
async fn a_refused_launch_with_no_flags_names_the_argument_count_only() {
    let cli = FakeClaudeCli::new();
    let words = ["claude", "auth", "login"];

    refusal(cli.spawn(launch(&words)).await, &words);

    assert_eq!(
        cli.refused_launch_shapes(),
        vec!["claude with 3 arguments: no flags; 2 values redacted".to_owned()],
        "expected a flag-free refusal to say so | received: {:?}",
        cli.refused_launch_shapes()
    );
}

#[tokio::test]
async fn a_fake_that_refused_nothing_has_no_refused_launch_shapes() {
    let cli = FakeClaudeCli::new().with_turn(Run::completing());

    spawned_and_printed(&cli, &["claude", "--print"]).await;

    assert!(
        cli.refused_launch_shapes().is_empty(),
        "expected no refused launch shapes | received: {:?}",
        cli.refused_launch_shapes()
    );
}

#[tokio::test]
async fn the_refusal_error_points_at_the_shape_helper_and_stays_free_of_argv() {
    let cli = FakeClaudeCli::new();
    let words = ["claude", "--print", "--model", "sk-ant-canary-argument"];

    let message = refusal(cli.spawn(launch(&words)).await, &words);

    assert!(
        message.contains("refused_launch_shapes"),
        "expected the refusal to name FakeClaudeCli::refused_launch_shapes | received: {message}"
    );
    for argv_word in ["--model", "--print", "sk-ant-canary-argument"] {
        assert!(
            !message.contains(argv_word),
            "expected Error::Launch to carry no argv, found {argv_word} | received: {message}"
        );
    }
}

#[tokio::test]
async fn a_scripted_turn_plays_and_the_next_one_is_refused() {
    let cli = FakeClaudeCli::new().with_turn(Run::replaying("scripted"));

    assert_eq!(
        spawned_and_printed(&cli, &["claude", "--print"]).await,
        vec!["scripted"]
    );
    let message = refusal(
        cli.spawn(launch(&["claude", "--print"])).await,
        &["claude", "--print"],
    );

    assert!(
        message.contains("a turn with no Run queued"),
        "expected the second turn refused as unscripted | received: {message}"
    );
}

#[tokio::test]
async fn an_auth_subcommand_other_than_status_is_refused_and_its_argv_recorded() {
    let cli = FakeClaudeCli::new();
    let words = ["claude", "auth", "login"];

    let message = refusal(cli.spawn(launch(&words)).await, &words);

    assert!(
        message.contains("auth subcommand other than `status`") && !message.contains("login\""),
        "expected the refusal to name the subcommand and no argv | received: {message}"
    );
    let refused: Vec<Vec<String>> = cli
        .refused_launches()
        .into_iter()
        .map(|spec| spec.argv)
        .collect();
    assert_eq!(
        refused,
        vec![words.map(String::from).to_vec()],
        "expected refused_launches() to hold the received argv"
    );
}

#[tokio::test]
async fn a_refusal_is_a_launch_error_for_the_program_it_received() {
    let cli = FakeClaudeCli::new();

    let Err(error) = cli.spawn(launch(&["claude", "--print"])).await else {
        panic!("expected a refusal | received: a running child");
    };

    assert!(
        matches!(&error, Error::Launch { program, .. } if program == "claude"),
        "expected Error::Launch for program claude | received: {error:?}"
    );
}

#[tokio::test]
async fn a_line_written_to_a_turn_child_is_recorded() {
    let cli = FakeClaudeCli::new().with_turn(Run::completing());
    let mut process = cli
        .spawn(launch(&["claude", "--print"]))
        .await
        .expect("expected the turn to launch");
    let mut stdin = process.stdin.take().expect("expected a writable stdin");

    stdin
        .write_all(b"{\"type\":\"user\"}\n{\"type\":")
        .await
        .expect("expected the write to succeed");
    stdin
        .write_all(b"\"again\"}\n")
        .await
        .expect("expected the write to succeed");

    assert_eq!(
        cli.written(),
        vec![r#"{"type":"user"}"#, r#"{"type":"again"}"#],
        "expected every complete line, split across writes or not"
    );
}

#[test]
fn is_probe_names_the_three_probes_and_nothing_else() {
    let probes = [
        vec!["claude", "--version"],
        vec!["claude", "--help"],
        vec!["claude", "auth", "status"],
    ];
    let turns = [
        vec!["claude", "--print"],
        vec!["claude", "auth", "login"],
        vec!["claude"],
        vec![],
    ];
    for words in probes {
        let argv: Vec<String> = words.iter().map(|word| (*word).to_owned()).collect();
        assert!(is_probe(&argv), "expected a probe | received {argv:?}");
    }
    for words in turns {
        let argv: Vec<String> = words.iter().map(|word| (*word).to_owned()).collect();
        assert!(!is_probe(&argv), "expected a turn | received {argv:?}");
    }
}

#[test]
fn value_after_returns_the_next_argument_and_nothing_at_the_end() {
    let argv: Vec<String> = ["claude", "--effort", "high", "--print"]
        .map(String::from)
        .into();

    assert_eq!(value_after(&argv, "--effort"), Some("high"));
    assert_eq!(
        value_after(&argv, "--print"),
        None,
        "no argument follows the last flag"
    );
    assert_eq!(
        value_after(&argv, "--model"),
        None,
        "an absent flag has no value"
    );
}

fn parsed(run: Run) -> Vec<serde_json::Value> {
    let Run::Transcript { lines, exit } = run else {
        panic!("expected a transcript run | received a stalling one");
    };
    assert_eq!(
        exit.code,
        Some(0),
        "expected a clean exit | received {exit:?}"
    );
    lines
        .iter()
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|error| {
                panic!("expected a JSON line | received {line:?} ({error})")
            })
        })
        .collect()
}

#[test]
fn an_answered_schema_turn_ends_with_a_result_that_carries_the_answer() {
    let answer = serde_json::json!({"word": "ok"});

    let records = parsed(Run::answering_schema(&answer));

    let kinds: Vec<&str> = records
        .iter()
        .map(|record| record["type"].as_str().unwrap_or("<none>"))
        .collect();
    assert_eq!(
        kinds,
        ["system", "assistant", "user", "rate_limit_event", "result"]
    );
    let result = &records[4];
    assert_eq!(result["structured_output"], answer);
    assert_eq!(result["credential_expires_at"], CREDENTIAL_EXPIRY_CANARY);
    assert_eq!(result["is_error"], false);
    assert_eq!(
        records[1]["message"]["content"][0]["input"], answer,
        "expected the StructuredOutput tool call to carry the same answer"
    );
}

#[test]
fn an_unanswered_schema_turn_succeeds_without_a_structured_output() {
    let records = parsed(Run::leaving_schema_unanswered("no answer fits"));

    let result = records.last().expect("expected a result record");
    assert_eq!(result["type"], "result");
    assert_eq!(result["subtype"], "success");
    assert_eq!(result["result"], "no answer fits");
    let rejected = records
        .iter()
        .filter(|record| record["message"]["content"][0]["is_error"] == true)
        .count();
    assert_eq!(
        rejected, 2,
        "expected two rejected StructuredOutput calls | received {records:?}"
    );
    assert!(
        result.get("structured_output").is_none(),
        "expected no structured_output | received {result}"
    );
}

#[test]
fn replaying_skips_blank_lines_and_exiting_prints_nothing() {
    let Run::Transcript { lines, .. } = Run::replaying("one\n\n  \ntwo\n") else {
        panic!("expected a transcript run");
    };
    assert_eq!(lines, ["one", "two"]);

    let Run::Transcript { lines, exit } = Run::exiting(3) else {
        panic!("expected a transcript run");
    };
    assert!(lines.is_empty(), "expected no output | received {lines:?}");
    assert_eq!(exit.code, Some(3));
}

#[test]
fn stalling_keeps_the_lines_and_the_child_alive() {
    let Run::Stalls { lines } = Run::stalling(["one", "two"]) else {
        panic!("expected a stalling run");
    };
    assert_eq!(lines, ["one", "two"]);
}
