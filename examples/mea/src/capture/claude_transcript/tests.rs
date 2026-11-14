use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use mango_agent_claude::testing::{FakeClaudeCli, HELP_WITH_JSON_SCHEMA, Run, SIGNED_OUT};
use mango_external_agents::testing::{FakeHarness, FakeLauncher, FakeProcess};
use mango_external_agents::{
    ActivityKind, EventSink, ExitStatus, Interaction, InteractionId, InteractionKind,
    PermissionEffect, PermissionOption, PermissionRequest, PermissionResponse, ProcessControl,
    Question, QuestionForm, QuestionId, QuestionRequest, QuestionResponse, SessionState,
    SystemClock, TurnStream,
};

use super::*;

/// A signed-in `claude` whose help declares `--json-schema`, playing `turn_lines` for its one
/// turn and refusing any turn past it, so a second launch fails naming its argv.
fn scripted_cli(turn_lines: Vec<String>) -> Arc<FakeClaudeCli> {
    Arc::new(
        FakeClaudeCli::new()
            .with_help(HELP_WITH_JSON_SCHEMA)
            .with_turn(Run::replaying(&turn_lines.join("\n"))),
    )
}

const SCHEMA: &str = r#"{"type":"object","properties":{"word":{"type":"string"}}}"#;
/// A credential-shaped string no redaction rule names a value for.
const CANARY_TEXT: &str = "canary-credential-7731";
/// The numeric spelling of the same fact: `credential_expires_at` is a timestamp, not text.
const CANARY_NUMBER: u64 = 7_731_424_242;
/// A plan reset time, a Unix timestamp. Distinct from [`CANARY_NUMBER`] and from the header's
/// capture time, so a substring check on one cannot pass for another.
const CANARY_RESET_AT: u64 = 1_913_572_949;
/// A plan utilisation fraction, spelled with digits no other fixture number shares.
const CANARY_UTILIZATION: f64 = 0.80362;
/// A rate-limit window name, which is a key and not a value.
const CANARY_WINDOW_KEY: &str = "canary_window_key_5521";
/// A subagent type a user defined under their own agents directory.
const CANARY_AGENT_TYPE: &str = "canary-agent-type-9917";
/// The name of a member of a `rate_limit_event` this build never had, as a later Claude Code could
/// add. The vendor names it, so the name stays; its value is what is denied.
const FUTURE_MEMBER: &str = "futurePlanMember";
/// A session id the harness reads back, so a transcript keeps it.
const SESSION_ID: &str = "b01414e7-4b4b-43a2-9109-a33e21664340";

/// A `rate_limit_event` as the pinned Claude Code writes it, with every plan fact canaried: the
/// reset times, the utilisation, the overage flag, a window name and a member this build never had.
fn rate_limit_event() -> Value {
    json!({"type": "rate_limit_event",
        "rate_limit_info": {
            "status": CANARY_TEXT, "resetsAt": CANARY_RESET_AT, "rateLimitType": CANARY_TEXT,
            "overageStatus": CANARY_TEXT, "overageResetsAt": CANARY_RESET_AT + 1,
            "isUsingOverage": true,
            "unifiedWindows": {
                CANARY_WINDOW_KEY: {"utilization": CANARY_UTILIZATION,
                    "resetsAt": CANARY_RESET_AT + 2, "status": CANARY_TEXT}}},
        FUTURE_MEMBER: {"seats": CANARY_NUMBER + 1, "enabled": false},
        "uuid": "9694c392-b913-4f35-ba6f-06d9aff222d1",
        "session_id": SESSION_ID})
}

/// Every canary a plan fact or an account fact of [`rate_limit_event`] carries, as the text a
/// substring check looks for.
fn rate_limit_canaries() -> Vec<String> {
    vec![
        CANARY_TEXT.to_owned(),
        (CANARY_NUMBER + 1).to_string(),
        CANARY_RESET_AT.to_string(),
        (CANARY_RESET_AT + 1).to_string(),
        (CANARY_RESET_AT + 2).to_string(),
        CANARY_UTILIZATION.to_string(),
        CANARY_WINDOW_KEY.to_owned(),
    ]
}

/// The three machine facts a test capture ran under, built from the platform's own temporary
/// root so a Windows runner spells them as it spells its own paths.
struct MachineFacts {
    cwd: String,
    home: String,
    user: &'static str,
}

impl MachineFacts {
    fn new() -> Self {
        let root = std::env::temp_dir();
        Self {
            cwd: root.join("mea-claude-workspace").display().to_string(),
            home: root.join("mea-claude-home").display().to_string(),
            user: "capture-operator",
        }
    }

    fn paths(&self) -> Paths<'_> {
        Paths {
            cwd: &self.cwd,
            home: &self.home,
            user: self.user,
        }
    }
}

/// A schema turn as Claude Code streams it, with every private fact a real run carries.
fn answered_turn_lines(facts: &MachineFacts) -> Vec<String> {
    let answer = json!({"word": "mango"});
    [
        json!({"type": "system", "subtype": "init", "cwd": facts.cwd,
            "session_id": "b01414e7-4b4b-43a2-9109-a33e21664340",
            "tools": ["Bash", "mcp__private-server__lookup"],
            "mcp_servers": [{"name": "private-server", "status": "connected"}],
            "model": "claude-sonnet-5", "permissionMode": "default",
            "apiKeySource": "none", "claude_code_version": "2.1.325"}),
        json!({"type": "stream_event", "session_id": "b01414e7-4b4b-43a2-9109-a33e21664340",
            "event": {"type": "content_block_delta", "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": "{\"word\":\"mango\"}"}}}),
        json!({"type": "assistant", "message": {"id": "msg_1", "role": "assistant", "content": [
            {"type": "text", "text": format!("working in {}", facts.home)},
            {"type": "tool_use", "id": "toolu_answer", "name": "StructuredOutput",
             "input": answer}]}}),
        json!({"type": "user", "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_answer",
             "content": "Structured output provided successfully"}]}}),
        rate_limit_event(),
        json!({"type": "result", "subtype": "success", "is_error": false,
            "result": answer.to_string(), "structured_output": answer,
            "credential_expires_at": CANARY_NUMBER,
            "credential_hint": CANARY_TEXT,
            "email": "someone@example.com", "organization_id": CANARY_TEXT,
            "usage": {"input_tokens": 3, "output_tokens": 5}}),
    ]
    .iter()
    .map(ToString::to_string)
    .collect()
}

