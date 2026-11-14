//! `mea capture --harness claude --transcripts`: records the stream-json a real `claude` run emits.
//!
//! The turn is driven through [`ClaudeHarness`], so the command line is the harness's own and the
//! recorder never starts a Claude process itself. The harness exposes no raw-line API, but it
//! spawns through the host's injected [`ProcessLauncher`], and [`TurnLineTap`] is a launcher that
//! forwards every launch to the real one and keeps a copy of the stdout bytes of the turn launch
//! only. The probes (`--version`, `--help`, `auth status`) pass through untouched.
//!
//! A transcript is an archival JSONL with a `#` header and no manifest, like the Codex ones. Every
//! line is redacted before it reaches memory the recorder writes from, see [`redact_line`]: the
//! raw bytes live only in the tap.
//!
//! The recorder never signs in. It runs the CLI the operator already logged in, the way `mea turn`
//! does, and refuses a signed-out one.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use mango_agent_claude::ClaudeHarness;
use mango_external_agents::error::{Error, Result};
use mango_external_agents::launcher::TokioLauncher;
use mango_external_agents::transport::ExecutablePath;
use mango_external_agents::{
    ApprovalRouting, AuthState, ByteSource, CancelReason, CloseReason, ConfigurationChange,
    EnvSource, EventKind, Harness, HostContext, LaunchSpec, ManagedProcess, OpenSession,
    PermissionLevel, ProcessLauncher, Session, TurnRequest,
};
use serde_json::{Map, Value, json};

use crate::redact::{self, Paths};

/// The scenario that asks Claude Code for an answer under a host-supplied JSON Schema.
pub const STRUCTURED_OUTPUT: &str = "structured-output";

/// One scripted Claude conversation.
struct Scenario {
    /// The file stem it is written to, under `transcripts/`.
    name: &'static str,
    /// What it proves, for the header line.
    purpose: &'static str,
    /// What the turn says. It asks for no tool beyond the structured answer, so nothing needs
    /// approving.
    prompt: &'static str,
    /// Whether the conversation runs under `--json-schema`, which the operator must supply.
    needs_schema: bool,
}

/// The scenarios `mea capture --harness claude --transcripts` records.
const SCENARIOS: &[Scenario] = &[Scenario {
    name: STRUCTURED_OUTPUT,
    purpose: "a turn answered under --json-schema: a StructuredOutput tool call and a result carrying structured_output",
    prompt: "Answer through the structured output tool only. Fill every string with the word mango and every number with 1. Do not use any other tool.",
    needs_schema: true,
}];

/// The named Claude transcript scenarios, for the CLI's `--scenario` parser.
pub fn scenario_names() -> Vec<&'static str> {
    SCENARIOS.iter().map(|scenario| scenario.name).collect()
}

/// Whether the named Claude scenario runs under a schema the operator supplies.
pub fn scenario_needs_schema(name: &str) -> bool {
    SCENARIOS
        .iter()
        .any(|scenario| scenario.name == name && scenario.needs_schema)
}

/// Records one Claude scenario into `out_dir/transcripts/`.
///
/// `schema` is the text of the `--json-schema` file. The CLI is the one on `PATH`, already signed
/// in; the turn runs in `workspace`. A `model` runs the turn on that id instead of the account's
/// configured one, and the transcript header records it on a `# model: ID` line.
///
/// # Errors
///
/// Whatever the launcher, the harness or the vendor reported. A conversation that fails leaves
/// `<name>.partial.jsonl` behind, one that ends without the scenario's evidence leaves
/// `<name>.unqualified.jsonl`, and neither replaces a qualified `<name>.jsonl`.
pub async fn claude(
    out_dir: &Path,
    workspace: &Path,
    scenario: &str,
    schema: Option<&str>,
    model: Option<&str>,
) -> Result<()> {
    let workspace = workspace
        .canonicalize()
        .map_err(|error| Error::HostConfiguration {
            expected: "an existing capture workspace",
            received: format!("{workspace:?}: {error}"),
        })?;
    let tap = TurnLineTap::over(Arc::new(TokioLauncher::new()));
    let host = HostContext::builder()
        .launcher(tap.clone())
        .cwd(workspace.clone())
        // As `mea turn` does: Unix validation accepts the platform temporary root as scratch.
        .scratch(std::env::temp_dir())
        .environment(EnvSource::from_process())
        .client_info("mea", env!("CARGO_PKG_VERSION"))
        .build()?;
    let (cwd, home, user) = (
        workspace.to_string_lossy().into_owned(),
        super::home_directory(),
        super::account_name(),
    );
    let paths = Paths {
        cwd: &cwd,
        home: &home,
        user: &user,
    };
    claude_with(&host, &tap, out_dir, scenario, schema, model, paths).await
}

