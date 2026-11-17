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

/// [`super::redact_line`] for a test that does not read the drift report. The line is a recording
/// of its own: the `StructuredOutput` calls it names are the answer's.
fn redact_line(raw: &str, paths: Paths<'_>) -> Option<String> {
    let answers = AnswerCalls::in_lines(&[raw.to_owned()]);
    super::redact_line(raw, &answers, paths, &mut DriftReport::default())
}

/// Collects the notices a capture hands to the operator, in place of stderr.
#[derive(Default)]
struct RecordedNotices {
    lines: Mutex<Vec<String>>,
}

impl CaptureNotices for RecordedNotices {
    fn notice(&self, line: &str) {
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(line.to_owned());
    }
}

impl RecordedNotices {
    fn recorded(&self) -> Vec<String> {
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

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
/// A `credential_expires_at` as Claude Code 2.1.325 writes it: a string.
const CANARY_EXPIRES_AT_TEXT: &str = "2031-08-19T04:17:09.731Z";
/// A number a record could carry in a credential-shaped member. A default of "redact strings"
/// keeps it, so only the member name hides it. Claude Code 2.1.325 does not emit
/// `credential_expires_at` as a number; a later release could.
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
            "credential_expires_at": CANARY_EXPIRES_AT_TEXT,
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
    capture_noticed(
        launcher,
        out_dir,
        facts,
        schema,
        model,
        &RecordedNotices::default(),
    )
    .await
}

/// Runs `structured-output` over a scripted CLI and hands its notices to `notices`.
async fn capture_noticed(
    launcher: Arc<dyn ProcessLauncher>,
    out_dir: &Path,
    facts: &MachineFacts,
    schema: Option<&str>,
    model: Option<&str>,
    notices: &RecordedNotices,
) -> Result<()> {
    let tap = TurnLineTap::over(launcher);
    let host = host_over(tap.clone());
    claude_with(
        &host,
        &tap,
        notices,
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
fn the_registry_names_its_scenario() {
    assert_eq!(scenario_names(), vec!["structured-output"]);
    assert!(
        !scenario_names().contains(&"handshake"),
        "expected a Codex scenario to be absent from the Claude registry"
    );
}

/// Redacts one `result` record whose `credential_expires_at` is `expires_at`, and returns the
/// redacted line and its parsed record.
fn redact_result_expiring_at(expires_at: Value) -> (String, Value) {
    let facts = MachineFacts::new();
    let redacted = redact_line(
        &json!({"type": "result", "credential_expires_at": expires_at}).to_string(),
        facts.paths(),
    )
    .expect("expected a JSON object to be kept");
    let record = serde_json::from_str(&redacted).expect("expected JSON");
    (redacted, record)
}

/// The type Claude Code 2.1.325 emits. The default string rule would hide it too, so this pins the
/// observed shape; the name rule is what [`a_numeric_credential_expiry_is_redacted_by_name`] pins.
#[test]
fn a_string_credential_expiry_is_redacted() {
    let (redacted, record) = redact_result_expiring_at(json!(CANARY_EXPIRES_AT_TEXT));

    assert!(
        !redacted.contains(CANARY_EXPIRES_AT_TEXT),
        "expected no {CANARY_EXPIRES_AT_TEXT:?} in the redacted line | received: {redacted}"
    );
    assert_eq!(record["credential_expires_at"], redact::PLACEHOLDER);
}

/// A number is not a string, so the walk would keep it: only the member name hides it.
#[test]
fn a_numeric_credential_expiry_is_redacted_by_name() {
    let (redacted, record) = redact_result_expiring_at(json!(CANARY_NUMBER));

    assert!(
        !redacted.contains(&CANARY_NUMBER.to_string()),
        "expected no {CANARY_NUMBER} in the redacted line | received: {redacted}"
    );
    assert_eq!(record["credential_expires_at"], redact::PLACEHOLDER);
}

#[test]
fn credential_and_account_members_are_redacted_and_token_counts_survive() {
    let facts = MachineFacts::new();
    let redacted = redact_line(
        &json!({"type": "result", "credentialHint": CANARY_TEXT, "access_token": CANARY_TEXT,
                "nested": {"accountUuid": CANARY_TEXT, "email": "someone@example.com"},
                "usage": {"input_tokens": 3, "output_tokens": 5,
                          "cache_creation_input_tokens": 7}})
        .to_string(),
        facts.paths(),
    )
    .expect("expected a JSON object to be kept");

    for canary in [CANARY_TEXT, "someone@example.com"] {
        assert!(
            !redacted.contains(canary),
            "expected no {canary:?} in the redacted line | received: {redacted}"
        );
    }
    let record: Value = serde_json::from_str(&redacted).expect("expected JSON");
    assert_eq!(record["credentialHint"], redact::PLACEHOLDER);
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

/// Redacts the records of one recording together, as a capture does, so a record can depend on
/// another one.
fn redacted_records(records: &[Value]) -> Vec<Value> {
    let facts = MachineFacts::new();
    let lines = lines_of(records);
    let answers = AnswerCalls::in_lines(&lines);
    let mut drift = DriftReport::default();
    lines
        .iter()
        .map(|line| {
            let kept = super::redact_line(line, &answers, facts.paths(), &mut drift)
                .unwrap_or_else(|| panic!("expected a JSON object to be kept | received: {line}"));
            serde_json::from_str(&kept).unwrap_or_else(|error| {
                panic!("expected a redacted JSON line | received: {kept} ({error})")
            })
        })
        .collect()
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

/// A user-configuration boolean of `system/init` that no member-name rule matches, as Claude Code
/// 2.1.325 writes it.
const INIT_CONFIGURATION_FLAGS: [&str; 2] = ["analytics_disabled", "product_feedback_disabled"];
/// A member of `system/init` this build never had, as a later Claude Code could add.
const FUTURE_INIT_MEMBER: &str = "futureAccountMember";

/// The error a canary test raises when an init member outside [`INIT_STRUCTURE`] survives: the
/// received member and value, and the allow-list it should have been inside.
fn init_member_leak(member: &str, line: &Value) -> String {
    format!(
        "expected member {member:?} outside the init allow-list {INIT_STRUCTURE:?} to be \
         {placeholder:?} | received: {kept}",
        placeholder = redact::PLACEHOLDER,
        kept = line[member],
    )
}

#[test]
fn an_init_record_denies_the_user_configuration_booleans() {
    for member in INIT_CONFIGURATION_FLAGS {
        // The booleans carry a choice the user made, so a `true` and a `false` are both private.
        for flag in [true, false] {
            let line = redacted_record(&json!({"type": "system", "subtype": "init",
                "session_id": SESSION_ID, member: flag}));
            assert_eq!(
                line[member],
                redact::PLACEHOLDER,
                "{}",
                init_member_leak(member, &line)
            );
        }
    }
}

#[test]
fn an_init_member_this_build_never_saw_is_denied_by_default() {
    for value in [
        json!(true),
        json!(CANARY_NUMBER),
        json!(CANARY_TEXT),
        json!({"seats": 4}),
    ] {
        let line = redacted_record(&json!({"type": "system", "subtype": "init",
            "session_id": SESSION_ID, FUTURE_INIT_MEMBER: value}));
        assert_eq!(
            line[FUTURE_INIT_MEMBER],
            redact::PLACEHOLDER,
            "{}",
            init_member_leak(FUTURE_INIT_MEMBER, &line)
        );
    }
    let line = redacted_record(&json!({"type": "system", "subtype": "init",
        FUTURE_INIT_MEMBER: null}));
    assert!(
        line[FUTURE_INIT_MEMBER].is_null(),
        "expected null, which carries nothing, to stay null | received: {line}"
    );
}

#[test]
fn an_init_record_keeps_the_members_the_reducer_reads() {
    let line = redacted_record(&json!({"type": "system", "subtype": "init",
        "uuid": "58aa4f96-663d-4513-8b37-2ba65beb3743", "session_id": SESSION_ID,
        "cwd": "/somewhere/else", "model": "claude-sonnet-5", "permissionMode": "plan", "claude_code_version": "2.1.325",
        "slash_commands": ["review", "compact"], "skills": ["review"],
        "terminal_slash_commands": ["compact"], "capabilities": ["msg_lifecycle_v1"],
        "plugins": [{"name": "code-review"}],
        "analytics_disabled": false, "output_style": "default", "tools": ["Bash"]}));

    for member in INIT_STRUCTURE {
        assert!(
            !line[member].is_null(),
            "expected allow-listed member {member:?} to be kept | received: {line}"
        );
    }
    assert_eq!(line["session_id"], SESSION_ID);
    assert_eq!(line["model"], "claude-sonnet-5");
    assert_eq!(line["permissionMode"], "plan");
    assert_eq!(
        line["slash_commands"],
        json!([redact::PLACEHOLDER, redact::PLACEHOLDER]),
        "expected the command catalog to keep its length, which the reducer counts"
    );
    for member in ["analytics_disabled", "output_style", "tools"] {
        assert_eq!(
            line[member],
            redact::PLACEHOLDER,
            "{}",
            init_member_leak(member, &line)
        );
    }
}

#[test]
fn an_init_member_name_outside_a_system_init_record_is_not_denied_by_the_allow_list() {
    let line = redacted_record(&json!({"type": "system", "subtype": "status",
        "analytics_disabled": false, "num_turns": 1}));
    assert_eq!(line["analytics_disabled"], false);
    assert_eq!(line["num_turns"], 1);
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

/// Parameter and field names a tool author can pick that collide with a structural member, a
/// model id member or a tool-call name, each paired with a value only that author would write.
const TOOL_AUTHOR_COLLISIONS: &[(&str, &str)] = &[
    ("type", "canary-type-4411"),
    ("id", "canary-id-4412"),
    ("role", "canary-role-4413"),
    ("subtype", "canary-subtype-4414"),
    ("model", "canary-model-4415"),
];

/// `TOOL_AUTHOR_COLLISIONS` as the object a tool author wrote.
fn tool_author_object() -> Value {
    Value::Object(
        TOOL_AUTHOR_COLLISIONS
            .iter()
            .map(|(name, value)| (String::from(*name), json!(value)))
            .collect(),
    )
}

/// Fails naming each collision value that is still in `line`, with the shape it was expected in.
fn assert_no_tool_author_value(line: &Value, where_: &str) {
    let written = line.to_string();
    let kept: Vec<&str> = TOOL_AUTHOR_COLLISIONS
        .iter()
        .map(|(_, value)| *value)
        .filter(|value| written.contains(value))
        .collect();
    assert!(
        kept.is_empty(),
        "expected every string {where_} replaced, structural names included | received kept: {kept:?} in {line}"
    );
}

#[test]
fn a_denied_tool_calls_input_replaces_strings_under_structural_parameter_names() {
    let line = redacted_record(&json!({"type": "result", "permission_denials": [
        {"tool_name": "mcp__ledger__lookup", "tool_use_id": "toolu_1",
         "tool_input": tool_author_object()}]}));

    assert_no_tool_author_value(&line, "under permission_denials[].tool_input");
    let input = &line["permission_denials"][0]["tool_input"];
    assert!(
        input["type"] == redact::PLACEHOLDER && input.as_object().is_some_and(|map| map.len() == 5),
        "expected the five parameter names kept with [REDACTED] values | received: {input}"
    );
    assert_eq!(
        line["permission_denials"][0]["tool_use_id"], "toolu_1",
        "expected the call's own tool_use_id to stay beside its input | received: {line}"
    );
}

#[test]
fn a_tool_input_inside_a_denial_replaces_a_nested_tool_use_lookalike_name() {
    let line = redacted_record(&json!({"type": "result", "permission_denials": [
        {"tool_input": {"payload": {"type": "tool_use", "name": "canary-name-4416"}}}]}));

    assert!(
        !line.to_string().contains("canary-name-4416"),
        "expected a parameter object that reads like a tool call to lose its name | received: {line}"
    );
}

#[test]
fn a_tool_result_replaces_strings_under_structural_field_names() {
    let line = redacted_record(&json!({"type": "user", "tool_use_result": tool_author_object()}));

    assert_no_tool_author_value(&line, "under tool_use_result");
}

#[test]
fn a_tool_result_array_replaces_strings_under_structural_field_names() {
    let line = redacted_record(&json!({"type": "user",
        "tool_use_result": [tool_author_object(), {"nested": [tool_author_object()]}]}));

    assert_no_tool_author_value(&line, "inside an array under tool_use_result");
}

/// Numbers only a tool author would write: an account number and an amount over a customer's data.
const TOOL_AUTHOR_ACCOUNT_NUMBER: u64 = 4_417_208_113;
const TOOL_AUTHOR_AMOUNT: f64 = 1234.56;

/// A tool author's parameters or result fields holding a number and a boolean at every place the
/// walk meets one: a member, an array entry and a nested object, plus a null.
fn tool_author_numbers_and_booleans() -> Value {
    json!({
        "customerNumber": TOOL_AUTHOR_ACCOUNT_NUMBER,
        "amount": TOOL_AUTHOR_AMOUNT,
        "overdrawn": true,
        "balances": [TOOL_AUTHOR_ACCOUNT_NUMBER, TOOL_AUTHOR_AMOUNT, true],
        "nested": {"retries": TOOL_AUTHOR_ACCOUNT_NUMBER, "approved": true, "absent": null},
    })
}

/// `tool_author_numbers_and_booleans` as the walk writes it: the same shape and types, every
/// number `0`, every boolean `false`, the null untouched.
fn tool_author_numbers_and_booleans_replaced() -> Value {
    json!({
        "customerNumber": 0,
        "amount": 0,
        "overdrawn": false,
        "balances": [0, 0, false],
        "nested": {"retries": 0, "approved": false, "absent": null},
    })
}

#[test]
fn a_denied_tool_calls_input_replaces_numbers_and_booleans_keeping_their_type() {
    let line = redacted_record(&json!({"type": "result", "permission_denials": [
        {"tool_name": "mcp__ledger__lookup", "tool_use_id": "toolu_1",
         "tool_input": tool_author_numbers_and_booleans()}]}));

    assert_eq!(
        line["permission_denials"][0]["tool_input"],
        tool_author_numbers_and_booleans_replaced(),
        "expected every number 0 and every boolean false under permission_denials[].tool_input | received: {line}"
    );
}

#[test]
fn a_tool_result_replaces_numbers_and_booleans_keeping_their_type() {
    let line = redacted_record(
        &json!({"type": "user", "tool_use_result": tool_author_numbers_and_booleans()}),
    );

    assert_eq!(
        line["tool_use_result"],
        tool_author_numbers_and_booleans_replaced(),
        "expected every number 0 and every boolean false under tool_use_result | received: {line}"
    );
}

#[test]
fn a_tool_result_that_is_a_bare_number_or_boolean_is_replaced() {
    let number =
        redacted_record(&json!({"type": "user", "tool_use_result": TOOL_AUTHOR_ACCOUNT_NUMBER}));
    let flag = redacted_record(&json!({"type": "user", "tool_use_result": true}));

    assert_eq!(
        (&number["tool_use_result"], &flag["tool_use_result"]),
        (&json!(0), &json!(false)),
        "expected a bare number replaced by 0 and a bare boolean by false | received: {number} and {flag}"
    );
}

#[test]
fn the_numbers_beside_a_tool_input_and_a_tool_result_are_still_kept() {
    let line = redacted_record(&json!({"type": "result", "num_turns": 3, "is_error": false,
        "usage": {"input_tokens": 11},
        "tool_use_result": {"customerNumber": TOOL_AUTHOR_ACCOUNT_NUMBER},
        "permission_denials": [{"tool_input": {"amount": TOOL_AUTHOR_AMOUNT}}]}));

    assert!(
        line["num_turns"] == 3 && line["is_error"] == false && line["usage"]["input_tokens"] == 11,
        "expected the turn's own numbers kept beside the tool data | received: {line}"
    );
}

#[test]
fn a_number_inside_the_models_answer_stays_even_under_a_tool_data_name() {
    let line = redacted_record(&json!({"type": "result", "structured_output": {
        "tool_input": {"count": 1, "ready": true}}}));

    assert_eq!(
        line["structured_output"]["tool_input"],
        json!({"count": 1, "ready": true}),
        "expected the answer's numbers and booleans kept, as the README says | received: {line}"
    );
}

/// A card number a tool author used as an object key, and what the walk writes for it.
const CARD_NUMBER_KEY: &str = "4111111111111111";
const CARD_NUMBER_KEY_REDACTED: &str = "0000000000000000";

/// A map keyed by figures, as a tool author's ledger could write it.
fn figure_keyed_map() -> Value {
    json!({CARD_NUMBER_KEY: true, "-12.50": 1, "007": "x", "2024-01-01": 2, "v2": 3})
}

#[test]
fn a_figure_used_as_a_key_under_tool_data_is_replaced_by_zeros() {
    let denied = redacted_record(&json!({"type": "result", "permission_denials": [
        {"tool_name": "mcp__ledger__lookup", "tool_input": {"balances": figure_keyed_map()}}]}));
    let result = redacted_record(&json!({"type": "user",
        "tool_use_result": {"balances": figure_keyed_map()}}));
    let call = redacted_records(&[assistant_record_with_calls(
        &json!({}),
        &json!({"balances": figure_keyed_map()}),
    )]);

    let expected = json!({CARD_NUMBER_KEY_REDACTED: false, "-00.00": 0, "000": "[REDACTED]",
        "0000-00-00": 0, "v2": 0});
    for (where_, balances) in [
        (
            "tool_input",
            &denied["permission_denials"][0]["tool_input"]["balances"],
        ),
        ("tool_use_result", &result["tool_use_result"]["balances"]),
        (
            "a call's input",
            &call[0]["message"]["content"][1]["input"]["balances"],
        ),
    ] {
        assert_eq!(
            balances, &expected,
            "expected a figure-shaped key under {where_} written as zeros, any other key as given | received: {balances}"
        );
    }
}

#[test]
fn figure_keys_that_collapse_to_the_same_zeros_keep_both_entries() {
    let line = redacted_record(&json!({"type": "user", "tool_use_result": {
        "4111": true, "5222": false, "0000": true}}));

    let result = line["tool_use_result"]
        .as_object()
        .expect("expected the result object");
    let mut keys: Vec<&str> = result.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["0000", "0000#2", "0000#3"],
        "expected three figure keys to stay three entries | received: {line}"
    );
    assert!(
        !line.to_string().contains("4111") && !line.to_string().contains("5222"),
        "expected no figure to survive as a key | received: {line}"
    );
}

#[test]
fn a_figure_used_as_a_key_in_the_models_answer_stays() {
    let records = redacted_records(&[
        assistant_record_with_calls(&json!({CARD_NUMBER_KEY: 1}), &json!({})),
        json!({"type": "result", "structured_output": {CARD_NUMBER_KEY: 1, "nested": {"42": true}},
            "wire_tool_inputs": {ANSWER_TOOL_USE_ID: {"7": 1}}}),
    ]);

    assert_eq!(
        records[0]["message"]["content"][0]["input"],
        json!({CARD_NUMBER_KEY: 1}),
        "expected the answer call's keys as the schema named them | received: {}",
        records[0]
    );
    assert_eq!(
        records[1]["structured_output"],
        json!({CARD_NUMBER_KEY: 1, "nested": {"42": true}}),
        "expected structured_output's keys as the schema named them | received: {}",
        records[1]
    );
    assert_eq!(
        records[1]["wire_tool_inputs"][ANSWER_TOOL_USE_ID],
        json!({"7": 1}),
        "expected the answer's repeat keyed as the schema named it | received: {}",
        records[1]
    );
}

#[test]
fn a_card_number_with_separators_or_a_date_used_as_a_key_is_zeroed_keeping_its_separators() {
    let line = redacted_record(&json!({"type": "user", "tool_use_result": {
        "4111-1111-1111-1111": true,
        "4111 1111 1111 1111": true,
        "2024-03-09": 1,
        "12:30:45": 2,
        "named": 3}}));

    let result = line["tool_use_result"]
        .as_object()
        .expect("expected the result object");
    let keys: std::collections::BTreeSet<&str> = result.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        std::collections::BTreeSet::from([
            "0000 0000 0000 0000",
            "0000-0000-0000-0000",
            "00:00:00",
            "0000-00-00",
            "named"
        ]),
        "expected each separated figure key written as zeros with its separators | received: {line}"
    );
    let written = line.to_string();
    assert!(
        ["4111", "2024", "12:30"]
            .iter()
            .all(|figure| !written.contains(figure)),
        "expected no digit of a card number, date or time to survive as a key | received: {line}"
    );
}

#[test]
fn separated_figure_keys_that_collapse_to_the_same_zeros_keep_both_entries() {
    let line = redacted_record(&json!({"type": "user", "tool_use_result": {
        "4111-1111": true, "5222-2222": false}}));

    let result = line["tool_use_result"]
        .as_object()
        .expect("expected the result object");
    let mut keys: Vec<&str> = result.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["0000-0000", "0000-0000#2"],
        "expected two dashed figure keys to stay two entries | received: {line}"
    );
}

#[test]
fn the_drift_report_lists_a_figure_key_as_the_transcript_writes_it() {
    let drift = drift_over(&lines_of(&[json!({"type": "user",
        "tool_use_result": {CARD_NUMBER_KEY: true}})]));

    assert!(
        drift.map_keys.contains(CARD_NUMBER_KEY_REDACTED)
            && !drift.map_keys.contains(CARD_NUMBER_KEY),
        "expected the key line to hold the written zeros and not the figure | received: {:?}",
        drift.map_keys
    );
}

#[test]
fn only_a_figure_shaped_text_is_a_figure_key() {
    let figures = [
        "0",
        "4111111111111111",
        "-12.50",
        "3.14",
        "4111-1111-1111-1111",
        "4111 1111 1111 1111",
        "2024-01-01",
        "12:30:45",
        "01/02/2024",
        "+1 (555) 010-0100",
        "1.",
        ".5",
        "1.2.3",
        "+1",
    ];
    let others = [
        "",
        "-",
        ".",
        " ",
        "()",
        "1e5",
        "v2",
        "id",
        "a1",
        "1a",
        "2024-01-01T10:00",
        "١٢",
        "inf",
        "NaN",
    ];
    for text in figures {
        assert!(is_figure_key(text), "expected {text:?} read as a figure");
    }
    for text in others {
        assert!(
            !is_figure_key(text),
            "expected {text:?} not read as a figure"
        );
    }
}

/// The answer's own tool call: its input is what `--json-schema` made the model write.
const ANSWER_TOOL_USE_ID: &str = "toolu_answer";
/// A tool author's call, recorded beside the answer's.
const LEDGER_TOOL_USE_ID: &str = "toolu_ledger";

/// The answer a `--json-schema` turn wrote: numbers and booleans a replay and a qualification read.
fn schema_answer() -> Value {
    json!({"count": 1, "ready": true, "nested": {"total": 2}})
}

/// An assistant record holding the answer's `StructuredOutput` call and a tool author's call, each
/// with `input` as given.
fn assistant_record_with_calls(answer_input: &Value, ledger_input: &Value) -> Value {
    json!({"type": "assistant", "message": {"content": [
        {"type": "tool_use", "id": ANSWER_TOOL_USE_ID, "name": "StructuredOutput",
         "input": answer_input},
        {"type": "tool_use", "id": LEDGER_TOOL_USE_ID, "name": "mcp__ledger__lookup",
         "input": ledger_input}]}})
}

/// A record repeating each call's input under `wire_tool_inputs`, keyed by tool-use id.
fn wire_inputs_record(answer_input: &Value, ledger_input: &Value) -> Value {
    json!({"type": "result", "wire_tool_inputs": {
        ANSWER_TOOL_USE_ID: answer_input, LEDGER_TOOL_USE_ID: ledger_input}})
}

#[test]
fn a_tool_authors_call_input_replaces_numbers_and_booleans_and_the_answers_keeps_them() {
    let records = redacted_records(&[assistant_record_with_calls(
        &schema_answer(),
        &tool_author_numbers_and_booleans(),
    )]);

    let [answer_call, ledger_call] = &records[0]["message"]["content"]
        .as_array()
        .expect("expected the content blocks")[..]
    else {
        panic!("expected two tool calls | received: {}", records[0]);
    };
    assert_eq!(
        answer_call["input"],
        schema_answer(),
        "expected the StructuredOutput call's answer intact | received: {answer_call}"
    );
    assert_eq!(
        ledger_call["input"],
        tool_author_numbers_and_booleans_replaced(),
        "expected every number 0 and every boolean false in another tool's input | received: {ledger_call}"
    );
}

/// A number only a tool author would write, inside a block that reads like the answer's call.
const LOOKALIKE_ACCOUNT: u64 = 123_456;

/// A `StructuredOutput` tool_use block as a tool author's data can spell it out.
fn answer_lookalike_block() -> Value {
    json!({"type": "tool_use", "id": ANSWER_TOOL_USE_ID, "name": "StructuredOutput",
        "input": {"acct": LOOKALIKE_ACCOUNT, "open": true}})
}

/// Fails unless `value` is the lookalike block's input with its number and boolean replaced.
fn assert_lookalike_input_replaced(value: &Value, where_: &str) {
    assert_eq!(
        value,
        &json!({"acct": 0, "open": false}),
        "expected a StructuredOutput lookalike {where_} to be replaced like any tool data | received: {value}"
    );
}

#[test]
fn an_answer_lookalike_inside_another_tools_input_does_not_keep_its_numbers() {
    let records = redacted_records(&[assistant_record_with_calls(
        &schema_answer(),
        &json!({"payload": answer_lookalike_block()}),
    )]);

    let ledger_call = &records[0]["message"]["content"][1];
    assert_lookalike_input_replaced(
        &ledger_call["input"]["payload"]["input"],
        "in another tool's input",
    );
    assert_eq!(
        records[0]["message"]["content"][0]["input"],
        schema_answer(),
        "expected the real answer call intact beside it | received: {}",
        records[0]
    );
}

#[test]
fn an_answer_lookalike_inside_a_tool_result_or_denied_input_does_not_keep_its_numbers() {
    let records = redacted_records(&[
        assistant_record_with_calls(&schema_answer(), &json!({})),
        json!({"type": "user", "tool_use_result": {"echo": answer_lookalike_block()}}),
        json!({"type": "result", "permission_denials": [
            {"tool_use_id": LEDGER_TOOL_USE_ID, "tool_input": {"echo": answer_lookalike_block()}}]}),
    ]);

    assert_lookalike_input_replaced(
        &records[1]["tool_use_result"]["echo"]["input"],
        "in tool_use_result",
    );
    assert_lookalike_input_replaced(
        &records[2]["permission_denials"][0]["tool_input"]["echo"]["input"],
        "in tool_input",
    );
}

#[test]
fn an_answer_id_used_as_a_key_inside_another_tools_input_does_not_keep_its_numbers() {
    let records = redacted_records(&[assistant_record_with_calls(
        &schema_answer(),
        &json!({
            "wire_tool_inputs": {ANSWER_TOOL_USE_ID: {"acct": LOOKALIKE_ACCOUNT, "open": true}}}),
    )]);

    assert_lookalike_input_replaced(
        &records[0]["message"]["content"][1]["input"]["wire_tool_inputs"][ANSWER_TOOL_USE_ID],
        "under a wire_tool_inputs key nested in another tool's input",
    );
}

#[test]
fn a_structured_output_member_inside_tool_data_does_not_keep_its_numbers() {
    let records = redacted_records(&[
        json!({"type": "user", "tool_use_result": {"structured_output": {"acct": LOOKALIKE_ACCOUNT, "open": true}}}),
    ]);

    assert_eq!(
        records[0]["tool_use_result"]["structured_output"],
        json!({"acct": 0, "open": false}),
        "expected a structured_output member a tool wrote to be tool data | received: {}",
        records[0]
    );
}

#[test]
fn an_answer_lookalike_inside_tool_data_names_no_answer_call() {
    let answers = AnswerCalls::in_lines(&lines_of(&[
        assistant_record_with_calls(
            &json!({}),
            &json!({"payload": {
            "type": "tool_use", "id": "toolu_nested_in_input", "name": "StructuredOutput"}}),
        ),
        json!({"type": "user", "tool_use_result": {"echo": {
            "type": "tool_use", "id": "toolu_nested_in_result", "name": "StructuredOutput"}}}),
        json!({"type": "result", "permission_denials": [{"tool_input": {"echo": {
            "type": "tool_use", "id": "toolu_nested_in_denial", "name": "StructuredOutput"}}}]}),
        json!({"type": "result", "wire_tool_inputs": {LEDGER_TOOL_USE_ID: {"echo": {
            "type": "tool_use", "id": "toolu_nested_in_wire", "name": "StructuredOutput"}}}}),
    ]));

    let claimed: Vec<&str> = [
        ANSWER_TOOL_USE_ID,
        "toolu_nested_in_input",
        "toolu_nested_in_result",
        "toolu_nested_in_denial",
        "toolu_nested_in_wire",
    ]
    .into_iter()
    .filter(|id| answers.contains(id))
    .collect();
    assert_eq!(
        claimed,
        vec![ANSWER_TOOL_USE_ID],
        "expected only the call in the message to be the answer's, none a tool's data spelled out"
    );
}

#[test]
fn an_mcp_tool_named_like_the_answer_tool_is_another_tools_call() {
    let records = redacted_records(&[json!({"type": "assistant", "message": {"content": [
        {"type": "tool_use", "id": "toolu_mcp", "name": "mcp__x__StructuredOutput",
         "input": {"count": 1, "ready": true}}]}})]);

    assert_eq!(
        records[0]["message"]["content"][0]["input"],
        json!({"count": 0, "ready": false}),
        "expected only the exact StructuredOutput name to keep an input's numbers | received: {}",
        records[0]
    );
}

#[test]
fn a_tool_authors_call_input_replaces_strings_under_structural_parameter_names() {
    let records = redacted_records(&[assistant_record_with_calls(
        &json!({}),
        &tool_author_object(),
    )]);

    assert_no_tool_author_value(&records[0], "under another tool's input");
}

#[test]
fn wire_tool_inputs_replaces_numbers_of_a_tool_authors_call_and_keeps_the_answers() {
    // The answer's call is named once in the stream; the repeat may come before or after it.
    for records in [
        vec![
            assistant_record_with_calls(&json!({}), &json!({})),
            wire_inputs_record(&schema_answer(), &tool_author_numbers_and_booleans()),
        ],
        vec![
            wire_inputs_record(&schema_answer(), &tool_author_numbers_and_booleans()),
            assistant_record_with_calls(&json!({}), &json!({})),
        ],
    ] {
        let wire_position = records
            .iter()
            .position(|record| record.get("wire_tool_inputs").is_some())
            .expect("expected a wire_tool_inputs record");
        let redacted = redacted_records(&records);

        let wire = &redacted[wire_position]["wire_tool_inputs"];
        assert_eq!(
            wire[ANSWER_TOOL_USE_ID],
            schema_answer(),
            "expected the answer's repeat intact | received: {wire}"
        );
        assert_eq!(
            wire[LEDGER_TOOL_USE_ID],
            tool_author_numbers_and_booleans_replaced(),
            "expected the tool author's repeat replaced, keyed by the same id | received: {wire}"
        );
    }
}

#[test]
fn a_wire_input_of_an_unknown_call_is_replaced_like_a_tool_authors() {
    let records = redacted_records(&[wire_inputs_record(
        &schema_answer(),
        &tool_author_numbers_and_booleans(),
    )]);

    let wire = &records[0]["wire_tool_inputs"];
    assert_eq!(
        wire[ANSWER_TOOL_USE_ID],
        json!({"count": 0, "ready": false, "nested": {"total": 0}}),
        "expected an input no StructuredOutput call claims to be replaced | received: {wire}"
    );
}

#[test]
fn the_names_of_a_tool_authors_input_are_keys_and_not_retained_members() {
    let drift = drift_over(&lines_of(&[
        assistant_record_with_calls(&schema_answer(), &json!({"novelCounter": 3})),
        wire_inputs_record(&schema_answer(), &json!({"novelCounter": 3})),
    ]));

    assert!(
        drift.retained.is_empty() && drift.replaced_scalars.is_empty() && drift.unseen.is_empty(),
        "expected the tool author's parameter on the key line only | received retained: {:?} replaced: {:?} strings: {:?}",
        drift.retained,
        drift.replaced_scalars,
        drift.unseen
    );
    assert!(
        drift.map_keys.contains("novelCounter") && !drift.map_keys.contains(LEDGER_TOOL_USE_ID),
        "expected the parameter listed and the tool-use id not | received keys: {:?}",
        drift.map_keys
    );
}

#[test]
fn the_answer_calls_are_the_structured_output_tool_uses_at_any_depth() {
    let answers = AnswerCalls::in_lines(&[
        lines_of(&[json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "id": "toolu_message", "name": "StructuredOutput", "input": {}},
            {"type": "tool_use", "id": "toolu_other", "name": "mcp__ledger__lookup", "input": {}},
            {"type": "text", "id": "toolu_text", "name": "StructuredOutput"}]}})])
        .remove(0),
        lines_of(
            &[json!({"type": "stream_event", "event": {"content_block": {
            "type": "tool_use", "id": "toolu_stream", "name": "StructuredOutput"}}})],
        )
        .remove(0),
        String::from("not json: toolu_text"),
        lines_of(&[json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "name": "StructuredOutput"}]}})])
        .remove(0),
    ]);

    let found: Vec<&str> = ["toolu_message", "toolu_stream", "toolu_other", "toolu_text"]
        .into_iter()
        .filter(|id| answers.contains(id))
        .collect();
    assert_eq!(
        found,
        vec!["toolu_message", "toolu_stream"],
        "expected only the ids of StructuredOutput tool_use blocks, a call with no id adding none"
    );
}