fn host_over(launcher: Arc<dyn ProcessLauncher>) -> HostContext {
    HostContext::builder()
        .launcher(launcher)
        .cwd(std::env::temp_dir())
        .environment(EnvSource::from_pairs([("PATH", "/usr/bin")]))
        .client_info("mea-test", "0.0.0")
        .build()
        .expect("expected a fake host context")
}

/// Runs `structured-output` over a scripted CLI.
async fn capture_over(
    launcher: Arc<dyn ProcessLauncher>,
    out_dir: &Path,
    facts: &MachineFacts,
    schema: Option<&str>,
) -> Result<()> {
    capture_on_model(launcher, out_dir, facts, schema, None).await
}

/// Runs `structured-output` over a scripted CLI, on the model `--model` named.
async fn capture_on_model(
    launcher: Arc<dyn ProcessLauncher>,
    out_dir: &Path,
    facts: &MachineFacts,
    schema: Option<&str>,
    model: Option<&str>,
) -> Result<()> {
    let tap = TurnLineTap::over(launcher);
    let host = host_over(tap.clone());
    claude_with(
        &host,
        &tap,
        out_dir,
        STRUCTURED_OUTPUT,
        schema,
        model,
        facts.paths(),
    )
    .await
}

fn transcript_dir(out_dir: &Path) -> PathBuf {
    out_dir.join("transcripts")
}

fn read_transcript(out_dir: &Path, file: &str) -> String {
    let path = transcript_dir(out_dir).join(file);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("expected {path:?} to be written | received: {error}"))
}

#[test]
fn the_registry_names_its_scenario_and_says_it_needs_a_schema() {
    assert_eq!(scenario_names(), vec!["structured-output"]);
    assert!(scenario_needs_schema("structured-output"));
    assert!(
        !scenario_needs_schema("handshake"),
        "expected a Codex scenario to need no Claude schema"
    );
}

#[test]
fn a_credential_member_is_redacted_whatever_its_value_type() {
    let facts = MachineFacts::new();
    let redacted = redact_line(
        &json!({"type": "result", "credential_expires_at": CANARY_NUMBER,
                "credentialHint": CANARY_TEXT, "access_token": CANARY_TEXT,
                "nested": {"accountUuid": CANARY_TEXT, "email": "someone@example.com"},
                "usage": {"input_tokens": 3, "output_tokens": 5,
                          "cache_creation_input_tokens": 7}})
        .to_string(),
        facts.paths(),
    )
    .expect("expected a JSON object to be kept");

    for canary in [
        CANARY_TEXT,
        &CANARY_NUMBER.to_string(),
        "someone@example.com",
    ] {
        assert!(
            !redacted.contains(canary),
            "expected no {canary:?} in the redacted line | received: {redacted}"
        );
    }
    let record: Value = serde_json::from_str(&redacted).expect("expected JSON");
    assert_eq!(record["credential_expires_at"], redact::PLACEHOLDER);
    assert_eq!(record["nested"]["accountUuid"], redact::PLACEHOLDER);
    assert_eq!(
        record["usage"],
        json!({"input_tokens": 3, "output_tokens": 5, "cache_creation_input_tokens": 7}),
        "expected the token counts, which name no credential, to survive"
    );
}

#[test]
fn which_member_names_are_credential_shaped() {
    for name in [
        "credential_expires_at",
        "access_token",
        "refreshToken",
        "apiKey",
        "api-key",
        "Authorization",
        "email",
        "organization_id",
        "account_uuid",
        "oauth",
        "client_secret",
        "rate_limit_info",
        "rateLimitType",
        "resetsAt",
        "overageResetsAt",
        "isUsingOverage",
        "utilization",
        "usage_quota",
    ] {
        assert!(
            is_credential_member(name),
            "expected {name:?} credential-shaped"
        );
    }
    for name in [
        "input_tokens",
        "output_tokens",
        "type",
        "structured_output",
        "session_id",
        "total_cost_usd",
        "modelUsage",
        "ttft_ms",
        "subagent_stats",
        "permissionMode",
    ] {
        assert!(
            !is_credential_member(name),
            "expected {name:?} to be ordinary"
        );
    }
}

fn redacted_record(record: &Value) -> Value {
    let facts = MachineFacts::new();
    let line = redact_line(&record.to_string(), facts.paths())
        .unwrap_or_else(|| panic!("expected a JSON object to be kept | received: {record}"));
    serde_json::from_str(&line).unwrap_or_else(|error| {
        panic!("expected a redacted JSON line | received: {line} ({error})")
    })
}

#[test]
fn a_rate_limit_event_keeps_only_the_members_that_identify_the_record() {
    let line = redacted_record(&rate_limit_event());
    let written = line.to_string();

    for canary in rate_limit_canaries() {
        assert!(
            !written.contains(&canary),
            "expected no {canary:?} in a redacted rate_limit_event | received: {written}"
        );
    }
    assert!(
        !written.contains("true") && !written.contains("false"),
        "expected the overage flag and the unknown flag to be gone | received: {written}"
    );
    assert_eq!(
        line,
        json!({"type": "rate_limit_event",
            "rate_limit_info": redact::PLACEHOLDER,
            FUTURE_MEMBER: redact::PLACEHOLDER,
            "uuid": "9694c392-b913-4f35-ba6f-06d9aff222d1",
            "session_id": SESSION_ID}),
        "expected only type, uuid and session_id to be kept, every other member collapsed"
    );
}

#[test]
fn a_rate_limit_event_member_this_build_never_saw_is_denied_by_default() {
    let line = redacted_record(&json!({"type": "rate_limit_event", "futureBlock": [
        {"remaining": CANARY_NUMBER, "tier": CANARY_TEXT}], "futureFlag": true,
        "futureNumber": CANARY_RESET_AT, "futureNull": null}));

    assert_eq!(line["futureBlock"], redact::PLACEHOLDER);
    assert_eq!(line["futureFlag"], redact::PLACEHOLDER);
    assert_eq!(line["futureNumber"], redact::PLACEHOLDER);
    assert!(
        line["futureNull"].is_null(),
        "expected null, which carries nothing, to stay null | received: {line}"
    );
}