/// [`claude`] on a host and tap the caller built, so a test can inject its own launcher.
///
/// `host` must launch through `tap`, or the transcript stays empty and the capture reports it.
async fn claude_with(
    host: &HostContext,
    tap: &TurnLineTap,
    out_dir: &Path,
    selected: &str,
    schema: Option<&str>,
    model: Option<&str>,
    paths: Paths<'_>,
) -> Result<()> {
    let scenario = SCENARIOS
        .iter()
        .find(|scenario| scenario.name == selected)
        .ok_or_else(|| Error::HostConfiguration {
            expected: "a named Claude transcript scenario",
            received: selected.to_owned(),
        })?;
    let schema = match (scenario.needs_schema, schema) {
        (true, Some(schema)) => Some(schema),
        (true, None) => {
            return Err(Error::HostConfiguration {
                expected: "a JSON Schema for a scenario that runs under --json-schema",
                received: format!("no schema for claude/{}", scenario.name),
            });
        }
        (false, _) => None,
    };
    // Validated before any process starts: the harness's refusal says what is wrong with it.
    let harness = match schema {
        Some(schema) => ClaudeHarness::new().with_json_schema(schema)?,
        None => ClaudeHarness::new(),
    };
    // The harness refuses a model that reads as a flag too, but only once it has probed the CLI.
    if let Some(model) = model {
        mango_agent_claude::models::validate_model(model)?;
    }
    let directory = out_dir.join("transcripts");
    std::fs::create_dir_all(&directory).map_err(|error| Error::HostConfiguration {
        expected: "a writable fixture directory",
        received: error.to_string(),
    })?;

    println!("capturing claude/{}: {}", scenario.name, scenario.purpose);
    let discovery = harness.discover(host).await?;
    if let AuthState::LoggedOut { login_hint } = &discovery.auth {
        return Err(Error::HostConfiguration {
            expected: "a Claude CLI that is already signed in; mea capture never signs in",
            received: format!("signed out; sign in yourself with `{login_hint}`"),
        });
    }
    let mut request = OpenSession::new("mea-capture-claude");
    // Read-only with approvals routed to the user, stated rather than inherited: the operator's
    // own default may be `bypassPermissions`, and a fixture recorded under it would describe a run
    // where nothing was ever gated. The harness has no approver, so a prompt is denied by the CLI.
    request.configuration.level = ConfigurationChange::Set(PermissionLevel::ReadOnly);
    request.configuration.routing = ConfigurationChange::Set(ApprovalRouting::User);
    if let Some(model) = model {
        // Becomes `--model` on the turn's argv; without it the account's own default applies.
        request.configuration.model = ConfigurationChange::Set(model.to_owned());
    }
    if let Some(executable) = &discovery.executable {
        request = request.with_executable(ExecutablePath::resolved(executable.clone()));
    }
    let header = header(
        scenario,
        discovery.version.as_deref(),
        host.now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .ok(),
        model,
    );

    let session = harness.open_session(host, request).await?;
    let conversation = converse(session.as_ref(), scenario).await;
    let closed = session.close(CloseReason::Requested).await;
    // Redacted before anything is written or judged: what qualifies is what the file holds.
    let body: Vec<String> = tap
        .take_lines()
        .iter()
        .filter_map(|line| redact_line(line, paths))
        .collect();
    let outcome = crate::failure::with_cleanup(conversation, closed, |primary| {
        eprintln!("{}", primary_failure_line(scenario.name, primary));
    });

    let partial = directory.join(format!("{}.partial.jsonl", scenario.name));
    let unqualified = directory.join(format!("{}.unqualified.jsonl", scenario.name));
    let qualified = directory.join(format!("{}.jsonl", scenario.name));
    let terminal = match outcome {
        Ok(terminal) => terminal,
        Err(error) => {
            // A cleanup that could not be confirmed lands here too, even when the turn finished:
            // an unreaped child makes the recording one nobody can vouch for.
            write(
                &partial,
                &join(&header, Some("# incomplete capture"), &body),
            )?;
            println!("  retained incomplete capture {}", partial.display());
            return Err(error);
        }
    };
    if let Err(received) = qualify(&body, terminal.as_deref()) {
        write(
            &unqualified,
            &join(&header, Some("# unqualified capture"), &body),
        )?;
        println!("  retained unqualified capture {}", unqualified.display());
        return Err(Error::Protocol {
            expected: String::from(
                "a captured assistant StructuredOutput tool_use and a final result whose structured_output is a JSON object",
            ),
            received,
        });
    }
    write(&qualified, &join(&header, None, &body))?;
    for stale in [&partial, &unqualified] {
        if let Err(error) = std::fs::remove_file(stale)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(Error::HostConfiguration {
                expected: "a removable stale capture diagnostic",
                received: error.to_string(),
            });
        }
    }
    println!("  wrote {}", qualified.display());
    Ok(())
}