#[test]
fn every_member_the_walk_reads_as_a_tool_calls_arguments_is_declared() {
    assert_eq!(
        [CALL_INPUT_MEMBER, WIRE_INPUTS_MEMBER]
            .iter()
            .map(|member| child_keys_of(member))
            .collect::<Vec<_>>(),
        vec![Some(ChildKeys::UserChosen), Some(ChildKeys::ToolUseIds)],
        "expected input declared UserChosen and wire_tool_inputs declared ToolUseIds in REVIEWED_RETAINED_MEMBERS"
    );
}

/// A numeric and a boolean member no review lists, nested one level below a direct key.
const NOVEL_NESTED_NUMBER_MEMBER: &str = "novelNestedCounter";
const NOVEL_NESTED_BOOLEAN_MEMBER: &str = "novelNestedFlag";
/// The value of [`NOVEL_NESTED_NUMBER_MEMBER`]; it must never reach a notice.
const NOVEL_NESTED_NUMBER_VALUE: u64 = 7_305_118;

fn tool_data_with_novel_nested_scalars() -> Value {
    json!({"wrapper": {
        NOVEL_NESTED_NUMBER_MEMBER: NOVEL_NESTED_NUMBER_VALUE,
        NOVEL_NESTED_BOOLEAN_MEMBER: true,
        "numLines": 4,
        "absent": null,
    }})
}