#[test]
fn plan_usage_in_a_record_other_than_a_rate_limit_event_is_redacted_by_member_name() {
    let line = redacted_record(&json!({"type": "result", "subtype": "success",
        "rate_limit_info": {CANARY_WINDOW_KEY: {"utilization": CANARY_UTILIZATION}},
        "overage": {"resetsAt": CANARY_RESET_AT},
        "utilization": CANARY_UTILIZATION, "resetsAt": CANARY_RESET_AT,
        "isUsingOverage": true, "num_turns": 1}));
    let written = line.to_string();

    for canary in rate_limit_canaries() {
        assert!(
            !written.contains(&canary),
            "expected no {canary:?} outside a rate_limit_event | received: {written}"
        );
    }
    assert_eq!(line["isUsingOverage"], redact::PLACEHOLDER);
    assert_eq!(line["num_turns"], 1);
}

#[test]
fn a_tool_input_map_is_the_models_answer_whatever_the_schema_names_its_properties() {
    let line = redacted_record(&json!({"type": "assistant",
        "wire_tool_inputs": {"toolu_synthetic_1": {
            "type": CANARY_TEXT, "id": CANARY_TEXT, "model": CANARY_TEXT, "subtype": CANARY_TEXT}}}));

    assert!(
        !line.to_string().contains(CANARY_TEXT),
        "expected a property named like a structural member to be redacted in the input map | received: {line}"
    );
    assert!(
        line["wire_tool_inputs"]["toolu_synthetic_1"].is_object(),
        "expected the tool-use id key and the answer's shape to survive | received: {line}"
    );
}

#[test]
fn subagent_type_names_are_redacted_and_the_counters_beside_them_survive() {
    let line = redacted_record(&json!({"type": "result", "subagent_stats": {
        "spawned": 3, "completed": 2, "by_type": {CANARY_AGENT_TYPE: 2, "general-purpose": 1}}}));

    assert!(
        !line.to_string().contains(CANARY_AGENT_TYPE),
        "expected no user-defined agent type name | received: {line}"
    );
    assert_eq!(line["subagent_stats"]["by_type"], redact::PLACEHOLDER);
    assert_eq!(line["subagent_stats"]["spawned"], 3);
    assert_eq!(line["subagent_stats"]["completed"], 2);
}

#[test]
fn turn_scoped_numbers_and_the_requested_permission_mode_are_kept() {
    let line = redacted_record(&json!({"type": "system", "subtype": "init",
        "permissionMode": "plan"}));
    assert_eq!(line["permissionMode"], "plan");

    let line = redacted_record(&json!({"type": "result", "total_cost_usd": 0.0123,
        "duration_ms": 1234, "usage": {"input_tokens": 3, "output_tokens": 5},
        "modelUsage": {"claude-sonnet-5": {"costUSD": 0.0123, "contextWindow": 200_000}}}));
    assert_eq!(line["total_cost_usd"], 0.0123);
    assert_eq!(line["duration_ms"], 1234);
    assert_eq!(line["usage"]["output_tokens"], 5);
    assert_eq!(
        line["modelUsage"]["claude-sonnet-5"]["contextWindow"],
        200_000
    );
}

#[test]
fn a_line_keeps_its_protocol_structure_and_loses_what_the_machine_and_model_wrote() {
    let facts = MachineFacts::new();
    let lines = answered_turn_lines(&facts);
    let redacted: Vec<Value> = lines
        .iter()
        .filter_map(|line| redact_line(line, facts.paths()))
        .map(|line| serde_json::from_str(&line).expect("expected JSON"))
        .collect();

    let init = &redacted[0];
    assert_eq!(init["type"], "system");
    assert_eq!(init["subtype"], "init");
    assert_eq!(init["session_id"], "b01414e7-4b4b-43a2-9109-a33e21664340");
    assert_eq!(init["model"], "claude-sonnet-5");
    assert_eq!(init["cwd"], redact::FIXTURE_CWD);
    assert_eq!(
        init["tools"],
        json!([redact::PLACEHOLDER, redact::PLACEHOLDER]),
        "expected the tool list, which names the user's servers, to lose its entries"
    );
    assert_eq!(init["mcp_servers"][0]["name"], redact::PLACEHOLDER);
    assert_eq!(init["mcp_servers"][0]["status"], redact::PLACEHOLDER);
    assert_eq!(init["apiKeySource"], redact::PLACEHOLDER);
    assert_eq!(
        init["permissionMode"], "default",
        "expected the permission mode, which the capture requests rather than inherits, to survive"
    );

    let blocks = &redacted[2]["message"]["content"];
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[0]["text"], redact::PLACEHOLDER);
    assert_eq!(blocks[1]["name"], "StructuredOutput");
    assert_eq!(blocks[1]["id"], "toolu_answer");
    assert_eq!(
        blocks[1]["input"]["word"],
        redact::PLACEHOLDER,
        "expected the model's answer text to be private"
    );
    assert_eq!(
        redacted[3]["message"]["content"][0]["tool_use_id"],
        "toolu_answer"
    );
    assert_eq!(
        redacted[3]["message"]["content"][0]["content"],
        redact::PLACEHOLDER
    );

    let result = &redacted[5];
    assert_eq!(result["subtype"], "success");
    assert_eq!(result["is_error"], false);
    assert!(
        result["structured_output"].is_object(),
        "expected the answer's shape to survive for qualification | received: {result}"
    );
    assert_eq!(result["result"], redact::PLACEHOLDER);
}

#[test]
fn an_absolute_local_path_never_reaches_a_redacted_line() {
    let facts = MachineFacts::new();
    let spelling_the_cli_resolved = std::env::temp_dir()
        .join("symlink-resolved-workspace")
        .display()
        .to_string();
    let line = json!({
        "type": "system", "subtype": "init",
        // Not the workspace string the recorder was given, as a symlinked workspace reads.
        "cwd": spelling_the_cli_resolved,
        "session_id": "b01414e7-4b4b-43a2-9109-a33e21664340",
        "memory_paths": {"auto": format!("{}/.claude/memory", facts.home)},
        "plugins": [format!("{}/plugins/x", facts.home)],
        // A path used as a map key, and a retained structural member that holds one.
        "paths": {format!("{}/notes.md", facts.cwd): 1},
        "model": format!("{}/model", facts.home),
    })
    .to_string();

    let redacted = redact_line(&line, facts.paths()).expect("expected a JSON object");

    for private in [
        facts.cwd.as_str(),
        facts.home.as_str(),
        spelling_the_cli_resolved.as_str(),
    ] {
        let escaped = serde_json::to_string(private).expect("expected an escaped path");
        let escaped = escaped.trim_matches('"');
        assert!(
            !redacted.contains(escaped),
            "expected no local path {private:?} in the redacted line | received: {redacted}"
        );
    }
    let record: Value = serde_json::from_str(&redacted).expect("expected JSON");
    assert_eq!(record["cwd"], redact::FIXTURE_CWD);
    assert_eq!(
        record["paths"]
            .as_object()
            .map(|paths| paths.keys().cloned().collect::<Vec<_>>()),
        Some(vec![format!("{}/notes.md", redact::FIXTURE_CWD)]),
        "expected the capture's own directory rewritten inside a key"
    );
}