/// Starts the turn and reads it to its end, answering nothing.
///
/// The prompt asks for no tool, so no approval is expected. One that arrives anyway is not
/// answered, here or anywhere in the library: the capture fails and the session close that follows
/// ends the child. Returns the vendor's own failure code when the turn ended in one, so a
/// capture that qualifies for nothing can say why.
async fn converse(session: &dyn Session, scenario: &Scenario) -> Result<Option<String>> {
    let mut stream = session
        .start_turn(TurnRequest::new("mea-capture-1", scenario.prompt))
        .await?;
    let drain = async {
        let mut failure = None;
        while let Some(event) = stream.recv().await {
            match event.kind {
                EventKind::ApprovalRequested { .. } => {
                    return Err(unexpected_prompt("an approval request"));
                }
                EventKind::QuestionAsked { .. } => {
                    return Err(unexpected_prompt("a question"));
                }
                EventKind::Error { error } => {
                    failure = Some(format!("{} ({})", error.code.as_str(), error.message));
                }
                _ => {}
            }
        }
        Ok(failure)
    };
    match tokio::time::timeout(crate::turn::TURN_DEADLINE, drain).await {
        Ok(result) => result,
        Err(_) => {
            session.cancel(CancelReason::Timeout).await?;
            Err(Error::Timeout {
                operation: format!("a captured claude/{} turn", scenario.name),
                after: crate::turn::TURN_DEADLINE,
            })
        }
    }
}

/// The line printed when a capture fails and its cleanup fails too: the conversation's own error,
/// which the cleanup error returned to the caller would otherwise hide.
fn primary_failure_line(scenario: &str, primary: &Error) -> String {
    format!(
        "claude/{scenario}: capture failed before cleanup: {}",
        crate::command_failure(primary)
    )
}

fn unexpected_prompt(received: &str) -> Error {
    Error::Protocol {
        expected: String::from(
            "a turn that asks nothing: the capture grants no permission and answers no question",
        ),
        received: format!("{received}; left unanswered"),
    }
}

/// The header lines of a transcript: what it is, and which build recorded it and when.
///
/// The `# capture {json}` line is the same shape the Codex subagent transcript carries, with the
/// CLI's own version where Codex has its own. A capture that chose a model adds `# model: ID`
/// under the label, as a Codex capture does, so a reader and a re-run see what it ran on; one that
/// chose none keeps the two lines the committed transcripts have.
fn header(
    scenario: &Scenario,
    version: Option<&str>,
    captured_at: Option<u64>,
    model: Option<&str>,
) -> Vec<String> {
    let metadata = json!({
        "claudeVersion": version,
        "captureVersion": env!("CARGO_PKG_VERSION"),
        "capturedAtUnixSeconds": captured_at,
    });
    let mut lines = vec![format!("# claude/{}: {}", scenario.name, scenario.purpose)];
    if let Some(model) = model {
        // The operator typed it, and on Bedrock or Vertex that can be an ARN or a project path.
        lines.push(format!("# model: {}", redact_model_id(model)));
    }
    lines.push(format!("# capture {metadata}"));
    lines
}