#[test]
fn a_direct_tool_data_number_is_listed_as_a_key_and_not_as_a_replaced_member() {
    let drift = drift_over(&lines_of(&[
        json!({"type": "user", "tool_use_result": {"novelCounter": 3, "novelFlag": true}}),
    ]));

    assert!(
        drift.retained.is_empty()
            && drift.unseen.is_empty()
            && drift.denied.is_empty()
            && drift.replaced_scalars.is_empty()
            && drift.map_keys.len() == 2,
        "expected a direct key on the key line only | received retained: {:?} strings: {:?} denied: {:?} replaced: {:?} keys: {:?}",
        drift.retained,
        drift.unseen,
        drift.denied,
        drift.replaced_scalars,
        drift.map_keys
    );
}

#[test]
fn a_number_or_boolean_replaced_below_a_direct_tool_data_key_is_reported_by_name() {
    let drift = drift_over(&lines_of(&[
        json!({"type": "user", "tool_use_result": tool_data_with_novel_nested_scalars()}),
        json!({"type": "result", "permission_denials": [
            {"tool_input": tool_data_with_novel_nested_scalars()}]}),
    ]));

    assert_eq!(
        drift.replaced_scalars.iter().collect::<Vec<_>>(),
        vec![NOVEL_NESTED_NUMBER_MEMBER, NOVEL_NESTED_BOOLEAN_MEMBER],
        "expected the replaced numeric and boolean member no review lists, and not `numLines` or the null | received replaced: {:?}",
        drift.replaced_scalars
    );
}