#[test]
fn a_tool_name_is_kept_unless_it_names_the_users_mcp_server() {
    let facts = MachineFacts::new();
    let redacted = redact_line(
        &json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"name": "x"}},
            {"type": "tool_use", "id": "toolu_2", "name": "mcp__private-server__lookup",
             "input": {}},
            {"type": "plugin", "name": "private-plugin"}]}})
        .to_string(),
        facts.paths(),
    )
    .expect("expected a JSON object");
    let record: Value = serde_json::from_str(&redacted).expect("expected JSON");
    let blocks = &record["message"]["content"];

    assert_eq!(blocks[0]["name"], "Bash");
    assert_eq!(blocks[0]["input"]["name"], redact::PLACEHOLDER);
    assert_eq!(blocks[1]["name"], redact::PLACEHOLDER);
    assert_eq!(blocks[2]["name"], redact::PLACEHOLDER);
}

#[test]
fn an_answer_member_named_type_does_not_survive_as_structure() {
    let facts = MachineFacts::new();
    let redacted = redact_line(
        &json!({"type": "result", "structured_output": {"type": "secret-answer"}}).to_string(),
        facts.paths(),
    )
    .expect("expected a JSON object");
    let record: Value = serde_json::from_str(&redacted).expect("expected JSON");

    assert_eq!(record["type"], "result");
    assert_eq!(record["structured_output"]["type"], redact::PLACEHOLDER);
}

#[test]
fn text_that_is_not_a_json_object_is_dropped_not_copied() {
    let facts = MachineFacts::new();
    for raw in [
        "warning: reading /home/someone/.claude",
        "[1,2]",
        "\"text\"",
        "",
        "{\"unfinished\":",
    ] {
        assert_eq!(
            redact_line(raw, facts.paths()),
            None,
            "expected {raw:?} to be dropped"
        );
    }
}

fn lines_of(records: &[Value]) -> Vec<String> {
    records.iter().map(ToString::to_string).collect()
}

fn qualifying_records() -> Vec<Value> {
    vec![
        json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "name": "StructuredOutput", "input": {}}]}}),
        json!({"type": "result", "subtype": "success", "structured_output": {"word": "x"}}),
    ]
}

#[test]
fn a_tool_call_and_a_result_with_an_object_answer_qualify() {
    assert_eq!(qualify(&lines_of(&qualifying_records()), None), Ok(()));
}

#[test]
fn a_transcript_missing_either_piece_of_evidence_is_unqualified_and_says_which() {
    let mut no_tool_call = qualifying_records();
    no_tool_call.remove(0);
    let mut no_answer = qualifying_records();
    no_answer[1] = json!({"type": "result", "subtype": "success", "result": "plain text"});
    let mut string_answer = qualifying_records();
    string_answer[1] = json!({"type": "result", "structured_output": "mango"});
    let mut other_tool = qualifying_records();
    other_tool[0] = json!({"type": "assistant", "message": {"content": [
        {"type": "tool_use", "name": "Bash", "input": {}}]}});
    let mut answer_before_the_last_result = qualifying_records();
    answer_before_the_last_result.push(json!({"type": "result", "subtype": "error"}));

    for (records, expected) in [
        (
            no_tool_call,
            "StructuredOutput tool_use absent; final result structured_output an object",
        ),
        (
            no_answer,
            "StructuredOutput tool_use present; final result structured_output absent",
        ),
        (
            string_answer,
            "final result structured_output present but not an object",
        ),
        (other_tool, "StructuredOutput tool_use absent"),
        (
            answer_before_the_last_result,
            "final result structured_output absent",
        ),
        (
            Vec::new(),
            "final result structured_output not received (no result record)",
        ),
    ] {
        let received = qualify(&lines_of(&records), None)
            .expect_err("expected the transcript to be unqualified");
        assert!(
            received.contains(expected),
            "expected {expected:?} | received: {received}"
        );
    }
}

#[test]
fn an_unqualified_report_names_the_vendor_failure_and_only_structural_words() {
    let received = qualify(
        &lines_of(&[json!({"type": "result", "subtype": "error_max_turns"})]),
        Some("claude-turn-failed (stream ended)"),
    )
    .expect_err("expected an unqualified transcript");

    assert!(
        received.contains("record kinds: result/error_max_turns"),
        "received: {received}"
    );
    assert!(
        received.contains("turn failed: claude-turn-failed (stream ended)"),
        "received: {received}"
    );
}

/// A vendor failure the way the harness reports one: a code and the vendor's own reason.
struct FakeVendorFailure;

impl FakeVendorFailure {
    const CODE: &'static str = "claude-turn-failed";
    const MESSAGE: &'static str = "the model fake-model-1 does not exist";

    fn error() -> Error {
        Error::Vendor(mango_external_agents::VendorError::new(
            mango_external_agents::ErrorCode::from_static(Self::CODE),
            Self::MESSAGE,
        ))
    }
}

#[test]
fn the_primary_failure_line_carries_the_vendor_message_and_the_scenario() {
    let line = primary_failure_line("structured-output", &FakeVendorFailure::error());

    assert!(
        line.starts_with("claude/structured-output: capture failed before cleanup: "),
        "expected the scenario and the cleanup context | received: {line}"
    );
    assert!(
        line.contains(FakeVendorFailure::CODE) && line.contains(FakeVendorFailure::MESSAGE),
        "expected the vendor code and message as `<code>: vendor failure: <message>` | received: {line}"
    );
}