/// A transcript file: header, an optional label for a diagnostic, then the redacted body.
fn join(header: &[String], label: Option<&str>, body: &[String]) -> String {
    header
        .iter()
        .map(String::as_str)
        .chain(label)
        .chain(body.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn write(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text).map_err(|error| Error::HostConfiguration {
        expected: "a writable fixture file",
        received: error.to_string(),
    })
}

/// Whether the redacted transcript holds what `structured-output` exists to record.
///
/// A completed turn alone proves nothing: the CLI answers a schema it ignored with plain text. The
/// evidence is a `StructuredOutput` tool call in an assistant message and a last `result` record
/// whose `structured_output` is a JSON object. On failure the error is what was seen instead, in
/// structural words only.
fn qualify(body: &[String], vendor_failure: Option<&str>) -> std::result::Result<(), String> {
    let records: Vec<Value> = body
        .iter()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let called = records
        .iter()
        .filter(|record| record["type"] == "assistant")
        .filter_map(|record| record["message"]["content"].as_array())
        .flatten()
        .any(|block| block["type"] == "tool_use" && block["name"] == "StructuredOutput");
    let result = records
        .iter()
        .rev()
        .find(|record| record["type"] == "result");
    let answered = result.is_some_and(|record| record["structured_output"].is_object());
    if called && answered {
        return Ok(());
    }
    let kinds: Vec<String> = records
        .iter()
        .map(|record| match record["subtype"].as_str() {
            Some(subtype) => format!("{}/{subtype}", record["type"].as_str().unwrap_or("?")),
            None => record["type"].as_str().unwrap_or("?").to_owned(),
        })
        .fold(Vec::new(), |mut kinds, kind| {
            if !kinds.contains(&kind) {
                kinds.push(kind);
            }
            kinds
        });
    let received = format!(
        "StructuredOutput tool_use {}; final result structured_output {}; record kinds: {}{}",
        if called { "present" } else { "absent" },
        match result {
            None => "not received (no result record)",
            Some(record) if record["structured_output"].is_object() => "an object",
            Some(record) if record["structured_output"].is_null() => "absent",
            Some(_) => "present but not an object",
        },
        if kinds.is_empty() {
            String::from("none")
        } else {
            kinds.join(", ")
        },
        vendor_failure.map_or_else(String::new, |failure| format!("; turn failed: {failure}")),
    );
    Err(mango_external_agents::redact::bounded(
        &received,
        mango_external_agents::normalize::TextLimit::ErrorMessage,
    )
    .text)
}

// ---------------------------------------------------------------------------------------------
// Redaction
// ---------------------------------------------------------------------------------------------

/// String members that carry protocol structure rather than anything the model or the machine
/// wrote. Every other string in a recorded line becomes [`redact::PLACEHOLDER`], so a field a
/// later Claude Code adds cannot leak before this list is reviewed.
///
/// `cwd` is not here: it is always rewritten to [`redact::FIXTURE_CWD`]. `name` is not here either,
/// because it is a tool name only beside `"type": "tool_use"` and a server or plugin name
/// everywhere else; see [`redact_value`]. Neither is `model`: its value is kept only when
/// [`is_plain_vendor_model_id`] accepts it.
const STRUCTURAL_STRING_MEMBERS: &[&str] = &[
    "claude_code_version",
    "id",
    "parent_tool_use_id",
    "permissionMode",
    "role",
    "session_id",
    "stop_reason",
    "subtype",
    "terminal_reason",
    "tool_use_id",
    "type",
    "uuid",
];

/// Members whose whole subtree is the model's answer or its input: every string inside is
/// replaced, even one under a structural member name, because a schema can name a property `type`.
///
/// `wire_tool_inputs` is the same answer a second time, keyed by tool-use id: Claude Code repeats
/// each tool call's input there, so a schema that names a property `type` or `id` would survive it
/// if it were not listed. The `toolu_…` keys stay. They are opaque and random, and the same ids
/// are kept as the values of `id` and `tool_use_id` so a call still pairs with its result.
const ANSWER_MEMBERS: &[&str] = &["input", "structured_output", "wire_tool_inputs"];

/// The members of a `rate_limit_event` that identify the record rather than describe the account.
/// The record reports plan usage (utilisation, reset times, overage state, the names of the
/// windows an account has), and every other member is replaced whole, so a member a later Claude
/// Code adds is denied until it is reviewed here. Nothing replays the record: the reducer ignores
/// it.
const RATE_LIMIT_EVENT_STRUCTURE: &[&str] = &["type", "uuid", "session_id"];

/// Members that hold a map keyed by a name the user chose, such as a subagent type from their own
/// agents directory. The keys are the private part, and a map cannot keep its counts without them,
/// so the whole value is replaced.
const USER_KEYED_MEMBERS: &[&str] = &["by_type"];

/// Fragments that make a member name credential-, account- or plan-shaped, matched after the name
/// is lowercased and stripped of `_` and `-`. Such a member is replaced whatever its value, a
/// number included: a default of "redact strings" does not reach a numeric reset time, or a
/// `credential_expires_at` that a later Claude Code writes as a number (2.1.325 writes a string).
/// The plan fragments back up [`RATE_LIMIT_EVENT_STRUCTURE`] for plan usage that shows up in a
/// record of another type.
const CREDENTIAL_NAME_FRAGMENTS: &[&str] = &[
    "account",
    "apikey",
    "authorization",
    "cookie",
    "credential",
    "email",
    "oauth",
    "organization",
    "overage",
    "passwd",
    "password",
    "quota",
    "ratelimit",
    "resetsat",
    "secret",
    "userid",
    "utilization",
];

/// Whether a member name is credential- or account-shaped.
///
/// `token` as a whole word counts (`access_token`, `refresh_token`) and the plural does not, so
/// the `input_tokens` and `output_tokens` counts of a usage record survive.
fn is_credential_member(name: &str) -> bool {
    let name: String = name
        .chars()
        .filter(|character| !matches!(character, '_' | '-'))
        .flat_map(char::to_lowercase)
        .collect();
    name.ends_with("token")
        || CREDENTIAL_NAME_FRAGMENTS
            .iter()
            .any(|fragment| name.contains(fragment))
}

/// Members that hold a map keyed by model id, one entry of per-model usage each.
const MODEL_KEYED_MEMBERS: &[&str] = &["modelUsage", "model_usage_since_last_result"];

/// The aliases the CLI accepts for `--model`, which name a tier and nobody's account.
const MODEL_ALIASES: &[&str] = &["best", "default", "haiku", "opus", "opusplan", "sonnet"];

/// The cross-region prefixes of a Bedrock model id, such as `us.anthropic.claude-…`.
const BEDROCK_REGION_PREFIXES: &[&str] = &["apac", "au", "eu", "global", "jp", "us", "us-gov"];

/// The model family words of a vendor id. A word outside this list makes the id user-shaped, so a
/// family a later Claude Code adds is redacted until it is reviewed here.
const MODEL_FAMILIES: &[&str] = &["haiku", "instant", "opus", "sonnet"];

/// Whether `id` is a model name the vendor assigns, one that says nothing about who runs it.
///
/// The shapes kept: an alias (`opus`, `sonnet[1m]`), a vendor id (`claude-sonnet-5`,
/// `claude-haiku-4-5-20251001`, `claude-opus-4-1@20250805` on Vertex) and a Bedrock id, with or
/// without its cross-region prefix (`us.anthropic.claude-sonnet-4-5-20250929-v1:0`). Everything
/// else is private: an ARN carries an AWS account id, a Vertex path carries a project, and an
/// inference-profile name is whatever its owner typed. The check is an allow-list, so a shape this
/// function has not seen is redacted.
fn is_plain_vendor_model_id(id: &str) -> bool {
    // The context-window suffix (`[1m]`) selects a variant of the same model.
    let id = id.strip_suffix("[1m]").unwrap_or(id);
    if MODEL_ALIASES.contains(&id) {
        return true;
    }
    let id = id.strip_prefix("anthropic.").unwrap_or(id);
    let id = BEDROCK_REGION_PREFIXES
        .iter()
        .find_map(|region| {
            id.strip_prefix(region)
                .and_then(|rest| rest.strip_prefix(".anthropic."))
        })
        .unwrap_or(id);
    // Vertex puts the release date after `@`; it reads as one more segment.
    let id = id.replacen('@', "-", 1);
    let Some(rest) = id.strip_prefix("claude") else {
        return false;
    };
    let mut segments = rest.split('-');
    // `claude` ends its own segment: the text before the first `-` is empty.
    segments.next() == Some("")
        && segments.all(|segment| {
            MODEL_FAMILIES.contains(&segment)
                || segment == "latest"
                || is_digits(segment, 1..=2)
                || is_digits(segment, 8..=8)
                || is_bedrock_revision(segment)
        })
}

fn is_digits(text: &str, length: std::ops::RangeInclusive<usize>) -> bool {
    length.contains(&text.len()) && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// A Bedrock revision suffix: `v1` or `v1:0`.
fn is_bedrock_revision(segment: &str) -> bool {
    segment.strip_prefix('v').is_some_and(|revision| {
        let (version, minor) = revision.split_once(':').unwrap_or((revision, "0"));
        is_digits(version, 1..=2) && is_digits(minor, 1..=2)
    })
}

/// A model id as a transcript keeps it: a plain vendor id as is, any other id as
/// [`redact::PLACEHOLDER`]. See [`is_plain_vendor_model_id`].
fn redact_model_id(id: &str) -> String {
    if is_plain_vendor_model_id(id) {
        id.to_owned()
    } else {
        String::from(redact::PLACEHOLDER)
    }
}

/// `key` if the map has no such entry, otherwise `key#2`, `key#3`, … so that two keys redacted to
/// the same text keep both entries and their counters.
fn unused_key(map: &Map<String, Value>, key: String) -> String {
    if !map.contains_key(&key) {
        return key;
    }
    (2..)
        .map(|ordinal| format!("{key}#{ordinal}"))
        .find(|candidate| !map.contains_key(candidate))
        .unwrap_or(key)
}

/// One raw stdout line as the redacted line a transcript keeps, or `None` for a line that is not
/// a JSON object.
///
/// Text that is not JSON has no structure to keep and could say anything (a shell banner, a
/// warning with a path in it), so it is dropped rather than copied.
fn redact_line(raw: &str, paths: Paths<'_>) -> Option<String> {
    let record: Value = serde_json::from_str(raw).ok()?;
    record
        .is_object()
        .then(|| redact_value(record, None, ValueContext::default(), paths).to_string())
}

/// What the walk knows about the object a value sits in.
#[derive(Clone, Copy, Default)]
struct ValueContext {
    /// The enclosing object is a tool call, so its `name` is a tool name.
    tool_call: bool,
    /// The value is inside a member of [`ANSWER_MEMBERS`].
    answer: bool,
}

fn redact_value(
    value: Value,
    member: Option<&str>,
    context: ValueContext,
    paths: Paths<'_>,
) -> Value {
    match value {
        Value::Object(members) => {
            let tool_call = matches!(
                members.get("type").and_then(Value::as_str),
                Some("tool_use" | "server_tool_use")
            );
            let rate_limit_event =
                members.get("type").and_then(Value::as_str) == Some("rate_limit_event");
            // The keys of a per-model usage map are model ids, which can name an account. Inside
            // the model's answer the keys are the schema's own and stay.
            let model_keyed = !context.answer
                && member.is_some_and(|member| MODEL_KEYED_MEMBERS.contains(&member));
            let mut redacted = Map::new();
            for (name, value) in members {
                // Keys are rewritten too: a map keyed by a path would carry it.
                let key = if model_keyed {
                    redact_model_id(&name)
                } else {
                    redact::rewrite_paths(&name, paths)
                };
                let key = unused_key(&redacted, key);
                let denied = is_credential_member(&name)
                    || USER_KEYED_MEMBERS.contains(&name.as_str())
                    || (rate_limit_event && !RATE_LIMIT_EVENT_STRUCTURE.contains(&name.as_str()));
                // A null carries nothing, so it stays null.
                let kept = if denied && !value.is_null() {
                    placeholder()
                } else {
                    let inner = ValueContext {
                        tool_call,
                        answer: context.answer || ANSWER_MEMBERS.contains(&name.as_str()),
                    };
                    redact_value(value, Some(&name), inner, paths)
                };
                redacted.insert(key, kept);
            }
            Value::Object(redacted)
        }
        // An array's strings are judged by the member that holds the array, so `tools` and
        // `mcp_servers` lose their entries while `content` keeps its block types.
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| redact_value(item, member, context, paths))
                .collect(),
        ),
        Value::String(text) => match member {
            // Whatever the CLI says its directory is, the transcript says the fixture's: a
            // symlinked or canonicalised spelling of the workspace is not rewritten by value.
            Some("cwd") => Value::String(String::from(redact::FIXTURE_CWD)),
            Some("name") if context.tool_call && !context.answer && !text.starts_with("mcp__") => {
                Value::String(redact::rewrite_paths(&text, paths))
            }
            Some("model") if !context.answer => Value::String(redact_model_id(&text)),
            Some(member) if !context.answer && STRUCTURAL_STRING_MEMBERS.contains(&member) => {
                Value::String(redact::rewrite_paths(&text, paths))
            }
            _ => placeholder(),
        },
        other => other,
    }
}