#[test]
fn a_replaced_tool_data_scalar_is_named_on_a_notice_and_never_by_value() {
    // A built-in result as the vendor shapes it, with a numeric field no review lists.
    let drift = drift_over(&lines_of(&[json!({"type": "user", "tool_use_result": {
        "file": {"numLines": 4, NOVEL_NESTED_NUMBER_MEMBER: NOVEL_NESTED_NUMBER_VALUE}}})]));

    let notices = drift.lines(STRUCTURED_OUTPUT);
    assert!(
        notices.iter().any(|notice| notice
            .contains("members not seen before, their numbers and booleans replaced (1)")
            && notice.contains(NOVEL_NESTED_NUMBER_MEMBER)),
        "expected the member name on the replaced line | received: {notices:?}"
    );
    assert_no_value_in(&notices, &[&NOVEL_NESTED_NUMBER_VALUE.to_string()]);
}

/// An ARN that names an account; the transcript writes its key as `[REDACTED]`.
const MODEL_KEY_NAMING_AN_ACCOUNT: &str =
    "arn:aws:bedrock:us-east-1:111122223333:inference-profile/team-ledger";

#[test]
fn a_number_under_a_model_keyed_map_inside_tool_data_is_not_reported_by_its_raw_key() {
    let drift = drift_over(&lines_of(&[json!({"type": "user", "tool_use_result": {
        "wrapper": {"modelUsage": {MODEL_KEY_NAMING_AN_ACCOUNT: 5}}}})]));

    let notices = drift.lines(STRUCTURED_OUTPUT);
    assert!(
        !drift.replaced_scalars.contains(MODEL_KEY_NAMING_AN_ACCOUNT),
        "expected a model id key to stay out of the replaced line, as the transcript redacts it | received: {:?}",
        drift.replaced_scalars
    );
    assert_no_value_in(&notices, &["111122223333"]);
}

#[test]
fn a_number_inside_the_models_answer_is_not_a_replaced_tool_data_member() {
    let drift = drift_over(&lines_of(&[
        json!({"type": "result", "structured_output": {"tool_input": tool_data_with_novel_nested_scalars()}}),
    ]));

    assert!(
        drift.replaced_scalars.is_empty(),
        "expected the answer's own numbers left out of the replaced line | received: {:?}",
        drift.replaced_scalars
    );
}

#[test]
fn a_structural_name_replaced_inside_tool_data_is_not_reported_as_a_new_member() {
    let drift = drift_over(&lines_of(&[
        json!({"type": "user", "tool_use_result": tool_author_object()}),
        json!({"type": "result", "permission_denials": [
            {"tool_input": {"payload": tool_author_object(), TOOL_PARAMETER_KEY: "x"}}]}),
    ]));

    assert!(
        drift.unseen.is_empty(),
        "expected no structural name reported as unseen | received: {:?}",
        drift.unseen
    );
}

#[test]
fn a_string_named_outside_every_list_inside_tool_data_is_still_reported() {
    let drift = drift_over(&lines_of(&[
        json!({"type": "user", "tool_use_result": {"wrapper": {"novelNote": "x", "type": "y"}}}),
    ]));

    assert_eq!(
        drift.unseen.iter().collect::<Vec<_>>(),
        vec!["novelNote"],
        "expected only the member no review lists to be reported"
    );
}