#[test]
fn the_header_names_the_scenario_the_cli_version_and_the_capture_time() {
    let lines = header(&SCENARIOS[0], Some("2.1.325"), Some(1_790_000_000), None);

    assert!(
        lines[0].starts_with("# claude/structured-output: "),
        "received: {lines:?}"
    );
    let metadata: Value =
        serde_json::from_str(lines[1].strip_prefix("# capture ").unwrap_or_else(|| {
            panic!("expected a `# capture {{json}}` line | received: {lines:?}")
        }))
        .expect("expected JSON metadata");
    assert_eq!(metadata["claudeVersion"], "2.1.325");
    assert_eq!(metadata["capturedAtUnixSeconds"], 1_790_000_000);
    assert_eq!(metadata["captureVersion"], env!("CARGO_PKG_VERSION"));

    let unknown = header(&SCENARIOS[0], None, None, None);
    assert!(
        unknown[1].contains("\"claudeVersion\":null"),
        "received: {unknown:?}"
    );
    assert_eq!(
        lines.len(),
        2,
        "expected no model line without --model | received: {lines:?}"
    );
}

#[test]
fn a_header_names_the_model_between_the_label_and_the_capture_metadata_when_one_was_chosen() {
    let plain = header(&SCENARIOS[0], Some("2.1.325"), Some(1_790_000_000), None);
    let pinned = header(
        &SCENARIOS[0],
        Some("2.1.325"),
        Some(1_790_000_000),
        Some("claude-opus-5"),
    );

    assert_eq!(
        pinned,
        vec![
            plain[0].clone(),
            String::from("# model: claude-opus-5"),
            plain[1].clone()
        ],
        "expected the Codex header shape: label, `# model: ID`, then the rest"
    );
}

#[test]
fn a_transcript_file_is_header_then_label_then_body_ending_in_a_newline() {
    let header = vec![String::from("# one"), String::from("# two")];
    let body = vec![String::from("{}")];

    assert_eq!(join(&header, None, &body), "# one\n# two\n{}\n");
    assert_eq!(
        join(&header, Some("# incomplete capture"), &body),
        "# one\n# two\n# incomplete capture\n{}\n"
    );
}

#[test]
fn only_the_harnesss_stream_json_launch_is_a_turn() {
    let argv = |words: &[&str]| {
        words
            .iter()
            .map(|word| (*word).to_owned())
            .collect::<Vec<_>>()
    };

    assert!(is_turn_launch(&argv(&[
        "claude",
        "--print",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
    ])));
    for probe in [
        argv(&["claude", "--version"]),
        argv(&["claude", "--help"]),
        argv(&["claude", "auth", "status"]),
        argv(&["claude", "--output-format", "json"]),
        argv(&["claude", "--output-format"]),
    ] {
        assert!(!is_turn_launch(&probe), "expected {probe:?} to be a probe");
    }
}

/// A child whose stdout arrives as the chunks it was given, however they cut the lines.
struct ChunkedStdout {
    chunks: VecDeque<Vec<u8>>,
}

#[async_trait::async_trait]
impl ByteSource for ChunkedStdout {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        Ok(self.chunks.pop_front())
    }
}

/// A launcher whose children print these chunks, whatever they are asked to run.
struct ChunkedLauncher {
    inner: FakeLauncher,
    chunks: Vec<&'static [u8]>,
}

#[async_trait::async_trait]
impl ProcessLauncher for ChunkedLauncher {
    async fn spawn(&self, spec: LaunchSpec) -> Result<ManagedProcess> {
        self.inner
            .push(FakeProcess::transcript(Vec::<String>::new()));
        let mut process = self.inner.spawn(spec).await?;
        process.stdout = Box::new(ChunkedStdout {
            chunks: self.chunks.iter().map(|chunk| chunk.to_vec()).collect(),
        });
        Ok(process)
    }
}

fn launch_of(words: &[&str]) -> LaunchSpec {
    LaunchSpec {
        argv: words.iter().map(|word| (*word).to_owned()).collect(),
        cwd: std::env::temp_dir(),
        env: BTreeMap::new(),
        stdin: false,
        hide_window: false,
    }
}

async fn read_to_end(process: &mut ManagedProcess) -> Vec<u8> {
    let mut bytes = Vec::new();
    while let Some(chunk) = process.stdout.next_chunk().await.expect("expected a chunk") {
        bytes.extend(chunk);
    }
    bytes
}

#[tokio::test]
async fn the_tap_keeps_whole_lines_across_chunk_cuts_and_still_passes_every_byte_on() {
    let chunks: Vec<&'static [u8]> = vec![b"{\"a\":1}\n{\"b\"", b":2}\r\n\n", b"{\"c\":3}"];
    let tap = TurnLineTap::over(Arc::new(ChunkedLauncher {
        inner: FakeLauncher::new(),
        chunks: chunks.clone(),
    }));

    let mut process = tap
        .spawn(launch_of(&["claude", "--output-format", "stream-json"]))
        .await
        .expect("expected the turn launch");
    let passed_on = read_to_end(&mut process).await;

    assert_eq!(
        passed_on,
        chunks.concat(),
        "expected the harness to see every byte unchanged"
    );
    assert_eq!(
        tap.take_lines(),
        vec!["{\"a\":1}", "{\"b\":2}", "{\"c\":3}"],
        "expected complete lines only, the unterminated last one included"
    );
    assert!(
        tap.take_lines().is_empty(),
        "expected take_lines to leave the tap empty"
    );
}

#[tokio::test]
async fn the_tap_keeps_nothing_from_a_probe_launch() {
    let tap = TurnLineTap::over(Arc::new(ChunkedLauncher {
        inner: FakeLauncher::new(),
        chunks: vec![b"2.1.325 (Claude Code)\n"],
    }));

    let mut process = tap
        .spawn(launch_of(&["claude", "--version"]))
        .await
        .expect("expected the probe launch");
    let passed_on = read_to_end(&mut process).await;

    assert_eq!(passed_on, b"2.1.325 (Claude Code)\n");
    assert!(
        tap.take_lines().is_empty(),
        "expected a probe's output to stay out of the tap"
    );
}