fn placeholder() -> Value {
    Value::String(String::from(redact::PLACEHOLDER))
}

// ---------------------------------------------------------------------------------------------
// The tap
// ---------------------------------------------------------------------------------------------

/// A launcher that forwards to the host's own and keeps the stdout lines of the turn launch.
///
/// Only the launch that streams JSON (`--output-format stream-json`, the harness's turn argv) is
/// tapped; the probes the harness runs through the same launcher are not.
pub(super) struct TurnLineTap {
    inner: Arc<dyn ProcessLauncher>,
    lines: Arc<Mutex<Vec<String>>>,
}

impl TurnLineTap {
    /// A tap over the launcher that really starts the CLI.
    pub(super) fn over(inner: Arc<dyn ProcessLauncher>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            lines: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Hands over the lines read so far, leaving the tap empty.
    pub(super) fn take_lines(&self) -> Vec<String> {
        std::mem::take(&mut *self.lines.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// Whether a launch is the harness's turn rather than a probe.
fn is_turn_launch(argv: &[String]) -> bool {
    argv.windows(2)
        .any(|pair| pair[0] == "--output-format" && pair[1] == "stream-json")
}

#[async_trait::async_trait]
impl ProcessLauncher for TurnLineTap {
    async fn spawn(&self, spec: LaunchSpec) -> Result<ManagedProcess> {
        let tapped = is_turn_launch(&spec.argv);
        let mut process = self.inner.spawn(spec).await?;
        if tapped {
            process.stdout = Box::new(TappedStdout {
                inner: std::mem::replace(&mut process.stdout, Box::new(EndedStdout)),
                lines: Arc::clone(&self.lines),
                unfinished: Vec::new(),
            });
        }
        Ok(process)
    }
}

/// A child's stdout that copies each complete line into the tap as it passes.
struct TappedStdout {
    inner: Box<dyn ByteSource>,
    lines: Arc<Mutex<Vec<String>>>,
    /// Bytes of a line whose newline has not arrived yet.
    unfinished: Vec<u8>,
}

impl TappedStdout {
    fn keep(&self, line: &[u8]) {
        let text = String::from_utf8_lossy(line);
        let text = text.trim_end_matches('\r');
        if !text.trim().is_empty() {
            self.lines
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(text.to_owned());
        }
    }
}

#[async_trait::async_trait]
impl ByteSource for TappedStdout {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        let chunk = self.inner.next_chunk().await?;
        match &chunk {
            Some(bytes) => {
                self.unfinished.extend_from_slice(bytes);
                while let Some(end) = self.unfinished.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = self.unfinished.drain(..=end).collect();
                    self.keep(&line[..end]);
                }
            }
            // The last line may end without a newline; the stream is over, so it is complete.
            None => {
                let line = std::mem::take(&mut self.unfinished);
                self.keep(&line);
            }
        }
        Ok(chunk)
    }
}

/// The stdout a spawned process is left holding while its real one is being wrapped.
struct EndedStdout;

#[async_trait::async_trait]
impl ByteSource for EndedStdout {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests;