#[test]
fn the_structural_members_beside_a_tool_input_and_a_tool_result_keep_their_strings() {
    let line = redacted_record(
        &json!({"type": "user", "role": "user", "session_id": SESSION_ID,
        "tool_use_result": {"type": "text"},
        "permission_denials": [{"tool_use_id": "toolu_1", "tool_input": {"type": "x"}}]}),
    );

    assert_eq!(
        (
            line["type"].as_str(),
            line["role"].as_str(),
            line["session_id"].as_str()
        ),
        (Some("user"), Some("user"), Some(SESSION_ID)),
        "expected the record's own structural strings kept | received: {line}"
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

/// Model ids that carry an AWS account, a Google Cloud project or a name the user chose, each with
/// the private part that must not reach a transcript.
const ACCOUNT_SHAPED_MODEL_IDS: &[(&str, &str, &str)] = &[
    (
        "a Bedrock inference-profile ARN",
        "arn:aws:bedrock:us-east-1:123456789012:inference-profile/us.anthropic.claude-sonnet-4-5-20250929-v1:0",
        "123456789012",
    ),
    (
        "a Bedrock application inference-profile ARN",
        "arn:aws:bedrock:eu-west-1:210987654321:application-inference-profile/k3j9x0q2w1ab",
        "210987654321",
    ),
    (
        "a Vertex project path",
        "projects/acme-payments-4821/locations/us-east5/publishers/anthropic/models/claude-sonnet-4-5@20250929",
        "acme-payments-4821",
    ),
    (
        "an inference-profile name the user chose",
        "acme-payments-claude-profile",
        "acme-payments",
    ),
];

/// Model ids a vendor assigns, which describe the build and nobody's account.
const PLAIN_VENDOR_MODEL_IDS: &[&str] = &[
    "claude-sonnet-5",
    "claude-haiku-4-5-20251001",
    "claude-3-5-sonnet-20241022",
    "claude-opus-4-1@20250805",
    "claude-sonnet-4-5-20250929[1m]",
    "anthropic.claude-opus-5-v1:0",
    "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
    // The publisher form `model_accepted` takes: it names no project and no location.
    "publishers/anthropic/models/claude-opus-5",
    "publishers/anthropic/models/claude-opus-4-1@20250805",
    "opus",
    "sonnet[1m]",
];

/// Publisher paths that are not exactly `publishers/anthropic/models/<plain id>`, each with the
/// segment that makes it private or unreviewed.
const PUBLISHER_PATHS_THAT_STAY_REDACTED: &[(&str, &str)] = &[
    (
        "a location segment",
        "locations/us-east5/publishers/anthropic/models/claude-opus-5",
    ),
    (
        "a project segment",
        "projects/acme-payments-4821/publishers/anthropic/models/claude-opus-5",
    ),
    (
        "a project and a location",
        "projects/acme/locations/us-east5/publishers/anthropic/models/claude-opus-5",
    ),
    (
        "a segment before the publisher",
        "acme/publishers/anthropic/models/claude-opus-5",
    ),
    (
        "an extra segment after the model",
        "publishers/anthropic/models/claude-opus-5/versions/1",
    ),
    (
        "a trailing slash",
        "publishers/anthropic/models/claude-opus-5/",
    ),
    (
        "a non-anthropic publisher",
        "publishers/acme/models/claude-opus-5",
    ),
    (
        "a non-claude model id",
        "publishers/anthropic/models/acme-payments-claude-profile",
    ),
    (
        "an alias in place of a model id",
        "publishers/anthropic/models/opus",
    ),
    ("no model id", "publishers/anthropic/models/"),
    (
        "a doubled prefix",
        "publishers/anthropic/models/publishers/anthropic/models/claude-opus-5",
    ),
    (
        "an upper-case publisher",
        "publishers/Anthropic/models/claude-opus-5",
    ),
];

#[test]
fn a_model_member_is_redacted_when_the_id_carries_an_account_or_a_user_chosen_name() {
    for (shape, id, private) in ACCOUNT_SHAPED_MODEL_IDS {
        let line = redacted_record(&json!({"type": "assistant", "message": {"model": id}}));
        assert!(
            !line.to_string().contains(private),
            "expected `model` to hold no {private:?} for {shape} | received: {line}"
        );
        assert_eq!(
            line["message"]["model"],
            redact::PLACEHOLDER,
            "expected {shape} ({id:?}) to become {} | received: {line}",
            redact::PLACEHOLDER
        );
    }
}

#[test]
fn a_plain_vendor_model_id_is_kept_in_the_model_member_and_the_usage_keys() {
    for id in PLAIN_VENDOR_MODEL_IDS {
        let line = redacted_record(&json!({"type": "result", "model": id,
            "modelUsage": {*id: {"costUSD": 0.01}},
            "model_usage_since_last_result": {*id: {"costUSD": 0.01}}}));
        assert_eq!(
            line["model"], *id,
            "expected the plain vendor id to be kept | received: {line}"
        );
        for member in ["modelUsage", "model_usage_since_last_result"] {
            assert_eq!(
                line[member][*id]["costUSD"], 0.01,
                "expected {id:?} kept as a key of {member} | received: {line}"
            );
        }
    }
}

#[test]
fn a_publisher_path_with_more_than_the_plain_model_id_is_redacted_in_every_model_slot() {
    for (shape, id) in PUBLISHER_PATHS_THAT_STAY_REDACTED {
        assert!(
            !is_plain_vendor_model_id(id),
            "expected {id:?} with {shape} to be redacted | received: kept"
        );
        let line = redacted_record(&json!({"type": "result", "model": id,
            "modelUsage": {*id: {"costUSD": 0.01}}}));
        assert_eq!(
            line["model"],
            redact::PLACEHOLDER,
            "expected `model` to become {} for {shape} | received: {line}",
            redact::PLACEHOLDER
        );
        assert_eq!(
            line["modelUsage"][redact::PLACEHOLDER]["costUSD"],
            0.01,
            "expected the usage key to become {} for {shape} | received: {line}",
            redact::PLACEHOLDER
        );
    }
}

#[test]
fn a_model_usage_key_that_carries_an_account_is_redacted_and_its_counters_survive() {
    for member in ["modelUsage", "model_usage_since_last_result"] {
        for (shape, id, private) in ACCOUNT_SHAPED_MODEL_IDS {
            let line = redacted_record(&json!({"type": "result",
                member: {*id: {"inputTokens": 7, "costUSD": 0.5}}}));
            assert!(
                !line.to_string().contains(private),
                "expected the keys of {member} to hold no {private:?} for {shape} | received: {line}"
            );
            assert_eq!(
                line[member][redact::PLACEHOLDER]["inputTokens"],
                7,
                "expected one redacted key carrying the counters of {shape} in {member} | received: {line}"
            );
        }
    }
}

#[test]
fn two_redacted_model_usage_keys_stay_two_entries() {
    let line = redacted_record(&json!({"type": "result", "modelUsage": {
        ACCOUNT_SHAPED_MODEL_IDS[0].1: {"inputTokens": 1},
        ACCOUNT_SHAPED_MODEL_IDS[2].1: {"inputTokens": 2},
        "claude-sonnet-5": {"inputTokens": 4}}}));

    let usage = line["modelUsage"].as_object().expect("expected a map");
    let mut counts: Vec<i64> = usage
        .values()
        .filter_map(|entry| entry["inputTokens"].as_i64())
        .collect();
    counts.sort_unstable();
    assert_eq!(
        counts,
        [1, 2, 4],
        "expected a counter per model, none lost to a key collision | received: {line}"
    );
}

#[test]
fn which_model_ids_are_plain_vendor_names() {
    for id in PLAIN_VENDOR_MODEL_IDS {
        assert!(
            is_plain_vendor_model_id(id),
            "expected {id:?} to be a plain vendor id"
        );
    }
    for id in ACCOUNT_SHAPED_MODEL_IDS
        .iter()
        .map(|(_, id, _)| id)
        .chain(&[
            // A family this list has not reviewed, a 12-digit account id and a bare Bedrock prefix.
            "claude-acme-prod",
            "claude-123456789012",
            "us.anthropic.acme-claude",
            "anthropic.claude-opus-5-v1:0:extra",
            "claude-opus-5\nacme",
            "",
        ])
    {
        assert!(
            !is_plain_vendor_model_id(id),
            "expected {id:?} to be user- or account-shaped"
        );
    }
}

#[test]
fn a_redacted_model_id_keeps_the_placeholder_and_a_plain_one_is_returned_whole() {
    assert_eq!(redact_model_id("claude-sonnet-5"), "claude-sonnet-5");
    assert_eq!(
        redact_model_id(ACCOUNT_SHAPED_MODEL_IDS[0].1),
        redact::PLACEHOLDER
    );
}

#[test]
fn a_key_taken_in_the_map_gets_the_next_free_ordinal() {
    let mut map = Map::new();
    assert_eq!(unused_key(&map, String::from("[REDACTED]")), "[REDACTED]");
    map.insert(String::from("[REDACTED]"), Value::Null);
    assert_eq!(unused_key(&map, String::from("[REDACTED]")), "[REDACTED]#2");
    map.insert(String::from("[REDACTED]#2"), Value::Null);
    assert_eq!(unused_key(&map, String::from("[REDACTED]")), "[REDACTED]#3");
}

#[test]
fn an_answer_property_named_model_is_redacted_whatever_its_value() {
    let line = redacted_record(&json!({"type": "assistant",
        "wire_tool_inputs": {"toolu_1": {"model": "claude-sonnet-5"}}}));
    assert_eq!(
        line["wire_tool_inputs"]["toolu_1"]["model"],
        redact::PLACEHOLDER,
        "expected a model-named property of the answer to stay the model's text | received: {line}"
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
        redact::PLACEHOLDER,
        "expected the tool list, which names the user's servers, to be replaced whole"
    );
    assert_eq!(init["mcp_servers"], redact::PLACEHOLDER);
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
    // Not an init record: that one replaces `paths` and `memory_paths` whole, and this test is
    // about the rewrite of the members a record keeps.
    let line = json!({
        "type": "system", "subtype": "status",
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
fn a_schema_capture_notice_says_the_answer_is_kept_verbatim_and_names_the_scenario() {
    assert_eq!(
        verbatim_answer_notice(STRUCTURED_OUTPUT),
        "claude/structured-output: the model's answer (key names and numbers) is kept verbatim in the transcript; the schema and prompt must elicit no real data. Review the file before committing"
    );
}

#[test]
fn a_schema_capture_notice_is_one_line() {
    let notice = verbatim_answer_notice(STRUCTURED_OUTPUT);
    assert!(
        !notice.contains('\n') && !notice.is_empty(),
        "expected a nonempty single-line notice | received: {notice:?}"
    );
}

#[test]
fn a_built_in_scenario_elicits_no_figure_but_the_placeholder_one() {
    for scenario in SCENARIOS {
        let digit_runs: Vec<&str> = scenario
            .prompt
            .split(|character: char| !character.is_ascii_digit())
            .filter(|run| !run.is_empty())
            .collect();
        assert!(
            digit_runs.iter().all(|run| *run == "1"),
            "(claude/{}): expected a prompt whose only figure is the placeholder 1 | received digit runs: {digit_runs:?} in {:?}",
            scenario.name,
            scenario.prompt
        );
        assert!(
            scenario.prompt.contains("every number with 1")
                && scenario.prompt.contains("every string with the word mango"),
            "(claude/{}): expected a prompt that fills every string and number with a placeholder | received: {:?}",
            scenario.name,
            scenario.prompt
        );
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
fn a_header_model_line_redacts_an_id_that_carries_an_account_and_keeps_a_plain_one() {
    for (shape, id, private) in ACCOUNT_SHAPED_MODEL_IDS {
        let lines = header(&SCENARIOS[0], None, None, Some(id));
        assert!(
            !lines.join("\n").contains(private),
            "expected no {private:?} in the header for {shape} | received: {lines:?}"
        );
        assert_eq!(
            lines[1],
            format!("# model: {}", redact::PLACEHOLDER),
            "expected {shape} ({id:?}) to be redacted on the model line | received: {lines:?}"
        );
    }
    for id in PLAIN_VENDOR_MODEL_IDS {
        let lines = header(&SCENARIOS[0], None, None, Some(id));
        assert_eq!(
            lines[1],
            format!("# model: {id}"),
            "expected the plain vendor id to be kept | received: {lines:?}"
        );
    }
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
        CANARY_EXPIRES_AT_TEXT.to_owned(),
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
        &RecordedNotices::default(),
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

#[tokio::test]
async fn a_schema_capture_hands_the_verbatim_answer_notice_to_the_operator_once() {
    let facts = MachineFacts::new();
    let out = tempfile::tempdir().expect("expected an output directory");
    let notices = RecordedNotices::default();

    capture_noticed(
        scripted_cli(answered_turn_lines(&facts)),
        out.path(),
        &facts,
        Some(SCHEMA),
        None,
        &notices,
    )
    .await
    .unwrap_or_else(|error| panic!("expected the capture to qualify, received {error}"));

    let expected = verbatim_answer_notice(STRUCTURED_OUTPUT);
    let received: Vec<String> = notices
        .recorded()
        .into_iter()
        .filter(|notice| notice.contains("kept verbatim"))
        .collect();
    assert_eq!(
        received,
        vec![expected],
        "expected the verbatim-answer notice exactly once for a schema turn"
    );
}

#[tokio::test]
async fn the_verbatim_answer_notice_reaches_the_operator_before_a_refusal_ends_the_capture() {
    let facts = MachineFacts::new();
    let out = tempfile::tempdir().expect("expected an output directory");
    let cli = Arc::new(
        FakeClaudeCli::new()
            .with_help(HELP_WITH_JSON_SCHEMA)
            .with_auth(SIGNED_OUT),
    );
    let notices = RecordedNotices::default();

    capture_noticed(cli, out.path(), &facts, Some(SCHEMA), None, &notices)
        .await
        .expect_err("expected a signed-out CLI to be refused");

    assert_eq!(
        notices.recorded(),
        vec![verbatim_answer_notice(STRUCTURED_OUTPUT)],
        "expected the notice the operator reads before committing, even for a refused capture"
    );
}

#[tokio::test]
async fn a_capture_refused_for_want_of_a_schema_says_nothing_about_a_verbatim_answer() {
    let facts = MachineFacts::new();
    let out = tempfile::tempdir().expect("expected an output directory");
    let notices = RecordedNotices::default();

    capture_noticed(
        Arc::new(FakeLauncher::new()),
        out.path(),
        &facts,
        None,
        None,
        &notices,
    )
    .await
    .expect_err("expected a missing schema to be refused");

    assert_eq!(
        notices.recorded(),
        Vec::<String>::new(),
        "expected no verbatim-answer notice for a capture that runs under no schema"
    );
}

/// A CLI whose turn launch fails with a vendor refusal and leaves behind a child that cannot be
/// stopped, so the capture's cleanup fails after its conversation did. Probes run as on the shared
/// fake.
struct RefusedTurnWithUnstoppableChildCli {
    cli: Arc<FakeClaudeCli>,
}

#[async_trait::async_trait]
impl ProcessLauncher for RefusedTurnWithUnstoppableChildCli {
    async fn spawn(&self, spec: LaunchSpec) -> Result<ManagedProcess> {
        let turn = is_turn_launch(&spec.argv);
        let process = self.cli.spawn(spec).await?;
        if !turn {
            return Ok(process);
        }
        Err(Error::CleanupRequired {
            control: Arc::new(UnstoppableControl {
                inner: process.control,
            }),
            source: Box::new(FakeVendorFailure::error()),
        })
    }
}

#[tokio::test]
async fn a_failed_turn_and_a_failed_cleanup_hand_the_vendor_message_to_the_operator() {
    let facts = MachineFacts::new();
    let out = tempfile::tempdir().expect("expected an output directory");
    let cli = Arc::new(RefusedTurnWithUnstoppableChildCli {
        cli: scripted_cli(vec![String::from(r#"{"type":"result","is_error":false}"#)]),
    });
    let notices = RecordedNotices::default();

    let error = capture_noticed(cli, out.path(), &facts, Some(SCHEMA), None, &notices)
        .await
        .expect_err("expected a refused turn with an unstoppable child to fail the capture");

    assert!(
        error.cleanup_control().is_some(),
        "expected the capture to fail with its process cleanup still owed | received: {error:?}"
    );
    let primary: Vec<String> = notices
        .recorded()
        .into_iter()
        .filter(|notice| notice.contains("capture failed before cleanup"))
        .collect();
    assert_eq!(
        primary.len(),
        1,
        "expected one primary-failure line | received: {:?}",
        notices.recorded()
    );
    assert!(
        primary[0].starts_with("claude/structured-output: capture failed before cleanup: ")
            && primary[0].contains(FakeVendorFailure::CODE)
            && primary[0].contains(FakeVendorFailure::MESSAGE),
        "expected the scenario, then the vendor code and message of the failed turn | received: {:?}",
        primary[0]
    );
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

// ---------------------------------------------------------------------------------------------
// The drift report
// ---------------------------------------------------------------------------------------------

/// A member of an ordinary record that no review has seen. The vendor names it, so the name is
/// what a maintainer needs; the value is what is private.
const NOVEL_STRING_MEMBER: &str = "novelVendorNote";
/// A value for [`NOVEL_STRING_MEMBER`]; it must never reach a notice.
const NOVEL_STRING_VALUE: &str = "canary-novel-value-6204";
/// A `system/init` member outside the allow-list, with a value that must never reach a notice.
const NOVEL_INIT_MEMBER: &str = "novelInitSetting";
const NOVEL_INIT_VALUE: &str = "canary-init-value-6205";

/// The pinned stream with a novel string member, a novel init member and a novel rate-limit member
/// added, each carrying a canary value.
fn drifted_turn_lines(facts: &MachineFacts) -> Vec<String> {
    let mut lines = answered_turn_lines(facts);
    let mut init: Value = serde_json::from_str(&lines[0]).expect("expected the init line");
    init[NOVEL_INIT_MEMBER] = json!(NOVEL_INIT_VALUE);
    lines[0] = init.to_string();
    lines.insert(
        2,
        json!({"type": "assistant", "session_id": SESSION_ID,
            NOVEL_STRING_MEMBER: NOVEL_STRING_VALUE,
            "message": {"id": "msg_0", "role": "assistant", "content": []}})
        .to_string(),
    );
    lines
}

/// Runs the walk over `lines` as a capture does and returns the drift it collected.
fn drift_over(lines: &[String]) -> DriftReport {
    let facts = MachineFacts::new();
    let answers = AnswerCalls::in_lines(lines);
    let mut drift = DriftReport::default();
    for line in lines {
        super::redact_line(line, &answers, facts.paths(), &mut drift);
    }
    drift
}

fn assert_no_value_in(notices: &[String], values: &[&str]) {
    for value in values {
        assert!(
            notices.iter().all(|notice| !notice.contains(value)),
            "expected no value in a drift notice | value: {value:?} | received: {notices:?}"
        );
    }
}

#[tokio::test]
async fn a_capture_names_the_novel_members_on_its_notices_and_never_their_values() {
    let facts = MachineFacts::new();
    let out = tempfile::tempdir().expect("expected an output directory");
    let cli = scripted_cli(drifted_turn_lines(&facts));
    let notices = RecordedNotices::default();

    capture_noticed(cli, out.path(), &facts, Some(SCHEMA), None, &notices)
        .await
        .unwrap_or_else(|error| panic!("expected the capture to qualify, received {error}"));

    let recorded = notices.recorded();
    for name in [NOVEL_STRING_MEMBER, NOVEL_INIT_MEMBER, FUTURE_MEMBER] {
        assert!(
            recorded.iter().any(|notice| notice.contains(name)),
            "expected the member name in a drift notice | name: {name:?} | received: {recorded:?}"
        );
    }
    assert_no_value_in(
        &recorded,
        &[
            NOVEL_STRING_VALUE,
            NOVEL_INIT_VALUE,
            CANARY_TEXT,
            &CANARY_RESET_AT.to_string(),
            &(CANARY_NUMBER + 1).to_string(),
        ],
    );
    let written = read_transcript(out.path(), "structured-output.jsonl");
    assert!(
        !written.contains(NOVEL_STRING_VALUE) && !written.contains(NOVEL_INIT_VALUE),
        "expected the transcript to hold no novel value | received: {written}"
    );
}

#[tokio::test]
async fn an_unqualified_capture_still_reports_its_drift_after_writing_the_file() {
    let facts = MachineFacts::new();
    let out = tempfile::tempdir().expect("expected an output directory");
    let plain_answer = lines_of(&[
        json!({"type": "system", "subtype": "init", "cwd": facts.cwd,
               "session_id": SESSION_ID, NOVEL_INIT_MEMBER: NOVEL_INIT_VALUE}),
        json!({"type": "result", "subtype": "success", "is_error": false, "result": "mango"}),
    ]);
    let notices = RecordedNotices::default();

    capture_noticed(
        scripted_cli(plain_answer),
        out.path(),
        &facts,
        Some(SCHEMA),
        None,
        &notices,
    )
    .await
    .expect_err("expected a turn without structured output to fail the capture");

    let recorded = notices.recorded();
    assert!(
        recorded
            .iter()
            .any(|notice| notice.contains(NOVEL_INIT_MEMBER)),
        "expected the init member name in a drift notice | received: {recorded:?}"
    );
    assert_no_value_in(&recorded, &[NOVEL_INIT_VALUE]);
    assert!(
        transcript_dir(out.path())
            .join("structured-output.unqualified.jsonl")
            .exists(),
        "expected the unqualified file to be written before the notice"
    );
}

#[test]
fn the_pinned_stream_reports_no_member_this_build_has_not_reviewed() {
    let facts = MachineFacts::new();
    let drift = drift_over(&answered_turn_lines(&facts));

    assert!(
        drift.unseen.is_empty() && drift.retained.is_empty(),
        "expected no unseen member in the pinned stream | received strings: {:?} retained: {:?}",
        drift.unseen,
        drift.retained
    );
}

#[test]
fn a_denied_member_is_reported_unless_a_capture_already_showed_it() {
    let facts = MachineFacts::new();
    let mut lines = answered_turn_lines(&facts);
    lines.push(
        json!({"type": "result", "subagent_stats": {
            "by_type": {CANARY_AGENT_TYPE: {"count": 2}}}})
        .to_string(),
    );

    let drift = drift_over(&lines);

    // The init configuration, `rate_limit_info` and `by_type` are denied on every run; only the
    // member this build never had is news.
    let denied: Vec<&str> = drift.denied.iter().map(String::as_str).collect();
    assert_eq!(
        denied,
        vec![FUTURE_MEMBER],
        "expected only the member outside REVIEWED_DENIED_MEMBERS {REVIEWED_DENIED_MEMBERS:?} | received: {denied:?}"
    );
}

#[test]
fn the_reviewed_denied_members_are_sorted() {
    let mut sorted = REVIEWED_DENIED_MEMBERS.to_vec();
    sorted.sort_unstable();
    assert_eq!(
        REVIEWED_DENIED_MEMBERS,
        sorted.as_slice(),
        "expected the reviewed list sorted"
    );
}

#[test]
fn a_member_a_name_rule_hides_and_a_schema_property_are_not_drift() {
    let facts = MachineFacts::new();
    let drift = drift_over(&lines_of(&[
        json!({"type": "result", "credentialHint": CANARY_TEXT, "email": "someone@example.com"}),
        json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "name": "StructuredOutput",
             "input": {"schemaProperty": "mango", "by_type": "mango"}}]}}),
        json!({"type": "system", "subtype": "init", "session_id": SESSION_ID,
               "cwd": facts.cwd, NOVEL_INIT_MEMBER: null}),
        json!({"type": "result", NOVEL_NUMBER_MEMBER: null}),
    ]));

    assert!(
        drift.denied.is_empty() && drift.unseen.is_empty() && drift.retained.is_empty(),
        "expected a name-rule member, an answer property and a null to add no name | received \
         denied: {:?} unseen: {:?} retained: {:?}",
        drift.denied,
        drift.unseen,
        drift.retained
    );
}

#[test]
fn a_report_counts_distinct_names_once_and_shows_the_first_few_sorted() {
    let mut drift = DriftReport::default();
    for _ in 0..3 {
        for ordinal in (0..DRIFT_NAMES_SHOWN + 5).rev() {
            drift.note_unseen(&format!("member{ordinal:03}"));
        }
    }

    let lines = drift.lines(STRUCTURED_OUTPUT);

    assert_eq!(
        lines.len(),
        1,
        "expected one line for one kind | received: {lines:?}"
    );
    let total = DRIFT_NAMES_SHOWN + 5;
    let expected_shown: Vec<String> = (0..DRIFT_NAMES_SHOWN)
        .map(|ordinal| format!("member{ordinal:03}"))
        .collect();
    assert!(
        lines[0].contains(&format!("({total}): {}", expected_shown.join(", ")))
            && lines[0].contains(", and 5 more;")
            && !lines[0].contains(&format!("member{DRIFT_NAMES_SHOWN:03}")),
        "expected {total} distinct names, the first {DRIFT_NAMES_SHOWN} sorted, and 5 more | received: {lines:?}"
    );
}

/// The bound the report had before it was raised: a real run with this many new names hid the
/// rest behind a count.
const FORMER_DRIFT_NAMES_SHOWN: usize = 12;

#[test]
fn a_report_names_every_member_past_the_former_bound_up_to_the_current_one() {
    let mut drift = DriftReport::default();
    let total = FORMER_DRIFT_NAMES_SHOWN * 3;
    for ordinal in 0..total {
        drift.note_denied(&format!("member{ordinal:03}"));
    }

    let lines = drift.lines(STRUCTURED_OUTPUT);

    assert_eq!(
        lines.len(),
        1,
        "expected one line for one kind | received: {lines:?}"
    );
    let missing: Vec<String> = (0..total)
        .map(|ordinal| format!("member{ordinal:03}"))
        .filter(|name| !lines[0].contains(name))
        .collect();
    assert!(
        missing.is_empty() && !lines[0].contains(" more;"),
        "expected all {total} names and no remainder | missing: {missing:?} | received: {lines:?}"
    );
}

#[test]
fn a_report_past_the_bound_counts_the_names_it_left_out() {
    let mut drift = DriftReport::default();
    let total = DRIFT_NAMES_SHOWN + 7;
    for ordinal in 0..total {
        drift.note_retained(&format!("member{ordinal:03}"));
    }

    let lines = drift.lines(STRUCTURED_OUTPUT);

    let shown = lines[0].matches("member").count();
    assert!(
        lines[0].contains(&format!("({total}):"))
            && lines[0].contains(", and 7 more;")
            && lines[0].contains(&format!("member{:03}", DRIFT_NAMES_SHOWN - 1))
            && !lines[0].contains(&format!("member{DRIFT_NAMES_SHOWN:03}")),
        "expected {total} names counted, the first {DRIFT_NAMES_SHOWN} shown and 7 more | shown: {shown} | received: {lines:?}"
    );
}

#[test]
fn a_report_escapes_a_hostile_name_and_cuts_a_long_one() {
    let mut drift = DriftReport::default();
    drift.note_denied("bell\u{7}\nline\u{1b}[2J");
    drift.note_unseen(&"x".repeat(DRIFT_NAME_CHARS + 10));
    drift.note_retained("count\u{1b}]0;title\u{7}");

    let lines = drift.lines(STRUCTURED_OUTPUT);

    assert_eq!(
        lines.len(),
        3,
        "expected one line per kind | received: {lines:?}"
    );
    assert!(
        lines[0].contains("bell\\u0007\\nline\\u001b[2J"),
        "expected the controls written as visible escapes | received: {:?}",
        lines[0]
    );
    assert!(
        lines[1].contains(&format!("{}...", "x".repeat(DRIFT_NAME_CHARS)))
            && !lines[1].contains(&"x".repeat(DRIFT_NAME_CHARS + 1)),
        "expected a name cut at {DRIFT_NAME_CHARS} characters | received: {:?}",
        lines[1]
    );
    assert!(
        lines[2].contains("count\\u001b]0;title\\u0007"),
        "expected the retained name written as visible escapes | received: {:?}",
        lines[2]
    );
    for line in &lines {
        assert!(
            !line.chars().any(char::is_control),
            "expected a one-line notice with no raw control | received: {line:?}"
        );
    }
}

#[test]
fn an_empty_report_says_nothing() {
    let drift = DriftReport::default();
    let notices = RecordedNotices::default();

    drift.announce(&notices, STRUCTURED_OUTPUT);

    assert!(
        notices.recorded().is_empty(),
        "expected no notice for a stream with no drift | received: {:?}",
        notices.recorded()
    );
}

#[test]
fn a_report_line_names_the_scenario_and_the_kind() {
    let mut drift = DriftReport::default();
    drift.note_denied("tools");
    drift.note_unseen("novel");
    drift.note_retained("counts");

    assert_eq!(
        drift.lines(STRUCTURED_OUTPUT),
        vec![
            "claude/structured-output: members replaced by the deny-by-default rule (1): tools; review them against the lists in claude_transcript.rs".to_owned(),
            "claude/structured-output: members not seen before, their strings replaced (1): novel; review them against the lists in claude_transcript.rs".to_owned(),
            "claude/structured-output: members not seen before, their numbers, booleans and objects kept (1): counts; review them against the lists in claude_transcript.rs".to_owned(),
        ]
    );
}

#[test]
fn the_reviewed_string_members_are_sorted_and_do_not_repeat_a_structural_one() {
    let mut sorted = REVIEWED_REDACTED_STRING_MEMBERS.to_vec();
    sorted.sort_unstable();
    assert_eq!(
        REVIEWED_REDACTED_STRING_MEMBERS,
        sorted.as_slice(),
        "expected the reviewed list sorted"
    );
    for member in REVIEWED_REDACTED_STRING_MEMBERS {
        assert!(
            !STRUCTURAL_STRING_MEMBERS.contains(member),
            "expected {member:?} to be reviewed once: its string is kept by STRUCTURAL_STRING_MEMBERS, so it never reaches the default"
        );
    }
}

/// Every JSON line of the committed Claude transcripts: the real streams the reviewed lists were
/// read from.
fn committed_transcript_lines() -> Vec<String> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/claude/transcripts");
    let mut lines = Vec::new();
    let entries = std::fs::read_dir(&directory).unwrap_or_else(|error| {
        panic!("expected {directory:?} to be readable | received: {error}")
    });
    for entry in entries {
        let path = entry.expect("expected a directory entry").path();
        if path
            .extension()
            .is_none_or(|extension| extension != "jsonl")
        {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("expected {path:?} to be readable | received: {error}"));
        lines.extend(
            text.lines()
                .filter(|line| !line.starts_with('#'))
                .map(str::to_owned),
        );
    }
    assert!(
        !lines.is_empty(),
        "expected committed Claude transcripts under {directory:?} | received none"
    );
    lines
}

/// What makes "seen before" checkable: the committed streams are the ones the two reviewed lists
/// were built from, so they report nothing, and a name they do show is one the lists must gain.
#[test]
fn the_committed_claude_transcripts_report_no_member_outside_the_reviewed_lists() {
    let drift = drift_over(&committed_transcript_lines());

    assert!(
        drift.denied.is_empty()
            && drift.unseen.is_empty()
            && drift.retained.is_empty()
            && drift.replaced_scalars.is_empty(),
        "expected the committed transcripts to need no new reviewed member | received denied: {:?} \
         unseen: {:?} retained: {:?} replaced: {:?}",
        drift.denied,
        drift.unseen,
        drift.retained,
        drift.replaced_scalars
    );
}

#[test]
fn a_report_holds_the_key_as_the_transcript_writes_it_not_the_raw_name() {
    let facts = MachineFacts::new();
    let home_keyed = format!("{}/novel-key", facts.home);
    let model_keyed = String::from("arn:aws:bedrock:us-east-1:111122223333:profile/novel");
    let drift = drift_over(&lines_of(&[
        json!({"type": "system", "subtype": "init", "session_id": SESSION_ID,
               home_keyed.clone(): CANARY_NUMBER}),
        json!({"type": "assistant", home_keyed.clone(): "value"}),
        json!({"type": "result", "modelUsage": {model_keyed.clone(): "value"}}),
    ]));

    let notices: Vec<String> = drift.lines(STRUCTURED_OUTPUT);
    assert_no_value_in(&notices, &[facts.home.as_str(), "111122223333"]);
    assert!(
        notices
            .iter()
            .any(|notice| notice.contains(redact::PLACEHOLDER)),
        "expected the redacted model-id key to be what the report holds | received: {notices:?}"
    );
}

/// A number member no review has seen, with a value that must never reach a notice.
const NOVEL_NUMBER_MEMBER: &str = "novelCounter";
/// A map keyed by names a user chose, as `by_type` is, under a member no review has seen.
const NOVEL_MAP_MEMBER: &str = "novelCountsByKind";
/// A key of that map, which names something the user defined.
const USER_CHOSEN_KEY: &str = "my-private-agent-6207";
/// The count under [`USER_CHOSEN_KEY`] and the numbers beside it; none may reach a notice.
const NOVEL_COUNT: u64 = 7_390_421;

fn retained_names(lines: &[Value]) -> Vec<String> {
    drift_over(&lines_of(lines)).retained.into_iter().collect()
}

#[test]
fn a_new_number_boolean_or_object_member_is_reported_by_name_and_never_by_value() {
    let names = retained_names(&[json!({"type": "result",
        NOVEL_NUMBER_MEMBER: NOVEL_COUNT,
        "novelFlag": true,
        "novelGroup": {"reviewedLater": false}})]);

    assert_eq!(
        names,
        vec![
            NOVEL_NUMBER_MEMBER,
            "novelFlag",
            "novelGroup",
            "reviewedLater"
        ],
        "expected the name of every retained non-string member outside REVIEWED_RETAINED_MEMBERS"
    );
    let drift = drift_over(&lines_of(&[
        json!({"type": "result", NOVEL_NUMBER_MEMBER: NOVEL_COUNT}),
    ]));
    assert_no_value_in(&drift.lines(STRUCTURED_OUTPUT), &[&NOVEL_COUNT.to_string()]);
}

#[test]
fn a_map_of_counts_keyed_by_user_chosen_names_reports_the_map_and_its_keys() {
    let names = retained_names(&[json!({"type": "result",
        NOVEL_MAP_MEMBER: {USER_CHOSEN_KEY: NOVEL_COUNT, "other-kind": {"count": 2}}})]);

    for expected in [NOVEL_MAP_MEMBER, USER_CHOSEN_KEY, "other-kind"] {
        assert!(
            names.iter().any(|name| name == expected),
            "expected the map name and its keys in the report | missing: {expected:?} | received: {names:?}"
        );
    }
}

#[test]
fn a_member_that_is_a_number_in_an_array_is_reported_once_under_the_array_member_name() {
    let names = retained_names(&[json!({"type": "result",
        "novelSeries": [1, 2, 3], "novelRecords": [{"count": 1}, {"count": 2}]})]);

    assert_eq!(
        names,
        vec!["count", "novelRecords", "novelSeries"],
        "expected each array member name once, whatever the item count"
    );
}

#[test]
fn a_retained_number_inside_the_models_answer_is_the_schemas_own_and_not_reported() {
    let drift = drift_over(&lines_of(&[
        json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "id": "toolu_1", "name": "StructuredOutput",
             "input": {"schemaCounter": 1, "schemaFlag": true, "schemaGroup": {"nested": 2}}}]}}),
        json!({"type": "result", "structured_output": {
            "schemaCounter": 1, "schemaGroup": {"nested": 2, "deeper": {"leaf": 3}}},
            "wire_tool_inputs": {"toolu_1": {"schemaCounter": 1}}}),
    ]));

    assert!(
        drift.retained.is_empty()
            && drift.replaced_scalars.is_empty()
            && drift.unseen.is_empty()
            && drift.map_keys.is_empty(),
        "expected no answer member in the report | received retained: {:?} replaced: {:?} strings: {:?} keys: {:?}",
        drift.retained,
        drift.replaced_scalars,
        drift.unseen,
        drift.map_keys
    );
}

#[test]
fn a_denied_number_or_object_is_replaced_and_not_reported_as_retained() {
    let facts = MachineFacts::new();
    let names = retained_names(&[
        json!({"type": "result", "resetsAt": CANARY_RESET_AT,
            "subagent_stats": {"by_type": {USER_CHOSEN_KEY: {"count": NOVEL_COUNT}}}}),
        json!({"type": "system", "subtype": "init", "session_id": SESSION_ID, "cwd": facts.cwd,
            NOVEL_INIT_MEMBER: {"nested": NOVEL_COUNT}}),
    ]);

    assert_eq!(
        names,
        vec!["subagent_stats"],
        "expected only the retained parent of a denied map | received: {names:?} | the denied \
         values are placeholders, so neither their names nor their keys can appear"
    );
}

#[test]
fn a_model_keyed_usage_entry_adds_no_name_and_its_counters_are_reviewed_ones() {
    let arn = "arn:aws:bedrock:us-east-1:111122223333:inference-profile/novel";
    let names = retained_names(&[json!({"type": "result", "modelUsage": {
        "claude-sonnet-5": {"inputTokens": 1, "outputTokens": 2},
        arn: {"inputTokens": 3}}})]);

    assert!(
        names.is_empty(),
        "expected a model id key to be data and not a member name | received: {names:?}"
    );
}

#[test]
fn a_retained_member_name_is_reported_as_the_transcript_writes_it() {
    let facts = MachineFacts::new();
    let home_keyed = format!("{}/novel-counter", facts.home);
    let drift = drift_over(&lines_of(&[
        json!({"type": "result", home_keyed.clone(): 1}),
    ]));

    let notices = drift.lines(STRUCTURED_OUTPUT);
    assert_no_value_in(&notices, &[facts.home.as_str()]);
    assert!(
        drift
            .retained
            .iter()
            .all(|name| !name.contains(&facts.home))
            && !drift.retained.is_empty(),
        "expected the home directory rewritten in the retained name | received: {:?}",
        drift.retained
    );
}

#[test]
fn the_reviewed_retained_members_are_sorted_and_unique() {
    let names: Vec<&str> = REVIEWED_RETAINED_MEMBERS
        .iter()
        .map(|(name, _)| *name)
        .collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        names, sorted,
        "expected the reviewed list sorted without a repeat"
    );
}