#[tokio::test]
async fn a_qualified_capture_writes_the_redacted_transcript_and_nothing_private() {
    let facts = MachineFacts::new();
    let out = tempfile::tempdir().expect("expected an output directory");
    let cli = scripted_cli(answered_turn_lines(&facts));

    capture_over(cli.clone(), out.path(), &facts, Some(SCHEMA))
        .await
        .unwrap_or_else(|error| panic!("expected the capture to qualify, received {error}"));

    let written = read_transcript(out.path(), "structured-output.jsonl");
    let mut lines = written.lines();
    assert!(
        lines
            .next()
            .is_some_and(|line| line.starts_with("# claude/structured-output: ")),
        "received: {written}"
    );
    let metadata = lines.next().unwrap_or_default();
    assert!(
        metadata.starts_with("# capture {")
            && metadata.contains("\"claudeVersion\":\"2.1.270 (Claude Code)\""),
        "expected the CLI's --version in the header | received: {metadata}"
    );
    let body: Vec<Value> = lines
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|error| {
                panic!("expected a JSON record after the header | received: {line:?} ({error})")
            })
        })
        .collect();
    assert_eq!(
        body.len(),
        6,
        "expected every record the CLI streamed | received: {written}"
    );
    assert_eq!(body[2]["message"]["content"][1]["name"], "StructuredOutput");
    assert_eq!(body[4]["type"], "rate_limit_event");
    assert!(body[5]["structured_output"].is_object());

    for private in [
        CANARY_NUMBER.to_string(),
        "someone@example.com".to_owned(),
        facts.cwd.clone(),
        facts.home.clone(),
    ]
    .into_iter()
    .chain(rate_limit_canaries())
    {
        let escaped = serde_json::to_string(&private).expect("expected an escaped value");
        assert!(
            !written.contains(escaped.trim_matches('"')),
            "expected {private:?} to stay out of the file | received: {written}"
        );
    }

    let turns = cli.turn_argvs();
    assert_eq!(
        turns.len(),
        1,
        "expected one turn launch | received: {turns:?}"
    );
    assert!(
        turns[0].iter().any(|word| word == "--json-schema"),
        "expected the harness's own argv to carry the schema | received: {:?}",
        turns[0]
    );
    assert!(
        turns[0]
            .windows(2)
            .any(|pair| pair[0] == "--permission-mode" && pair[1] == "plan"),
        "expected the capture to state read-only rather than inherit the operator's mode | received: {:?}",
        turns[0]
    );
    assert!(
        !transcript_dir(out.path())
            .join("structured-output.partial.jsonl")
            .exists()
            && !transcript_dir(out.path())
                .join("structured-output.unqualified.jsonl")
                .exists(),
        "expected no diagnostic beside a qualified transcript"
    );
}

#[tokio::test]
async fn a_chosen_model_reaches_the_turn_argv_and_the_transcript_header() {
    let facts = MachineFacts::new();
    let out = tempfile::tempdir().expect("expected an output directory");
    let cli = scripted_cli(answered_turn_lines(&facts));

    capture_on_model(
        cli.clone(),
        out.path(),
        &facts,
        Some(SCHEMA),
        Some("claude-opus-5"),
    )
    .await
    .unwrap_or_else(|error| panic!("expected the capture to qualify, received {error}"));

    let turns = cli.turn_argvs();
    assert!(
        turns[0]
            .windows(2)
            .any(|pair| pair[0] == "--model" && pair[1] == "claude-opus-5"),
        "expected the harness's argv to carry --model claude-opus-5 | received: {:?}",
        turns[0]
    );
    let written = read_transcript(out.path(), "structured-output.jsonl");
    let header: Vec<&str> = written.lines().take(3).collect();
    assert_eq!(
        header[1], "# model: claude-opus-5",
        "expected the model on the second header line | received: {header:?}"
    );
    assert!(
        header[2].starts_with("# capture {"),
        "expected the capture metadata after the model line | received: {header:?}"
    );
}

#[tokio::test]
async fn a_capture_without_a_model_passes_no_model_flag_and_writes_no_model_line() {
    let facts = MachineFacts::new();
    let out = tempfile::tempdir().expect("expected an output directory");
    let cli = scripted_cli(answered_turn_lines(&facts));

    capture_over(cli.clone(), out.path(), &facts, Some(SCHEMA))
        .await
        .unwrap_or_else(|error| panic!("expected the capture to qualify, received {error}"));

    let turns = cli.turn_argvs();
    assert!(
        !turns[0].iter().any(|word| word == "--model"),
        "expected the user's configured model to stay in force | received: {:?}",
        turns[0]
    );
    let written = read_transcript(out.path(), "structured-output.jsonl");
    assert!(
        !written.contains("# model:"),
        "expected no model line without --model | received: {written}"
    );
}

#[tokio::test]
async fn a_model_the_harness_cannot_put_on_argv_stops_the_capture_before_any_process_starts() {
    let facts = MachineFacts::new();
    let out = tempfile::tempdir().expect("expected an output directory");
    let launcher = Arc::new(FakeLauncher::new());

    let error = capture_on_model(
        launcher.clone(),
        out.path(),
        &facts,
        Some(SCHEMA),
        Some("--dangerously-skip-permissions"),
    )
    .await
    .expect_err("expected a model that reads as a flag to be refused");

    assert!(
        matches!(&error, Error::HostConfiguration { expected, received }
            if *expected == "a non-empty Claude model identifier that can occupy a --model value"
                && received == "30 code points"),
        "expected the harness's own refusal | received: {error:?}"
    );
    assert!(
        launcher.launches().is_empty(),
        "expected no launch | received: {:?}",
        launcher.launches()
    );
    assert!(
        !transcript_dir(out.path()).exists(),
        "expected nothing written"
    );
}