#[test]
fn only_the_members_the_walk_keys_by_model_id_are_declared_model_id_maps() {
    for (name, children) in REVIEWED_RETAINED_MEMBERS {
        assert_eq!(
            *children == ChildKeys::ModelIds,
            MODEL_KEYED_MEMBERS.contains(name),
            "expected {name:?} declared ModelIds exactly when MODEL_KEYED_MEMBERS {MODEL_KEYED_MEMBERS:?} lists it | received: {children:?}"
        );
    }
}

/// A reviewed-members table that gets `tool_input` wrong, to prove the check reads the table.
const TABLE_WITH_UNDECLARED_CHILDREN: &[(&str, ChildKeys)] = &[
    ("message", ChildKeys::Vendor),
    ("tool_input", ChildKeys::Leaf),
];

/// What a reviewed member holds that its declaration does not cover, one line each, found by
/// reading the raw JSON and not through the recorder, so the check is not the walk grading itself.
///
/// A [`ChildKeys::Leaf`] member must hold no object. A [`ChildKeys::Vendor`] member's non-null
/// children that carry something must each be a name some reviewed list or rule already holds. A user-chosen or
/// model-id map is covered whatever it holds. Members the recorder replaces whole or that are the
/// model's answer are not entered.
fn children_outside_declaration(records: &[Value], reviewed: &[(&str, ChildKeys)]) -> Vec<String> {
    fn is_listed(name: &str) -> bool {
        is_reviewed_retained(name)
            || REVIEWED_REDACTED_STRING_MEMBERS.contains(&name)
            || STRUCTURAL_STRING_MEMBERS.contains(&name)
            || REVIEWED_DENIED_MEMBERS.contains(&name)
            || ANSWER_MEMBERS.contains(&name)
            || is_credential_member(name)
            // The two names the walk handles by rule: `model` through `redact_model_id`, `cwd`
            // by rewriting it to the fixture's directory.
            || matches!(name, "model" | "cwd")
    }

    /// A child with nothing in it: the walk has no value to keep and no name below it to judge.
    fn carries_nothing(child: &Value) -> bool {
        match child {
            Value::Null => true,
            Value::Array(items) => items.is_empty(),
            Value::Object(members) => members.is_empty(),
            _ => false,
        }
    }

    fn walk(
        value: &Value,
        member: Option<&str>,
        reviewed: &[(&str, ChildKeys)],
        found: &mut Vec<String>,
    ) {
        match value {
            Value::Array(items) => {
                for item in items {
                    walk(item, member, reviewed, found);
                }
            }
            Value::Object(children) => {
                let declared = member.and_then(|member| {
                    reviewed
                        .iter()
                        .find_map(|(name, kind)| (*name == member).then_some(*kind))
                });
                if let (Some(member), Some(kind)) = (member, declared) {
                    match kind {
                        ChildKeys::Leaf if !children.is_empty() => found.push(format!(
                            "{member:?} is declared Leaf but holds an object with keys {:?}",
                            children.keys().collect::<Vec<_>>()
                        )),
                        ChildKeys::Vendor => {
                            for (key, child) in children {
                                if !carries_nothing(child) && !is_listed(key) {
                                    found.push(format!(
                                        "{member:?} is declared Vendor but its child {key:?} is in no reviewed list"
                                    ));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                for (key, child) in children {
                    let skipped = ANSWER_MEMBERS.contains(&key.as_str())
                        || REVIEWED_DENIED_MEMBERS.contains(&key.as_str())
                        || USER_KEYED_MEMBERS.contains(&key.as_str());
                    if !skipped {
                        walk(child, Some(key), reviewed, found);
                    }
                }
            }
            _ => {}
        }
    }

    let mut found = Vec::new();
    for record in records {
        walk(record, None, reviewed, &mut found);
    }
    found.sort_unstable();
    found.dedup();
    found
}

fn committed_transcript_records() -> Vec<Value> {
    committed_transcript_lines()
        .iter()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[test]
fn every_child_key_of_a_reviewed_member_in_the_committed_transcripts_is_listed_or_covered() {
    let found =
        children_outside_declaration(&committed_transcript_records(), REVIEWED_RETAINED_MEMBERS);

    assert!(
        found.is_empty(),
        "expected every child of a reviewed member to be a listed name or under a declared map | received: {found:#?}"
    );
}

#[test]
fn a_reviewed_member_declared_without_the_children_it_holds_is_caught() {
    let records = [json!({"type": "result", "message": {
        "novelChild": 1, "type": "x", "tool_input": {"file_path": "p"}}})];

    let found = children_outside_declaration(&records, TABLE_WITH_UNDECLARED_CHILDREN);

    assert_eq!(
        found,
        vec![
            String::from(
                r#""message" is declared Vendor but its child "novelChild" is in no reviewed list"#
            ),
            String::from(
                r#""tool_input" is declared Leaf but holds an object with keys ["file_path"]"#
            ),
        ],
        "expected the undeclared child and the Leaf that holds an object, each named"
    );
}

/// The committed transcripts show one tool call's parameters under `permission_denials` and the
/// keys of a built-in tool's result under `tool_use_result`.
const COMMITTED_USER_CHOSEN_KEYS: &[&str] = &["content", "file", "file_path", "type"];

#[test]
fn the_committed_claude_transcripts_report_only_the_keys_of_their_user_chosen_maps() {
    let drift = drift_over(&committed_transcript_lines());

    let keys: Vec<&str> = drift.map_keys.iter().map(String::as_str).collect();
    assert_eq!(
        keys, COMMITTED_USER_CHOSEN_KEYS,
        "expected the keys under the declared user-chosen maps of the committed transcripts"
    );
}

/// A parameter name a tool author chose, which no reviewed list holds.
const TOOL_PARAMETER_KEY: &str = "customerLedgerRef";

#[test]
fn a_key_of_a_user_chosen_map_is_reported_whether_or_not_a_list_holds_its_name() {
    let drift = drift_over(&lines_of(&[
        json!({"type": "result", "permission_denials": [
        {"tool_name": "mcp__ledger__lookup", "tool_input": {
            TOOL_PARAMETER_KEY: "x", "message": "y", "usage": 2, "event": {"index": 1}}}]}),
    ]));

    let keys: Vec<&str> = drift.map_keys.iter().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec![TOOL_PARAMETER_KEY, "event", "message", "usage"],
        "expected every direct key of tool_input, a reviewed name included"
    );
}

#[test]
fn every_member_the_walk_treats_as_a_tool_authors_data_is_declared_user_chosen() {
    for member in TOOL_DATA_MEMBERS {
        assert_eq!(
            child_keys_of(member),
            Some(ChildKeys::UserChosen),
            "expected {member:?} in TOOL_DATA_MEMBERS declared UserChosen in REVIEWED_RETAINED_MEMBERS | received: {:?}",
            child_keys_of(member)
        );
    }
}

/// A result key an MCP tool author chose, which no reviewed list holds.
const MCP_RESULT_KEY: &str = "ledgerBalanceDue";

/// An MCP tool's result as Claude Code records it: whatever keys the tool's author chose, with an
/// object, a number and a boolean under them.
fn mcp_shaped_tool_result() -> Value {
    json!({MCP_RESULT_KEY: 3, "ledgerEntry": {"sku": "x"}, "isOverdrawn": true})
}

#[test]
fn the_keys_of_an_mcp_tools_result_are_reported_as_user_chosen_and_not_as_vendor_drift() {
    let drift = drift_over(&lines_of(&[
        json!({"type": "user", "tool_use_result": mcp_shaped_tool_result()}),
    ]));

    let keys: Vec<&str> = drift.map_keys.iter().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec!["isOverdrawn", MCP_RESULT_KEY, "ledgerEntry"],
        "expected every direct key of the MCP result listed as a user-chosen key"
    );
    assert!(
        !drift.retained.contains("ledgerEntry") && !drift.retained.contains(MCP_RESULT_KEY),
        "expected no key of the MCP result judged as a retained member name | received: {:?}",
        drift.retained
    );
}

#[test]
fn the_keys_of_an_mcp_content_array_result_are_reported_and_the_members_below_are_judged() {
    let drift = drift_over(&lines_of(&[json!({"type": "user", "tool_use_result": [
        {"type": "text", "text": "x", "ledgerNote": "y", "ledgerEntry": {"deeperName": {"leaf": 1}}}]})]));

    let keys: Vec<&str> = drift.map_keys.iter().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec!["ledgerEntry", "ledgerNote", "text", "type"],
        "expected the direct keys of each array entry listed"
    );
    assert!(
        drift.unseen.is_empty() && drift.retained.contains("deeperName"),
        "expected a user-chosen key skipped and a member below it judged | received strings: {:?} retained: {:?}",
        drift.unseen,
        drift.retained
    );
}

#[test]
fn a_tool_result_that_is_a_bare_string_or_number_has_no_user_chosen_key() {
    let drift = drift_over(&lines_of(&[
        json!({"type": "user", "tool_use_result": "Error: refused"}),
        json!({"type": "user", "tool_use_result": 7}),
    ]));

    assert!(
        drift.map_keys.is_empty(),
        "expected no key from a result with no object | received: {:?}",
        drift.map_keys
    );
}

#[test]
fn a_tool_result_key_is_still_written_to_the_transcript_whatever_the_report_says() {
    let line =
        redacted_record(&json!({"type": "user", "tool_use_result": mcp_shaped_tool_result()}));

    assert_eq!(
        line["tool_use_result"],
        json!({MCP_RESULT_KEY: 0, "ledgerEntry": {"sku": redact::PLACEHOLDER}, "isOverdrawn": false}),
        "expected the keys kept and every value replaced | received: {line}"
    );
}

#[test]
fn a_tool_result_inside_the_models_answer_is_the_schemas_own_and_not_reported() {
    let drift = drift_over(&lines_of(&[
        json!({"type": "result", "structured_output": {"tool_use_result": {MCP_RESULT_KEY: 1}}}),
    ]));

    assert!(
        drift.map_keys.is_empty(),
        "expected no key from inside the answer | received: {:?}",
        drift.map_keys
    );
}

#[test]
fn a_user_chosen_key_is_not_also_judged_as_a_member_name() {
    let drift = drift_over(&lines_of(&[
        json!({"type": "result", "permission_denials": [
        {"tool_input": {TOOL_PARAMETER_KEY: "x", "novelCounter": 3, "novelGroup": {"leaf": true}}}]}),
    ]));

    assert!(
        drift.unseen.is_empty() && !drift.retained.contains(TOOL_PARAMETER_KEY),
        "expected a map key outside the by-name sets | received strings: {:?} retained: {:?}",
        drift.unseen,
        drift.retained
    );
    assert!(
        !drift.retained.contains("novelCounter") && !drift.retained.contains("novelGroup"),
        "expected a number and an object key outside the retained set | received: {:?}",
        drift.retained
    );
}

#[test]
fn the_members_below_a_user_chosen_key_are_judged_by_name() {
    let drift = drift_over(&lines_of(&[
        json!({"type": "result", "permission_denials": [
        {"tool_input": {"nested": {"deeperName": {"leaf": "x"}, "novelNote": "x"}}}]}),
    ]));

    assert_eq!(
        drift.map_keys.iter().collect::<Vec<_>>(),
        vec!["nested"],
        "expected only the direct child to be a map key"
    );
    assert!(
        drift.retained.contains("deeperName") && drift.unseen.contains("novelNote"),
        "expected the deeper members reported by name | received retained: {:?} strings: {:?}",
        drift.retained,
        drift.unseen
    );
}

#[test]
fn a_user_chosen_map_inside_the_models_answer_is_the_schemas_own_and_not_reported() {
    let drift = drift_over(&lines_of(&[
        json!({"type": "result", "structured_output": {
        "tool_input": {TOOL_PARAMETER_KEY: 1}}}),
    ]));

    assert!(
        drift.map_keys.is_empty(),
        "expected no key from inside the answer | received: {:?}",
        drift.map_keys
    );
}

#[test]
fn a_user_chosen_key_is_reported_as_the_transcript_writes_it() {
    let facts = MachineFacts::new();
    let home_keyed = format!("{}/ledger", facts.home);
    let drift = drift_over(&lines_of(&[
        json!({"type": "result", "permission_denials": [
        {"tool_input": {home_keyed.clone(): "x"}}]}),
    ]));

    assert!(
        !drift.map_keys.is_empty() && drift.map_keys.iter().all(|key| !key.contains(&facts.home)),
        "expected the home directory rewritten in the map key | received: {:?}",
        drift.map_keys
    );
}

#[test]
fn a_user_chosen_key_is_listed_even_when_its_value_is_denied_or_null() {
    let drift = drift_over(&lines_of(&[
        json!({"type": "result", "permission_denials": [
        {"tool_input": {"access_token": CANARY_TEXT, "emptyParam": null}}]}),
    ]));

    assert_eq!(
        drift.map_keys.iter().collect::<Vec<_>>(),
        vec!["access_token", "emptyParam"],
        "expected every key the transcript keeps, whatever its value"
    );
}

#[tokio::test]
async fn a_capture_lists_the_user_chosen_keys_on_its_notices_and_never_their_values() {
    let facts = MachineFacts::new();
    let out = tempfile::tempdir().expect("expected an output directory");
    let mut lines = answered_turn_lines(&facts);
    lines.insert(
        2,
        json!({"type": "assistant", "permission_denials": [{"tool_name": "mcp__ledger__lookup",
            "tool_input": {TOOL_PARAMETER_KEY: CANARY_TEXT, "amount": NOVEL_COUNT}}],
            "message": {"id": "msg_0", "role": "assistant", "content": []}})
        .to_string(),
    );
    let notices = RecordedNotices::default();

    capture_noticed(
        scripted_cli(lines),
        out.path(),
        &facts,
        Some(SCHEMA),
        None,
        &notices,
    )
    .await
    .unwrap_or_else(|error| panic!("expected the capture to qualify, received {error}"));

    let recorded = notices.recorded();
    assert!(
        recorded
            .iter()
            .any(|notice| notice.contains(TOOL_PARAMETER_KEY)
                && notice.contains("keys inside user-chosen maps")),
        "expected the key in a user-chosen map notice | received: {recorded:?}"
    );
    assert_no_value_in(&recorded, &[CANARY_TEXT, &NOVEL_COUNT.to_string()]);
}

#[tokio::test]
async fn a_capture_names_a_novel_retained_member_on_its_notices_and_never_its_value() {
    let facts = MachineFacts::new();
    let out = tempfile::tempdir().expect("expected an output directory");
    let mut lines = answered_turn_lines(&facts);
    lines.insert(
        2,
        json!({"type": "assistant", NOVEL_MAP_MEMBER: {USER_CHOSEN_KEY: NOVEL_COUNT},
            "message": {"id": "msg_0", "role": "assistant", "content": []}})
        .to_string(),
    );
    let notices = RecordedNotices::default();

    capture_noticed(
        scripted_cli(lines),
        out.path(),
        &facts,
        Some(SCHEMA),
        None,
        &notices,
    )
    .await
    .unwrap_or_else(|error| panic!("expected the capture to qualify, received {error}"));

    let recorded = notices.recorded();
    assert!(
        recorded
            .iter()
            .any(|notice| notice.contains(NOVEL_MAP_MEMBER)
                && notice.contains(USER_CHOSEN_KEY)
                && notice.contains("numbers, booleans and objects kept")),
        "expected the retained map and its key in a drift notice | received: {recorded:?}"
    );
    assert_no_value_in(&recorded, &[&NOVEL_COUNT.to_string()]);
}