#[tokio::test]
async fn a_qualified_capture_removes_the_diagnostics_an_earlier_attempt_left() {
    let facts = MachineFacts::new();
    let out = tempfile::tempdir().expect("expected an output directory");
    let directory = transcript_dir(out.path());
    std::fs::create_dir_all(&directory).expect("expected the transcript directory");
    for stale in [
        "structured-output.partial.jsonl",
        "structured-output.unqualified.jsonl",
    ] {
        std::fs::write(directory.join(stale), "# stale\n").expect("expected a stale file");
    }
    let cli = scripted_cli(answered_turn_lines(&facts));

    capture_over(cli, out.path(), &facts, Some(SCHEMA))
        .await
        .unwrap_or_else(|error| panic!("expected the capture to qualify, received {error}"));

    let left: Vec<String> = std::fs::read_dir(&directory)
        .expect("expected the transcript directory")
        .map(|entry| {
            entry
                .expect("expected an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(left, vec!["structured-output.jsonl"], "received: {left:?}");
}

#[tokio::test]
async fn a_turn_that_never_answered_is_kept_unqualified_and_fails_naming_what_is_missing() {
    let facts = MachineFacts::new();
    let out = tempfile::tempdir().expect("expected an output directory");
    let directory = transcript_dir(out.path());
    std::fs::create_dir_all(&directory).expect("expected the transcript directory");
    std::fs::write(
        directory.join("structured-output.jsonl"),
        "# earlier qualified capture\n",
    )
    .expect("expected an earlier capture");
    let plain_answer = lines_of(&[
        json!({"type": "system", "subtype": "init", "cwd": facts.cwd,
               "session_id": "b01414e7-4b4b-43a2-9109-a33e21664340"}),
        json!({"type": "assistant", "message": {"role": "assistant", "content": [
            {"type": "text", "text": "mango"}]}}),
        json!({"type": "result", "subtype": "success", "is_error": false, "result": "mango"}),
    ]);
    let cli = scripted_cli(plain_answer);

    let error = capture_over(cli, out.path(), &facts, Some(SCHEMA))
        .await
        .expect_err("expected a turn without structured output to fail the capture");

    let Error::Protocol { expected, received } = &error else {
        panic!("expected a protocol failure | received: {error:?}");
    };
    assert!(
        expected.contains("StructuredOutput tool_use"),
        "received: {expected}"
    );
    assert!(
        received.contains("StructuredOutput tool_use absent"),
        "received: {received}"
    );
    let unqualified = read_transcript(out.path(), "structured-output.unqualified.jsonl");
    assert!(
        unqualified
            .lines()
            .any(|line| line == "# unqualified capture"),
        "expected the label | received: {unqualified}"
    );
    assert_eq!(
        read_transcript(out.path(), "structured-output.jsonl"),
        "# earlier qualified capture\n",
        "expected an earlier qualified transcript to stay as it was"
    );
}

#[tokio::test]
async fn a_signed_out_cli_is_refused_without_signing_in_or_starting_a_turn() {
    let facts = MachineFacts::new();
    let out = tempfile::tempdir().expect("expected an output directory");
    let cli = Arc::new(
        FakeClaudeCli::new()
            .with_help(HELP_WITH_JSON_SCHEMA)
            .with_auth(SIGNED_OUT)
            .with_turn(Run::replaying(&answered_turn_lines(&facts).join("\n"))),
    );

    let error = capture_over(cli.clone(), out.path(), &facts, Some(SCHEMA))
        .await
        .expect_err("expected a signed-out CLI to be refused");

    let Error::HostConfiguration { expected, received } = &error else {
        panic!("expected a host configuration refusal | received: {error:?}");
    };
    assert!(
        expected.contains("already signed in"),
        "received: {expected}"
    );
    assert!(
        received.starts_with("signed out; sign in yourself with `"),
        "received: {received}"
    );
    assert!(cli.turn_argvs().is_empty(), "expected no turn launch");
    // Every `auth` launch: the fake refuses any subcommand but `status`, so a login attempt would
    // not appear here and would fail the capture with its own refusal instead.
    let auth_launches: Vec<Vec<String>> = cli
        .launches()
        .into_iter()
        .map(|spec| spec.argv)
        .filter(|argv| argv.get(1).is_some_and(|word| word == "auth"))
        .collect();
    assert_eq!(
        auth_launches.len(),
        1,
        "expected the one status probe, and no login launch | received: {auth_launches:?}"
    );
    assert_eq!(
        auth_launches[0].get(2).map(String::as_str),
        Some("status"),
        "expected the probe to be `auth status` | received: {:?}",
        auth_launches[0]
    );
    assert!(
        !transcript_dir(out.path())
            .join("structured-output.jsonl")
            .exists()
    );
}

#[tokio::test]
async fn a_scenario_that_needs_a_schema_is_refused_before_any_process_starts() {
    let facts = MachineFacts::new();
    let out = tempfile::tempdir().expect("expected an output directory");
    let launcher = Arc::new(FakeLauncher::new());

    let error = capture_over(launcher.clone(), out.path(), &facts, None)
        .await
        .expect_err("expected a missing schema to be refused");

    assert!(
        matches!(&error, Error::HostConfiguration { received, .. }
            if received == "no schema for claude/structured-output"),
        "received: {error:?}"
    );
    assert!(
        launcher.launches().is_empty(),
        "expected no launch | received: {:?}",
        launcher.launches()
    );
    assert!(
        !transcript_dir(out.path()).exists(),
        "expected nothing written"
    );
}

#[tokio::test]
async fn a_schema_the_harness_refuses_stops_the_capture_before_any_process_starts() {
    let facts = MachineFacts::new();
    let out = tempfile::tempdir().expect("expected an output directory");
    let launcher = Arc::new(FakeLauncher::new());

    let error = capture_over(launcher.clone(), out.path(), &facts, Some("[1, 2]"))
        .await
        .expect_err("expected a schema that is not an object to be refused");

    assert!(
        matches!(&error, Error::HostConfiguration { .. }),
        "expected the harness's own refusal | received: {error:?}"
    );
    assert!(
        launcher.launches().is_empty(),
        "expected no launch | received: {:?}",
        launcher.launches()
    );
    assert!(
        !transcript_dir(out.path()).exists(),
        "expected nothing written"
    );
}

#[tokio::test]
async fn an_unknown_scenario_is_refused_with_the_name_it_received() {
    let facts = MachineFacts::new();
    let out = tempfile::tempdir().expect("expected an output directory");
    let tap = TurnLineTap::over(Arc::new(FakeLauncher::new()));
    let host = host_over(tap.clone());

    let error = claude_with(
        &host,
        &tap,
        out.path(),
        "handshake",
        Some(SCHEMA),
        None,
        facts.paths(),
    )
    .await
    .expect_err("expected a Codex scenario to be refused");

    assert!(
        matches!(&error, Error::HostConfiguration { expected, received }
            if *expected == "a named Claude transcript scenario" && received == "handshake"),
        "received: {error:?}"
    );
}

/// A control whose child cannot be confirmed gone: every stop request fails.
struct UnstoppableControl {
    inner: Arc<dyn ProcessControl>,
}

#[async_trait::async_trait]
impl ProcessControl for UnstoppableControl {
    fn pid(&self) -> Option<u32> {
        Some(4242)
    }

    fn stderr_tail(&self) -> String {
        self.inner.stderr_tail()
    }

    async fn wait(&self) -> Result<ExitStatus> {
        self.inner.wait().await
    }

    async fn kill(&self, _reason: CancelReason) -> Result<()> {
        Err(Error::Link {
            peer: String::from("fake claude"),
            message: String::from("the kill was refused"),
        })
    }
}

/// A CLI whose turn child cannot be stopped; probes run as on the shared fake.
struct UnstoppableTurnCli {
    cli: Arc<FakeClaudeCli>,
}

#[async_trait::async_trait]
impl ProcessLauncher for UnstoppableTurnCli {
    async fn spawn(&self, spec: LaunchSpec) -> Result<ManagedProcess> {
        let turn = is_turn_launch(&spec.argv);
        let mut process = self.cli.spawn(spec).await?;
        if turn {
            process.control = Arc::new(UnstoppableControl {
                inner: process.control,
            });
        }
        Ok(process)
    }
}

#[tokio::test]
async fn a_turn_whose_child_cannot_be_stopped_leaves_only_an_incomplete_diagnostic() {
    let facts = MachineFacts::new();
    let out = tempfile::tempdir().expect("expected an output directory");
    let cli = Arc::new(UnstoppableTurnCli {
        cli: scripted_cli(answered_turn_lines(&facts)),
    });

    let result = capture_over(cli, out.path(), &facts, Some(SCHEMA)).await;

    assert!(
        result.is_err(),
        "expected an unconfirmed stop to fail a capture whose conversation completed"
    );
    let partial = read_transcript(out.path(), "structured-output.partial.jsonl");
    assert!(
        partial.lines().any(|line| line == "# incomplete capture"),
        "expected the label | received: {partial}"
    );
    assert!(
        !transcript_dir(out.path())
            .join("structured-output.jsonl")
            .exists(),
        "expected no qualified transcript from a child nobody confirmed gone"
    );
    for private in [CANARY_TEXT, facts.home.as_str()] {
        assert!(
            !partial.contains(private),
            "expected {private:?} redacted | received: {partial}"
        );
    }
}

/// Which interaction a [`PromptingSession`] raises when its turn starts.
#[derive(Clone, Copy)]
enum Prompt {
    Approval,
    Question,
}

/// A session whose turn asks for a permission or an answer and counts every reply it receives.
///
/// The recorder must leave both unanswered, so the counter is the observable half of that rule.
struct PromptingSession {
    inner: Box<dyn Session>,
    prompt: Prompt,
    replies: AtomicUsize,
}

#[async_trait::async_trait]
impl Session for PromptingSession {
    fn state(&self) -> &SessionState {
        self.inner.state()
    }

    async fn start_turn(&self, request: TurnRequest) -> Result<TurnStream> {
        let session_id = self.ids().session_id;
        let (sink, events) = EventSink::new(
            session_id.clone(),
            request.turn_id.clone(),
            request.attempt,
            Arc::new(SystemClock),
            3,
        );
        let expires = SystemTime::now() + Duration::from_secs(60);
        match self.prompt {
            Prompt::Approval => {
                let approval = PermissionRequest::new(
                    Interaction::new(
                        InteractionId::new("capture-approval"),
                        InteractionKind::Permission,
                        session_id,
                        expires,
                    ),
                    ActivityKind::Command,
                    "change a file",
                    vec![PermissionOption::new("deny", PermissionEffect::Reject)],
                );
                sink.emit(EventKind::ApprovalRequested { request: approval })
                    .await?;
            }
            Prompt::Question => {
                let question = QuestionRequest::new(
                    Interaction::new(
                        InteractionId::new("capture-question"),
                        InteractionKind::Question,
                        session_id,
                        expires,
                    ),
                    vec![
                        Question::new(
                            QuestionId::new("branch"),
                            "Which branch?",
                            QuestionForm::FreeText { placeholder: None },
                        )
                        .required(),
                    ],
                );
                sink.emit(EventKind::QuestionAsked { request: question })
                    .await?;
            }
        }
        Ok(TurnStream::accepted(
            request.turn_id,
            request.attempt,
            "prompting-turn",
            events,
        ))
    }

    async fn respond(&self, _response: PermissionResponse) -> Result<()> {
        self.replies.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn answer(&self, _response: QuestionResponse) -> Result<()> {
        self.replies.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn cancel(&self, reason: CancelReason) -> Result<()> {
        self.inner.cancel(reason).await
    }

    async fn close(&self, reason: CloseReason) -> Result<()> {
        self.inner.close(reason).await
    }
}

async fn converse_with_prompt(prompt: Prompt) -> (Result<Option<String>>, usize) {
    let inner = FakeHarness::new()
        .open_session(
            &host_over(Arc::new(FakeLauncher::new())),
            OpenSession::new("mea-test"),
        )
        .await
        .expect("expected a fake session");
    let session = PromptingSession {
        inner,
        prompt,
        replies: AtomicUsize::new(0),
    };
    let outcome = converse(&session, &SCENARIOS[0]).await;
    (outcome, session.replies.load(Ordering::SeqCst))
}

#[tokio::test]
async fn an_approval_request_fails_the_capture_unanswered() {
    let (outcome, replies) = converse_with_prompt(Prompt::Approval).await;

    match outcome {
        Err(Error::Protocol { received, .. }) => assert_eq!(
            received, "an approval request; left unanswered",
            "expected the approval named in the refusal"
        ),
        other => panic!("expected Error::Protocol for an approval request | received: {other:?}"),
    }
    assert_eq!(
        replies, 0,
        "expected no permission granted or denied by the recorder"
    );
}

#[tokio::test]
async fn a_question_fails_the_capture_unanswered() {
    let (outcome, replies) = converse_with_prompt(Prompt::Question).await;

    match outcome {
        Err(Error::Protocol { received, .. }) => assert_eq!(
            received, "a question; left unanswered",
            "expected the question named in the refusal"
        ),
        other => panic!("expected Error::Protocol for a question | received: {other:?}"),
    }
    assert_eq!(replies, 0, "expected no answer sent by the recorder");
}
