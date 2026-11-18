//! Public, reproducible vendor contracts for the drift check.
//!
//! Transcript capture records a real conversation, so an account, a model, and a vendor release
//! can all change its bytes. This module records only commands that need no login and the ACP
//! handshake that opens no session. The resulting files are safe to regenerate in CI. The one
//! exception is an ACP capture the operator opts into with `--open-session`: it also sends
//! `session/new`, which is a maintainer capture and never a CI one, and which runs the adapter
//! under a scratch home so that it stays unauthenticated.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mango_external_agents::error::{Error, Result};
use mango_external_agents::launcher::TokioLauncher;
use mango_external_agents::transports::stdio;
use mango_external_agents::{
    CancelReason, EnvSource, ErrorCode, ExecutablePath, HostContext, JsonRpcError,
    ProcessCleanupGuard, ProcessLauncher, StdioSpec,
};
use serde_json::{Map, Value, json};

mod agent_home;
pub mod manifest;
mod session_new;

/// The built-in ACP profile that the drift workflow installs on every operating system.
pub const DEFAULT_ACP_CAPTURE_PROFILE: &str = "opencode";

/// How `version.json` says a vendor CLI version got there: the operator named the binary.
const VENDOR_CLI_SOURCE: &str = "named by the operator, not located by mea";

/// A public capture command must answer promptly or leave the caller a useful failure.
const CONTRACT_TIMEOUT: Duration = Duration::from_secs(30);

/// Captures the public Claude CLI contract under `out_dir/contract`.
///
/// The capture runs only `claude --version` and `claude --help`. It deliberately does not run
/// `claude auth status`: login state varies by machine and is not a public, reproducible contract.
///
/// # Examples
///
/// `mea capture --harness claude --out fixtures/claude --workspace /tmp/mea-capture`
///
/// # Errors
///
/// Returns the launcher or filesystem error when either public probe cannot be recorded.
pub async fn claude(out_dir: &Path, workspace: &Path) -> Result<()> {
    claude_with(&capture_host(workspace)?, out_dir).await
}

/// Captures the public OpenAI app-server schema handshake contract under `out_dir/contract`.
///
/// The existing transcript capture stays separate because it records a live account conversation.
/// This operation reads the installed CLI's version, its `--help` and `app-server --help` text, and
/// invokes the documented schema generator in `workspace/codex-schema`. The two help texts make a
/// new subcommand or a changed `--listen` transport list show up as drift. None of the probes needs
/// a login or starts a session.
///
/// # Examples
///
/// `mea capture --harness codex --out fixtures/codex --workspace /tmp/mea-capture`
///
/// # Errors
///
/// Returns an error when the generator cannot write the required initialization schemas.
pub async fn codex_contract(out_dir: &Path, workspace: &Path) -> Result<()> {
    codex_contract_with(&capture_host(workspace)?, out_dir, workspace).await
}

/// Captures one built-in ACP profile's public `initialize` metadata under `out_dir/contract`.
///
/// The command sends no `authenticate` request, and no `session/new` request unless `open_session`
/// is set. `profile_id` must name a profile compiled into `mango-agent-acp`.
///
/// `vendor_cli` names the binary an adapter fronts, for the profiles whose adapter runs one
/// (`claude-agent-acp`, `codex-acp`). Its `--version` is recorded in `version.json` as `vendorCli`.
/// `mea` takes the name as given and never searches for the binary.
///
/// `open_session` also sends `session/new` once `initialize` has answered and records the mode ids
/// and `configOptions` in `session-new.json`, or the refusal an adapter that wants a login sends
/// instead. It is refused for a profile whose capture the drift lane records again, because an
/// answer that depends on the machine's agent state is not reproducible.
///
/// With `open_session`, every process of the capture runs under a scratch `HOME`,
/// `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and XDG directories that `mea` creates empty and removes
/// afterwards, error or not. The capture is unauthenticated by construction: it never sees the
/// operator's account, never writes into `~/.claude` or `~/.codex`, and what it records cannot
/// reflect the operator's subscription.
///
/// # Examples
///
/// `mea capture --harness acp --profile opencode --out fixtures/acp/opencode --workspace /tmp/mea-capture`
///
/// # Errors
///
/// Returns an error for an unknown profile, an unsuccessful probe, a malformed JSON-RPC answer, a
/// `vendor_cli` that does not print exactly one `--version` line, or `open_session` on a
/// reproducible profile.
pub async fn acp(
    out_dir: &Path,
    workspace: &Path,
    profile_id: &str,
    vendor_cli: Option<&str>,
    open_session: bool,
) -> Result<()> {
    acp_under(
        Arc::new(TokioLauncher::new()),
        EnvSource::from_process(),
        out_dir,
        workspace,
        profile_id,
        vendor_cli,
        open_session,
    )
    .await
}

/// [`acp`] with the launcher and the operator's environment named, so a test can watch what the
/// adapter is given.
async fn acp_under(
    launcher: Arc<dyn ProcessLauncher>,
    operator_environment: EnvSource,
    out_dir: &Path,
    workspace: &Path,
    profile_id: &str,
    vendor_cli: Option<&str>,
    open_session: bool,
) -> Result<()> {
    let profile =
        mango_agent_acp::builtin_profile(profile_id).ok_or_else(|| Error::HostConfiguration {
            expected: "a built-in ACP profile",
            received: profile_id.to_owned(),
        })?;
    // The agent starts in the capture workspace, which is also where `session/new` names its cwd.
    let session = open_session.then(|| workspace.to_path_buf());
    // `session/new` makes the adapter start its backend, which reads and writes the account under
    // HOME, CLAUDE_CONFIG_DIR and CODEX_HOME. An opened session must never be the operator's, so
    // every command of this capture sees empty scratch directories instead. `home` stays alive to
    // the end of this function, so it is removed on success and on every error path alike.
    let home = open_session
        .then(agent_home::AgentHome::create)
        .transpose()?;
    let environment = match &home {
        Some(home) => home.environment(&operator_environment),
        None => operator_environment,
    };
    let host = capture_host_with(launcher, environment, workspace)?;
    acp_with(
        &host,
        out_dir,
        profile.as_ref(),
        vendor_cli,
        session.as_deref(),
    )
    .await
}

async fn claude_with(host: &HostContext, out_dir: &Path) -> Result<()> {
    let version = command_output(
        host,
        vec![
            String::from(mango_agent_claude::probe::PROGRAM),
            String::from("--version"),
        ],
        mango_agent_claude::pinned::VENDOR_ENVIRONMENT_KEYS,
    )
    .await?;
    let help = command_output(
        host,
        vec![
            String::from(mango_agent_claude::probe::PROGRAM),
            String::from("--help"),
        ],
        mango_agent_claude::pinned::VENDOR_ENVIRONMENT_KEYS,
    )
    .await?;
    let surface = claude_surface(&help);

    write_text(
        &contract_dir(out_dir).join("help.txt"),
        &format!("{help}\n"),
    )?;
    write_json(
        &contract_dir(out_dir).join("version.json"),
        &json!({
            "command": ["claude", "--version"],
            "output": version,
        }),
    )?;
    write_json(&contract_dir(out_dir).join("cli-surface.json"), &surface)?;
    manifest::write(
        &contract_dir(out_dir),
        &json!({
            "format": 1,
            "probes": ["claude --version", "claude --help"],
            "reproducible": manifest::is_reproducible("claude"),
        }),
        &version,
    )
}

async fn codex_contract_with(host: &HostContext, out_dir: &Path, workspace: &Path) -> Result<()> {
    // `app-server generate-json-schema` is still the app-server binary: a build below the floor
    // is refused before it runs, as it is for a capture and for a session.
    super::require_supported_codex(host).await?;
    let version = command_output(
        host,
        vec![String::from("codex"), String::from("--version")],
        &["CODEX_HOME"],
    )
    .await?;
    let help = codex_portable_help(
        &command_output(
            host,
            vec![String::from("codex"), String::from("--help")],
            &["CODEX_HOME"],
        )
        .await?,
    );
    let app_server_help = command_output(
        host,
        vec![
            String::from("codex"),
            String::from("app-server"),
            String::from("--help"),
        ],
        &["CODEX_HOME"],
    )
    .await?;
    let generated = workspace.join("codex-schema");
    std::fs::create_dir_all(&generated).map_err(|error| Error::HostConfiguration {
        expected: "a writable Codex schema workspace",
        received: error.to_string(),
    })?;
    command_succeeds(
        host,
        vec![
            String::from("codex"),
            String::from("app-server"),
            String::from("generate-json-schema"),
            String::from("--out"),
            generated.to_string_lossy().into_owned(),
        ],
        &["CODEX_HOME"],
    )
    .await?;

    let contract = contract_dir(out_dir);
    // Stored as printed, as the Claude help is, except for the platform-gated subcommands
    // `codex_portable_help` drops. The check compares bytes, so any reworded or added subcommand,
    // flag or `--listen` value is a diff a maintainer reads.
    write_text(&contract.join("help.txt"), &format!("{help}\n"))?;
    write_text(
        &contract.join("app-server-help.txt"),
        &format!("{app_server_help}\n"),
    )?;
    copy_generated_schema(
        &generated.join("v1/InitializeParams.json"),
        &contract.join("initialize-params.schema.json"),
    )?;
    copy_generated_schema(
        &generated.join("v1/InitializeResponse.json"),
        &contract.join("initialize-response.schema.json"),
    )?;
    write_json(
        &contract.join("version.json"),
        &json!({
            "command": ["codex", "--version"],
            "output": version,
        }),
    )?;
    manifest::write(
        &contract,
        &json!({
            "format": 1,
            "probes": [
                "codex --version",
                "codex --help",
                "codex app-server --help",
                "codex app-server generate-json-schema --out <workspace>/codex-schema"
            ],
            "reproducible": manifest::is_reproducible("codex"),
            "schemaFiles": ["initialize-params.schema.json", "initialize-response.schema.json"],
        }),
        &version,
    )
}

async fn acp_with(
    host: &HostContext,
    out_dir: &Path,
    profile: &mango_agent_acp::AcpProfile,
    vendor_cli: Option<&str>,
    session_cwd: Option<&Path>,
) -> Result<()> {
    let reproducible = manifest::is_reproducible(profile.id.as_str());
    // Refused before any process starts: a session answer follows the machine's agent state, so a
    // capture the drift lane records again must not carry one.
    if session_cwd.is_some() && reproducible {
        return Err(Error::HostConfiguration {
            expected: "--open-session only for a maintainer capture, not a profile the drift lane records again",
            received: format!("--open-session with profile {}", profile.id.as_str()),
        });
    }
    if let Some(cwd) = session_cwd.filter(|cwd| !cwd.is_absolute()) {
        return Err(Error::HostConfiguration {
            expected: "an absolute --workspace for session/new, which the protocol requires as cwd",
            received: cwd.display().to_string(),
        });
    }
    let version = command_output(
        host,
        profile.resolved_version_argv(&ExecutablePath::default()),
        profile.vendor_environment_keys,
    )
    .await?;
    // Probed before `initialize` and before any file is written: a refused vendor answer leaves
    // no half-recorded capture behind.
    let vendor = match vendor_cli {
        Some(program) => Some(vendor_cli_version(host, profile, program).await?),
        None => None,
    };
    let probe = acp_probe(host, profile, session_cwd).await?;
    let contract = contract_dir(out_dir);
    write_json(&contract.join("initialize.json"), &probe.initialize)?;
    if let Some(session) = &probe.session_new {
        write_json(&contract.join("session-new.json"), &session.record)?;
    }
    let mut version_file = json!({
        "command": profile.resolved_version_argv(&ExecutablePath::default()),
        "output": version,
    });
    if let (Some(vendor), Some(members)) = (vendor, version_file.as_object_mut()) {
        members.insert(String::from("vendorCli"), vendor);
    }
    write_json(&contract.join("version.json"), &version_file)?;
    manifest::write(
        &contract,
        &json!({
            "format": 1,
            "profile": profile.id.as_str(),
            "probe": if probe.session_new.is_some() { "initialize, session/new" } else { "initialize" },
            "reproducible": reproducible,
            "sessionOpened": probe.session_new.as_ref().is_some_and(|session| session.opened),
        }),
        &version,
    )
}

fn capture_host(workspace: &Path) -> Result<HostContext> {
    capture_host_with(
        Arc::new(TokioLauncher::new()),
        EnvSource::from_process(),
        workspace,
    )
}

fn capture_host_with(
    launcher: Arc<dyn ProcessLauncher>,
    environment: EnvSource,
    workspace: &Path,
) -> Result<HostContext> {
    HostContext::builder()
        .launcher(launcher)
        .cwd(workspace.to_path_buf())
        .environment(environment)
        .client_info("mea", env!("CARGO_PKG_VERSION"))
        .build()
}

pub(super) async fn command_output(
    host: &HostContext,
    argv: Vec<String>,
    vendor_environment_keys: &[&str],
) -> Result<String> {
    let lines = command_lines(host, argv, vendor_environment_keys).await?;
    if lines.is_empty() {
        return Err(Error::Protocol {
            expected: String::from("a public probe which writes stdout"),
            received: String::from("no stdout"),
        });
    }
    Ok(lines.join("\n"))
}

async fn command_succeeds(
    host: &HostContext,
    argv: Vec<String>,
    vendor_environment_keys: &[&str],
) -> Result<()> {
    let _ = command_lines(host, argv, vendor_environment_keys).await?;
    Ok(())
}

async fn command_lines(
    host: &HostContext,
    argv: Vec<String>,
    vendor_environment_keys: &[&str],
) -> Result<Vec<String>> {
    let transport = stdio::open(
        host,
        &StdioSpec::new(argv.clone()),
        &ExecutablePath::default(),
        vendor_environment_keys,
    )
    .await?;
    let control = transport.control;
    let cleanup =
        ProcessCleanupGuard::new(Arc::clone(&control), *host.limits(), CancelReason::Shutdown);
    let (mut sender, mut receiver) = transport.link.split();
    let outcome = async {
        sender.close().await?;
        let read = tokio::time::timeout(CONTRACT_TIMEOUT, async {
            let mut lines = Vec::new();
            while let Some(line) = receiver.recv().await? {
                lines.push(line);
            }
            Ok::<_, Error>(lines)
        })
        .await
        .map_err(|_| Error::Timeout {
            operation: format!("{} output", command_name(&argv)),
            after: CONTRACT_TIMEOUT,
        })??;
        let status = tokio::time::timeout(CONTRACT_TIMEOUT, control.wait())
            .await
            .map_err(|_| Error::Timeout {
                operation: format!("{} exit", command_name(&argv)),
                after: CONTRACT_TIMEOUT,
            })??;
        if status.success() {
            return Ok(read);
        }
        Err(Error::Protocol {
            expected: format!("{} exiting successfully", command_name(&argv)),
            received: format!("exit {status:?}: {}", control.stderr_tail()),
        })
    }
    .await;
    let stopped = cleanup.finish().await.map(|_| ());
    crate::failure::with_cleanup(outcome, stopped, |primary| {
        eprintln!(
            "{}: public capture failed before cleanup: {}",
            command_name(&argv),
            crate::command_failure(primary)
        );
    })
}

/// What one ACP handshake recorded: the normalized `initialize` answer and, when asked for, the
/// `session/new` answer.
struct AcpProbe {
    initialize: Value,
    session_new: Option<session_new::SessionNewRecord>,
}

/// The `initialize` half of [`acp_probe`], which the cleanup and cancellation tests drive alone.
#[cfg(test)]
async fn acp_initialize(
    host: &HostContext,
    profile: &mango_agent_acp::AcpProfile,
) -> Result<Value> {
    acp_probe(host, profile, None)
        .await
        .map(|probe| probe.initialize)
}

/// Sends `initialize` and, with `session_cwd`, `session/new` over one stdio child, then stops it.
///
/// `session/new` goes out only after `initialize` answered with a result, and never with an
/// `authenticate` before it: an adapter that wants a login answers with an error, which the record
/// keeps. The child is not asked to close the session; stopping it ends the session.
async fn acp_probe(
    host: &HostContext,
    profile: &mango_agent_acp::AcpProfile,
    session_cwd: Option<&Path>,
) -> Result<AcpProbe> {
    let argv = profile.resolved_argv(&ExecutablePath::default());
    let transport = stdio::open(
        host,
        &StdioSpec::new(argv),
        &ExecutablePath::default(),
        profile.vendor_environment_keys,
    )
    .await?;
    let cleanup =
        ProcessCleanupGuard::new(transport.control, *host.limits(), CancelReason::Shutdown);
    let (mut sender, mut receiver) = transport.link.split();
    let outcome = async {
        let frame = acp_request(
            sender.as_mut(),
            receiver.as_mut(),
            profile,
            1,
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {"fs": {}, "terminal": false},
                "clientInfo": {"name": "mea", "version": env!("CARGO_PKG_VERSION")},
            }),
        )
        .await?;
        let initialize = normalize_acp_initialize(frame)?;
        let session_new = match session_cwd {
            Some(cwd) => {
                let frame = acp_request(
                    sender.as_mut(),
                    receiver.as_mut(),
                    profile,
                    2,
                    "session/new",
                    json!({"cwd": cwd, "mcpServers": []}),
                )
                .await?;
                Some(session_new::normalize_session_new(&frame)?)
            }
            None => None,
        };
        Ok(AcpProbe {
            initialize,
            session_new,
        })
    }
    .await;
    let stopped = cleanup.finish().await.map(|_| ());
    crate::failure::with_cleanup(outcome, stopped, |primary| {
        eprintln!(
            "{}: public capture failed before cleanup: {}",
            profile.display_name,
            crate::command_failure(primary)
        );
    })
}

/// Sends one request and waits for the frame that answers `id`, skipping what the agent writes in
/// between (logs, notifications, its own requests).
async fn acp_request(
    sender: &mut dyn mango_external_agents::LinkSender,
    receiver: &mut dyn mango_external_agents::LinkReceiver,
    profile: &mango_agent_acp::AcpProfile,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value> {
    sender
        .send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string())
        .await?;
    tokio::time::timeout(CONTRACT_TIMEOUT, async {
        loop {
            let Some(line) = receiver.recv().await? else {
                return Err(Error::Link {
                    peer: profile.display_name.clone(),
                    message: format!("exited before answering {method}"),
                });
            };
            let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if frame.get("id") == Some(&json!(id)) && frame.get("method").is_none() {
                return Ok(frame);
            }
        }
    })
    .await
    .map_err(|_| Error::Timeout {
        operation: format!("{} {method}", profile.display_name),
        after: CONTRACT_TIMEOUT,
    })?
}

fn normalize_acp_initialize(frame: Value) -> Result<Value> {
    // An adapter that cannot start (`codex-acp` with a `CODEX_HOME` that does not exist) answers
    // `initialize` with an error frame. `Error::Protocol` never prints what it received, so the
    // cause is raised as the vendor failure the ACP harness uses for the same reply: `mea` prints
    // that one with the agent's redacted message and code.
    // A `null` error member is no error: some peers write one beside a result.
    if let Some(error) = frame.get("error").filter(|error| !error.is_null()) {
        return Err(
            match serde_json::from_value::<JsonRpcError>(error.clone()) {
                Ok(error) => Error::Vendor(
                    error.into_vendor_error(ErrorCode::from_static("acp-request-failed"), None),
                ),
                Err(_) => Error::Protocol {
                    expected: String::from(
                        "an ACP initialize JSON-RPC error with an integer code and a string message",
                    ),
                    received: value_shape(error),
                },
            },
        );
    }
    let result = frame
        .get("result")
        .and_then(Value::as_object)
        .ok_or_else(|| Error::Protocol {
            expected: String::from("an ACP initialize result object"),
            received: value_shape(&frame),
        })?;
    let protocol_version =
        result
            .get("protocolVersion")
            .cloned()
            .ok_or_else(|| Error::Protocol {
                expected: String::from("an ACP initialize result with protocolVersion"),
                received: object_shape(result),
            })?;
    let mut normalized = Map::from_iter([(String::from("protocolVersion"), protocol_version)]);
    if let Some(capabilities) = result.get("agentCapabilities") {
        normalized.insert(
            String::from("agentCapabilities"),
            normalize_capabilities(capabilities),
        );
    }
    if let Some(methods) = result.get("authMethods") {
        normalized.insert(String::from("authMethods"), normalize_auth_methods(methods));
    }
    if let Some(info) = result.get("agentInfo").and_then(Value::as_object) {
        let mut public_info = Map::new();
        copy_public_member(info, &mut public_info, "name");
        copy_public_member(info, &mut public_info, "version");
        if !public_info.is_empty() {
            normalized.insert(String::from("agentInfo"), Value::Object(public_info));
        }
    }
    Ok(Value::Object(normalized))
}

fn copy_public_member(source: &Map<String, Value>, target: &mut Map<String, Value>, key: &str) {
    if let Some(value) = source.get(key) {
        target.insert(key.to_owned(), value.clone());
    }
}

fn normalize_capabilities(value: &Value) -> Value {
    match value {
        Value::Object(members) => Value::Object(
            members
                .iter()
                .filter(|(key, _)| key.as_str() != "_meta")
                .map(|(key, value)| (key.clone(), normalize_capabilities(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(normalize_capabilities).collect()),
        Value::String(_) => Value::String(String::from("[REDACTED]")),
        value => value.clone(),
    }
}

fn normalize_auth_methods(value: &Value) -> Value {
    let Some(methods) = value.as_array() else {
        return Value::Array(Vec::new());
    };
    Value::Array(
        methods
            .iter()
            .filter_map(Value::as_object)
            .map(|method| {
                let mut public = Map::new();
                copy_public_member(method, &mut public, "type");
                copy_public_member(method, &mut public, "id");
                copy_public_member(method, &mut public, "name");
                Value::Object(public)
            })
            .collect(),
    )
}

fn value_shape(value: &Value) -> String {
    match value {
        Value::Object(members) => object_shape(members),
        Value::Array(items) => format!("array with {} item(s)", items.len()),
        Value::String(_) => String::from("string"),
        Value::Number(_) => String::from("number"),
        Value::Bool(_) => String::from("boolean"),
        Value::Null => String::from("null"),
    }
}

fn object_shape(members: &Map<String, Value>) -> String {
    let mut names: Vec<&str> = members.keys().map(String::as_str).collect();
    names.sort_unstable();
    format!("object with members {}", names.join(", "))
}

fn claude_surface(help: &str) -> Value {
    let parsed = mango_agent_claude::cli_surface::CliSurface::parse(help);
    let mut flags: Vec<String> = mango_agent_claude::help::declared_options(help)
        .into_iter()
        .flat_map(|option| option.flags)
        .collect();
    flags.sort();
    flags.dedup();
    json!({
        "flags": flags,
        "permissionModes": parsed.accepted_modes().map(|modes| modes.iter().cloned().collect::<Vec<_>>()),
        "modelAliases": parsed.model_aliases(),
        "effortLevels": parsed.effort_levels(),
    })
}

/// Codex subcommands that only the macOS and Windows builds list in `codex --help`.
///
/// The pinned drift lane reproduces the contract on all three operating systems and the weekly lane
/// on Linux, so a row that exists on two of them would make every run fail on the third. `app`
/// launches the desktop app, which Codex does not ship for Linux.
const PLATFORM_ONLY_CODEX_COMMANDS: &[&str] = &["app"];

/// `help` without the rows of [`PLATFORM_ONLY_CODEX_COMMANDS`] and their wrapped description lines.
///
/// A row is a line indented by exactly two spaces whose first word is the command; its wrapped
/// description continues on lines indented deeper. Everything else is kept byte for byte.
fn codex_portable_help(help: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut dropping = false;
    for line in help.lines() {
        let command_row = line
            .strip_prefix("  ")
            .filter(|rest| !rest.starts_with(' '))
            .and_then(|rest| rest.split_whitespace().next());
        if let Some(name) = command_row {
            dropping = PLATFORM_ONLY_CODEX_COMMANDS.contains(&name);
        } else if !line.starts_with("    ") {
            dropping = false;
        }
        if !dropping {
            kept.push(line);
        }
    }
    kept.join("\n")
}

fn contract_dir(out_dir: &Path) -> PathBuf {
    out_dir.join("contract")
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    let text = serde_json::to_string_pretty(value).map_err(|error| Error::Protocol {
        expected: String::from("a serializable public fixture contract"),
        received: error.to_string(),
    })?;
    write_text(path, &format!("{text}\n"))
}

fn write_text(path: &Path, contents: &str) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Err(Error::HostConfiguration {
            expected: "a fixture path with a parent directory",
            received: path.display().to_string(),
        });
    };
    std::fs::create_dir_all(parent).map_err(|error| Error::HostConfiguration {
        expected: "a writable fixture directory",
        received: error.to_string(),
    })?;
    std::fs::write(path, contents).map_err(|error| Error::HostConfiguration {
        expected: "a writable public fixture file",
        received: error.to_string(),
    })
}

fn copy_generated_schema(source: &Path, target: &Path) -> Result<()> {
    let contents = std::fs::read_to_string(source).map_err(|error| Error::HostConfiguration {
        expected: "Codex app-server generated initialization schema files",
        received: format!("{}: {error}", source.display()),
    })?;
    write_text(target, &contents)
}

/// The command `version.json` records for a named vendor CLI: its file name and `--version`.
///
/// The operator names the program, often by a path inside an install tree. The path is a machine
/// path and stays out of the committed file, so only the final component is kept.
///
/// # Errors
///
/// Returns an error naming `program` when it has no file name, such as `..`.
fn vendor_cli_command(program: &str) -> Result<Vec<String>> {
    let name = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::HostConfiguration {
            expected: "--vendor-cli to name an executable by a path or a program name",
            received: format!("{program:?}"),
        })?;
    Ok(vec![name.to_owned(), String::from("--version")])
}

/// What the named vendor CLI prints for `--version`, shaped for `version.json`.
///
/// `mea` does not look for the binary beneath an adapter: each adapter picks it by its own rules,
/// which are not a documented contract. The operator names it instead, so the member says so rather
/// than claiming the adapter was seen running that binary. The probe goes through the same
/// cleanup-owned launcher and the profile's vendor environment as every other public capture.
///
/// # Errors
///
/// Returns an error naming `program` when it does not exit successfully with exactly one line.
async fn vendor_cli_version(
    host: &HostContext,
    profile: &mango_agent_acp::AcpProfile,
    program: &str,
) -> Result<Value> {
    let recorded = vendor_cli_command(program)?;
    let refuse = |received: String| Error::HostConfiguration {
        expected: "--vendor-cli to name an executable whose --version prints exactly one line",
        received: format!("{program:?}: {received}"),
    };
    let output = command_output(
        host,
        vec![program.to_owned(), String::from("--version")],
        profile.vendor_environment_keys,
    )
    .await
    .map_err(|error| refuse(error.to_string()))?;
    let lines = output.lines().count();
    if lines != 1 {
        return Err(refuse(format!("{lines} lines")));
    }
    Ok(json!({
        "source": VENDOR_CLI_SOURCE,
        "command": recorded,
        "output": output,
    }))
}

fn command_name(argv: &[String]) -> String {
    argv.first()
        .map(|program| mango_external_agents::redact::program_name(program))
        .unwrap_or_else(|| String::from("capture command"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{
        CONTRACT_TIMEOUT, acp_initialize, acp_with, claude_surface, claude_with,
        codex_contract_with, codex_portable_help, command_name, command_output,
        copy_generated_schema, normalize_acp_initialize, vendor_cli_command, write_json,
    };
    use crate::capture::tests::{CaptureCleanupLauncher, CaptureStop, capture_host_with_limits};
    use mango_external_agents::testing::{FakeLauncher, FakeProcess};
    use mango_external_agents::{
        ByteSink, CancelReason, Dispatch, EnvSource, Error, ExitStatus, HostContext, LaunchSpec,
        Limits, ManagedProcess, ProcessControl, ProcessLauncher, Result,
    };
    use serde_json::json;

    fn host(launcher: Arc<FakeLauncher>) -> HostContext {
        HostContext::builder()
            .launcher(launcher)
            .cwd(std::env::temp_dir())
            .environment(EnvSource::from_pairs([("PATH", "/fake/bin")]))
            .client_info("mea-test", "0.0.0")
            .build()
            .expect("expected a fake capture host")
    }

    fn temp_dir(label: &str) -> std::path::PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "mea-contract-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("expected a temporary contract directory");
        path
    }

    #[test]
    fn initialize_capture_drops_request_ids_extensions_and_machine_paths() {
        let normalized = normalize_acp_initialize(json!({
            "jsonrpc": "2.0",
            "id": 73,
            "result": {
                "protocolVersion": 1,
                "agentCapabilities": {
                    "loadSession": true,
                    "workspace": "/private/agent",
                    "_meta": {"installationPath": "/private/agent"}
                },
                "authMethods": [{
                    "id": "local",
                    "name": "Local",
                    "description": "API_KEY=private",
                    "env": {"API_KEY": "private"}
                }],
                "agentInfo": {"name": "Example", "version": "1.2.3", "path": "/private/agent"},
                "_meta": {"installationPath": "/private/agent"}
            }
        }))
        .expect("expected an initialize result");

        assert_eq!(normalized["protocolVersion"], 1);
        assert_eq!(
            normalized["agentInfo"],
            json!({"name": "Example", "version": "1.2.3"})
        );
        assert_eq!(normalized["agentCapabilities"]["workspace"], "[REDACTED]");
        assert!(normalized["agentCapabilities"].get("_meta").is_none());
        assert_eq!(
            normalized["authMethods"],
            json!([{"id": "local", "name": "Local"}])
        );
        assert!(normalized.get("id").is_none());
        assert!(normalized.get("_meta").is_none());
    }

    #[test]
    fn malformed_initialize_reply_is_refused_before_fixture_write() {
        let error = normalize_acp_initialize(json!({
            "id": 1,
            "result": {"credential": "API_KEY=private"}
        }))
        .expect_err("expected an absent protocol version to be refused");

        assert!(
            matches!(&error, Error::Protocol { expected, .. } if expected.contains("protocolVersion")),
            "expected the missing protocolVersion field, received {error:?}"
        );
        assert!(!error.to_string().contains("private"), "{error}");
    }

    /// A named fake ACP agent whose adapter cannot start: it answers `initialize` with a JSON-RPC
    /// error frame, as `codex-acp` does when `CODEX_HOME` points at a directory that is absent.
    struct RefusingInitializeAgent {
        code: i64,
        message: &'static str,
    }

    impl RefusingInitializeAgent {
        fn frame(&self) -> serde_json::Value {
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "error": {"code": self.code, "message": self.message, "data": {"detail": "private"}}
            })
        }

        fn process(&self) -> FakeProcess {
            FakeProcess::transcript([self.frame().to_string()]).with_exit(success())
        }
    }

    const CODEX_HOME_REFUSAL: &str = "Codex process has exited with code 1: CODEX_HOME points to \"/nowhere\", but that path does not exist API_KEY=private-token";

    #[test]
    fn initialize_error_frame_names_its_code_and_redacted_message() {
        let agent = RefusingInitializeAgent {
            code: 1001,
            message: CODEX_HOME_REFUSAL,
        };

        let error = normalize_acp_initialize(agent.frame())
            .expect_err("expected an initialize error frame to fail the capture");

        let Error::Vendor(vendor) = &error else {
            panic!(
                "(acp initialize): expected the agent's JSON-RPC error as a vendor failure | received: {error:?}"
            );
        };
        assert_eq!(vendor.vendor_code.as_deref(), Some("1001"));
        assert!(
            vendor.message.contains("CODEX_HOME points to"),
            "(acp initialize): expected the agent's message | received: {}",
            vendor.message
        );
        assert!(
            !vendor.message.contains("private-token"),
            "(acp initialize): expected credential-shaped text redacted | received: {}",
            vendor.message
        );
    }

    #[tokio::test]
    async fn failed_acp_initialize_prints_the_error_code_and_message_an_operator_reads() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(
            RefusingInitializeAgent {
                code: 1001,
                message: CODEX_HOME_REFUSAL,
            }
            .process(),
        );
        let profile =
            mango_agent_acp::builtin_profile("opencode").expect("expected OpenCode profile");

        let error = acp_initialize(&host(Arc::clone(&launcher)), profile.as_ref())
            .await
            .expect_err("expected the agent's error frame to fail the capture");

        let printed = crate::command_failure(&error);
        assert!(
            printed.contains("CODEX_HOME points to") && printed.contains("1001"),
            "(acp initialize): expected the error message and code 1001 | received: {printed}"
        );
        assert!(
            !printed.contains("invalid vendor data") && !printed.contains("private-token"),
            "(acp initialize): expected no generic text and no credential | received: {printed}"
        );
    }

    #[test]
    fn a_null_error_member_beside_a_result_is_no_error() {
        let normalized = normalize_acp_initialize(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"protocolVersion": 1},
            "error": null
        }))
        .unwrap_or_else(|error| {
            panic!(
                "(acp initialize): expected the result of a reply with `\"error\": null` | received: {error:?}"
            )
        });

        assert_eq!(normalized["protocolVersion"], 1);
    }

    #[test]
    fn a_null_error_member_without_a_result_is_refused_for_the_missing_result() {
        let error = normalize_acp_initialize(json!({"id": 1, "error": null}))
            .expect_err("expected a reply with no result to be refused");

        assert!(
            matches!(&error, Error::Protocol { expected, .. } if expected.contains("result object")),
            "(acp initialize): expected the missing-result refusal, not an error shape | received: {error:?}"
        );
    }

    #[test]
    fn initialize_error_member_without_a_code_and_message_is_a_shape_refusal() {
        let error = normalize_acp_initialize(json!({"id": 1, "error": "boom"}))
            .expect_err("expected a malformed error member to be refused");

        assert!(
            matches!(&error, Error::Protocol { expected, received }
                if expected.contains("JSON-RPC error") && received == "string"),
            "(acp initialize): expected a JSON-RPC error shape refusal | received: {error:?}"
        );
    }

    #[test]
    fn surface_is_sorted_and_retains_only_declared_flags() {
        let surface = claude_surface(
            "  --beta  Extra.\n  --alpha  Mentions --invented in prose.\n  --permission-mode <mode>  (choices: \"manual\", \"auto\")\n  --model <model>  (e.g. 'sonnet')\n  --effort <level>  (low, high)",
        );

        assert_eq!(
            surface["flags"],
            json!([
                "--alpha",
                "--beta",
                "--effort",
                "--model",
                "--permission-mode"
            ])
        );
        assert_eq!(surface["permissionModes"], json!(["auto", "manual"]));
        assert_eq!(surface["modelAliases"], json!(["sonnet"]));
        assert_eq!(surface["effortLevels"], json!(["low", "high"]));
    }

    #[tokio::test]
    async fn public_claude_capture_uses_only_version_and_help() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::transcript(["2.1.270 (Claude Code)"]).with_exit(success()));
        launcher.push(
            FakeProcess::transcript([
                "  --print  Print output.",
                "  --input-format <format>  Input.",
                "  --output-format <format>  Output.",
                "  --verbose  Verbose.",
                "  --include-partial-messages  Partial.",
                "  --forward-subagent-text  Forward.",
                "  --permission-mode <mode>  (choices: \"manual\")",
                "  --resume <id>  Resume.",
                "  --session-id <id>  Session.",
                "  --model <model>  (e.g. 'sonnet')",
            ])
            .with_exit(success()),
        );
        let out = temp_dir("claude");

        claude_with(&host(Arc::clone(&launcher)), &out)
            .await
            .expect("expected public Claude contract capture");

        let launches = launcher.launches();
        assert_eq!(launches.len(), 2);
        assert_eq!(launches[0].argv, vec!["claude", "--version"]);
        assert_eq!(launches[1].argv, vec!["claude", "--help"]);
        assert!(!out.join("contract/auth-status.json").exists());
        std::fs::remove_dir_all(out).expect("expected temporary capture cleanup");
    }

    #[test]
    fn codex_help_drops_the_desktop_app_row_that_only_macos_and_windows_list() {
        let printed = "Commands:\n  plugin  Manage plugins\n  acp     A hypothetical native ACP mode\n  app     Launch the Desktop app (opens\n          the installer if missing)\n  completion  Generate scripts\n\nOptions:\n  -c, --config <key=value>\n          Override a value";

        let portable = codex_portable_help(printed);

        assert_eq!(
            portable,
            "Commands:\n  plugin  Manage plugins\n  acp     A hypothetical native ACP mode\n  completion  Generate scripts\n\nOptions:\n  -c, --config <key=value>\n          Override a value",
            "(codex help): expected only the app row and its wrapped line dropped, an unknown subcommand kept | received {portable:?}"
        );
        assert_eq!(
            codex_portable_help("Commands:\n  plugin  Manage plugins"),
            "Commands:\n  plugin  Manage plugins",
            "(codex help): expected a Linux listing to pass through unchanged"
        );
    }

    #[tokio::test]
    async fn public_codex_capture_records_both_help_surfaces_with_digests() {
        let launcher = Arc::new(FakeLauncher::new());
        let version = || {
            FakeProcess::transcript([format!(
                "codex-cli {}",
                mango_agent_codex::MINIMUM_CODEX_VERSION
            )])
            .with_exit(success())
        };
        // The version gate and the contract each ask for the version.
        launcher.push(version());
        launcher.push(version());
        launcher.push(
            FakeProcess::transcript(["Codex CLI", "", "Commands:", "  exec  Run Codex"])
                .with_exit(success()),
        );
        launcher.push(
            FakeProcess::transcript(["[experimental] Run the app server", "  --listen <URL>"])
                .with_exit(success()),
        );
        launcher.push(FakeProcess::transcript(["generated"]).with_exit(success()));
        let out = temp_dir("codex-out");
        let workspace = temp_dir("codex-workspace");
        // The fake generator writes nothing, so the schemas it would have produced are placed.
        let generated = workspace.join("codex-schema/v1");
        std::fs::create_dir_all(&generated).expect("expected a generated schema directory");
        for name in ["InitializeParams.json", "InitializeResponse.json"] {
            std::fs::write(generated.join(name), "{}\n").expect("expected a generated schema");
        }

        codex_contract_with(&host(Arc::clone(&launcher)), &out, &workspace)
            .await
            .expect("expected public Codex contract capture");

        let launched: Vec<String> = launcher
            .launches()
            .iter()
            .map(|launch| launch.argv[1..].join(" "))
            .collect();
        let schema_argv = format!(
            "app-server generate-json-schema --out {}",
            workspace.join("codex-schema").to_string_lossy()
        );
        assert_eq!(
            launched,
            [
                "--version",
                "--version",
                "--help",
                "app-server --help",
                schema_argv.as_str()
            ],
            "(codex capture): expected version, version, help, app-server help, schema | received {launched:?}"
        );
        let contract = out.join("contract");
        assert_eq!(
            std::fs::read_to_string(contract.join("help.txt")).expect("expected help.txt"),
            "Codex CLI\n\nCommands:\n  exec  Run Codex\n"
        );
        assert_eq!(
            std::fs::read_to_string(contract.join("app-server-help.txt"))
                .expect("expected app-server-help.txt"),
            "[experimental] Run the app server\n  --listen <URL>\n"
        );
        let manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(contract.join("manifest.json")).expect("expected manifest"),
        )
        .expect("expected manifest JSON");
        assert_eq!(
            manifest["probes"],
            json!([
                "codex --version",
                "codex --help",
                "codex app-server --help",
                "codex app-server generate-json-schema --out <workspace>/codex-schema"
            ])
        );
        for file in ["help.txt", "app-server-help.txt"] {
            assert!(
                manifest["files"][file]
                    .as_str()
                    .is_some_and(|digest| digest.starts_with("sha256:")),
                "(codex manifest): expected a sha256 digest for {file} | received {:?}",
                manifest["files"][file]
            );
        }
        super::manifest::verify(&contract.join("manifest.json"))
            .expect("expected the written digests to verify");
        assert_eq!(launcher.live_children(), 0);
        std::fs::remove_dir_all(out).expect("expected temporary capture cleanup");
        std::fs::remove_dir_all(workspace).expect("expected temporary workspace cleanup");
    }

    #[tokio::test]
    async fn acp_capture_sends_only_initialize_and_reaps_the_agent() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::transcript(["1.18.30"]).with_exit(success()));
        launcher.push(FakeProcess::responding(|line| {
            if line.contains("\"method\":\"initialize\"") {
                return vec![String::from(
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true},"authMethods":[{"id":"local","name":"Local"}]}}"#,
                )];
            }
            Vec::new()
        }));
        let out = temp_dir("acp");
        let profile =
            mango_agent_acp::builtin_profile("opencode").expect("expected OpenCode profile");

        acp_with(
            &host(Arc::clone(&launcher)),
            &out,
            profile.as_ref(),
            None,
            None,
        )
        .await
        .expect("expected an ACP public contract capture");

        assert_eq!(launcher.launches()[0].argv, vec!["opencode", "--version"]);
        let writes = launcher.written();
        assert_eq!(writes.len(), 1);
        assert!(writes[0].contains("\"method\":\"initialize\""));
        assert!(!writes[0].contains("authenticate"));
        assert!(!writes[0].contains("session/new"));
        assert_eq!(launcher.live_children(), 0);
        assert!(
            !out.join("contract/session-new.json").exists(),
            "expected no session-new.json from a capture that did not opt in"
        );
        let manifest = read_manifest(&out);
        assert_eq!(manifest["probe"], "initialize");
        assert_eq!(manifest["sessionOpened"], false);
        std::fs::remove_dir_all(out).expect("expected temporary capture cleanup");
    }

    fn read_manifest(out: &std::path::Path) -> serde_json::Value {
        let text = std::fs::read_to_string(out.join("contract/manifest.json"))
            .expect("expected a manifest");
        serde_json::from_str(&text).expect("expected manifest.json to be JSON")
    }

    /// A named fake ACP adapter: it answers `initialize`, then `session/new` either with an opened
    /// session (modes and a model catalog beside a per-run session id) or with the login refusal
    /// `codex-acp` sends without an account. A request it does not know fails the test by name.
    struct SessionAgent {
        session_reply: &'static str,
    }

    const OPENED_SESSION_REPLY: &str = r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"per-run-id","modes":{"currentModeId":"auto","availableModes":[{"id":"default","name":"Manual","description":"Always ask"},{"id":"auto","name":"Auto"}]},"configOptions":[{"id":"model","name":"Model","category":"model","type":"select","currentValue":"default","options":[{"value":"default","name":"Default","description":"$4/$20 per Mtok"}]}]}}"#;
    const LOGIN_REFUSAL_REPLY: &str =
        r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32000,"message":"Authentication required"}}"#;

    impl SessionAgent {
        fn process(&self) -> FakeProcess {
            let session_reply = self.session_reply;
            FakeProcess::responding(move |line| {
                if line.contains("\"method\":\"initialize\"") {
                    return vec![String::from(
                        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentInfo":{"name":"fake-adapter","version":"1.0.0"}}}"#,
                    )];
                }
                if line.contains("\"method\":\"session/new\"") {
                    return vec![String::from(session_reply)];
                }
                panic!("(session agent): expected initialize or session/new | received: {line}");
            })
        }
    }

    fn session_capture_launcher(agent: &SessionAgent) -> Arc<FakeLauncher> {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::transcript(["fake-adapter 1.0.0"]).with_exit(success()));
        launcher.push(agent.process());
        launcher
    }

    fn maintainer_profile() -> std::sync::Arc<mango_agent_acp::AcpProfile> {
        mango_agent_acp::builtin_profile("claude-agent-acp")
            .expect("expected the claude-agent-acp profile")
    }

    #[tokio::test]
    async fn open_session_records_mode_ids_and_the_model_catalog_without_authenticating() {
        let launcher = session_capture_launcher(&SessionAgent {
            session_reply: OPENED_SESSION_REPLY,
        });
        let out = temp_dir("acp-session");
        let workspace = temp_dir("acp-session-workspace");
        let profile = maintainer_profile();

        acp_with(
            &host(Arc::clone(&launcher)),
            &out,
            profile.as_ref(),
            None,
            Some(&workspace),
        )
        .await
        .expect("expected a capture that opens a session");

        let writes = launcher.written();
        assert_eq!(
            writes.len(),
            2,
            "expected initialize then session/new | received {writes:?}"
        );
        assert!(writes[0].contains("\"method\":\"initialize\""));
        assert!(writes[1].contains("\"method\":\"session/new\""));
        let cwd = serde_json::to_string(&workspace).expect("expected a cwd string");
        assert!(
            writes[1].contains(&format!("\"cwd\":{cwd}"))
                && writes[1].contains("\"mcpServers\":[]"),
            "expected session/new to carry the workspace cwd and no MCP servers | received {}",
            writes[1]
        );
        assert!(
            writes.iter().all(|write| !write.contains("authenticate")),
            "expected no authenticate request | received {writes:?}"
        );
        let session: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(out.join("contract/session-new.json"))
                .expect("expected session-new.json"),
        )
        .expect("expected session-new.json to be JSON");
        assert_eq!(session["result"]["modes"]["currentModeId"], "auto");
        assert_eq!(
            session["result"]["configOptions"][0]["options"][0],
            json!({"value": "default", "name": "Default"})
        );
        let text = session.to_string();
        assert!(
            !text.contains("per-run-id") && !text.contains("$4/$20"),
            "expected the session id and the prices left out | received {text}"
        );
        let manifest = read_manifest(&out);
        assert_eq!(manifest["probe"], "initialize, session/new");
        assert_eq!(manifest["sessionOpened"], true);
        assert_eq!(manifest["reproducible"], false);
        assert!(
            manifest["files"].get("session-new.json").is_some(),
            "expected a digest for session-new.json | received {manifest}"
        );
        super::manifest::verify(&out.join("contract/manifest.json"))
            .expect("expected the written digests to verify");
        assert_eq!(launcher.live_children(), 0);
        std::fs::remove_dir_all(out).expect("expected temporary capture cleanup");
        std::fs::remove_dir_all(workspace).expect("expected temporary workspace cleanup");
    }

    #[tokio::test]
    async fn open_session_records_a_login_refusal_and_says_no_session_opened() {
        let launcher = session_capture_launcher(&SessionAgent {
            session_reply: LOGIN_REFUSAL_REPLY,
        });
        let out = temp_dir("acp-session-refused");
        let workspace = temp_dir("acp-session-refused-workspace");
        let profile = maintainer_profile();

        acp_with(
            &host(Arc::clone(&launcher)),
            &out,
            profile.as_ref(),
            None,
            Some(&workspace),
        )
        .await
        .expect("expected a login refusal to be a recorded answer");

        let session: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(out.join("contract/session-new.json"))
                .expect("expected session-new.json"),
        )
        .expect("expected session-new.json to be JSON");
        assert_eq!(
            session,
            json!({"error": {"code": -32000, "message": "Authentication required"}})
        );
        let manifest = read_manifest(&out);
        assert_eq!(manifest["probe"], "initialize, session/new");
        assert_eq!(manifest["sessionOpened"], false);
        assert_eq!(launcher.live_children(), 0);
        std::fs::remove_dir_all(out).expect("expected temporary capture cleanup");
        std::fs::remove_dir_all(workspace).expect("expected temporary workspace cleanup");
    }

    #[tokio::test]
    async fn a_refused_initialize_sends_no_session_new() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::transcript(["fake-adapter 1.0.0"]).with_exit(success()));
        launcher.push(
            RefusingInitializeAgent {
                code: 1001,
                message: "cannot start",
            }
            .process(),
        );
        let out = temp_dir("acp-session-no-initialize");
        let workspace = temp_dir("acp-session-no-initialize-workspace");
        let profile = maintainer_profile();

        acp_with(
            &host(Arc::clone(&launcher)),
            &out,
            profile.as_ref(),
            None,
            Some(&workspace),
        )
        .await
        .expect_err("expected the initialize refusal to fail the capture");

        let writes = launcher.written();
        assert_eq!(
            writes.len(),
            1,
            "expected only initialize on the wire | received {writes:?}"
        );
        assert!(!out.join("contract").exists(), "expected no fixture file");
        assert_eq!(launcher.live_children(), 0);
        std::fs::remove_dir_all(out).expect("expected temporary capture cleanup");
        std::fs::remove_dir_all(workspace).expect("expected temporary workspace cleanup");
    }

    #[tokio::test]
    async fn open_session_is_refused_for_a_profile_the_drift_lane_records_again() {
        let launcher = Arc::new(FakeLauncher::new());
        let out = temp_dir("acp-session-reproducible");
        let workspace = temp_dir("acp-session-reproducible-workspace");
        let profile =
            mango_agent_acp::builtin_profile("opencode").expect("expected the opencode profile");

        let refused = acp_with(
            &host(Arc::clone(&launcher)),
            &out,
            profile.as_ref(),
            None,
            Some(&workspace),
        )
        .await
        .expect_err("expected --open-session to be refused for a reproducible profile");

        let message = refused.to_string();
        assert!(
            message.contains("--open-session") && message.contains("opencode"),
            "expected the refusal to name the option and the profile | received {message}"
        );
        assert!(
            launcher.launches().is_empty(),
            "expected no process to start for a refused option"
        );
        std::fs::remove_dir_all(out).expect("expected temporary capture cleanup");
        std::fs::remove_dir_all(workspace).expect("expected temporary workspace cleanup");
    }

    #[tokio::test]
    async fn open_session_needs_an_absolute_workspace_for_the_protocol_cwd() {
        let launcher = Arc::new(FakeLauncher::new());
        let out = temp_dir("acp-session-relative");
        let profile = maintainer_profile();

        let refused = acp_with(
            &host(Arc::clone(&launcher)),
            &out,
            profile.as_ref(),
            None,
            Some(std::path::Path::new("relative/workspace")),
        )
        .await
        .expect_err("expected a relative workspace to be refused");

        let message = refused.to_string();
        assert!(
            message.contains("absolute") && message.contains("relative/workspace"),
            "expected the refusal to name the received path | received {message}"
        );
        assert!(
            launcher.launches().is_empty(),
            "expected no process to start"
        );
        std::fs::remove_dir_all(out).expect("expected temporary capture cleanup");
    }

    /// A named fake of an operator's process environment on a logged-in machine: every location an
    /// agent resolves its account or state from points at the operator's own directories.
    fn logged_in_operator_environment() -> EnvSource {
        EnvSource::from_pairs([
            ("PATH", "/fake/bin"),
            ("HOME", "/real/operator/home"),
            ("XDG_CONFIG_HOME", "/real/operator/xdg/config"),
            ("XDG_DATA_HOME", "/real/operator/xdg/data"),
            ("XDG_STATE_HOME", "/real/operator/xdg/state"),
            ("XDG_CACHE_HOME", "/real/operator/xdg/cache"),
            ("CLAUDE_CONFIG_DIR", "/real/operator/claude"),
            ("CODEX_HOME", "/real/operator/codex"),
        ])
    }

    /// The scratch home an `--open-session` child was given, read back from its environment.
    fn scratch_home_of(launch: &LaunchSpec) -> std::path::PathBuf {
        let home = launch.env.get("HOME").unwrap_or_else(|| {
            panic!(
                "expected HOME in the child environment | received {:?}",
                launch.env.keys().collect::<Vec<_>>()
            )
        });
        std::path::Path::new(home)
            .parent()
            .expect("expected the scratch HOME to sit inside a scratch root")
            .to_owned()
    }

    fn assert_child_saw_only_scratch_locations(launches: &[LaunchSpec]) -> std::path::PathBuf {
        assert!(
            !launches.is_empty(),
            "expected the capture to launch the adapter"
        );
        let root = scratch_home_of(&launches[0]);
        for launch in launches {
            for (key, value) in &launch.env {
                assert!(
                    !value.starts_with("/real/operator"),
                    "(child env {:?}): expected no value from the operator's own directories | received {key}={value}",
                    launch.program()
                );
            }
            for key in [
                "HOME",
                "XDG_CONFIG_HOME",
                "XDG_DATA_HOME",
                "XDG_STATE_HOME",
                "XDG_CACHE_HOME",
            ] {
                let value = launch.env.get(key).unwrap_or_else(|| {
                    panic!("(child env {:?}): expected {key} set to a scratch directory | received none", launch.program())
                });
                assert!(
                    std::path::Path::new(value).starts_with(&root),
                    "(child env {:?}): expected {key} inside {root:?} | received {value}",
                    launch.program()
                );
            }
        }
        root
    }

    #[tokio::test]
    async fn open_session_runs_the_adapter_under_a_scratch_home_never_the_operators() {
        for (profile_id, vendor_key) in [
            ("claude-agent-acp", "CLAUDE_CONFIG_DIR"),
            ("codex-acp", "CODEX_HOME"),
        ] {
            let launcher = session_capture_launcher(&SessionAgent {
                session_reply: OPENED_SESSION_REPLY,
            });
            let out = temp_dir("acp-session-scratch-home");
            let workspace = temp_dir("acp-session-scratch-home-workspace");

            super::acp_under(
                launcher.clone(),
                logged_in_operator_environment(),
                &out,
                &workspace,
                profile_id,
                None,
                true,
            )
            .await
            .expect("expected a capture that opens a session");

            let launches = launcher.launches();
            let root = assert_child_saw_only_scratch_locations(&launches);
            let adapter = launches.last().expect("expected the adapter launch");
            let vendor_home = adapter.env.get(vendor_key).unwrap_or_else(|| {
                panic!("({profile_id}): expected {vendor_key} set for the adapter | received none")
            });
            assert!(
                std::path::Path::new(vendor_home).starts_with(&root),
                "({profile_id}): expected {vendor_key} inside {root:?} | received {vendor_home}"
            );
            assert_eq!(
                adapter.env.get("PATH").map(String::as_str),
                Some("/fake/bin")
            );
            assert!(
                !root.exists(),
                "({profile_id}): expected the scratch home {root:?} removed after the capture"
            );
            std::fs::remove_dir_all(out).expect("expected temporary capture cleanup");
            std::fs::remove_dir_all(workspace).expect("expected temporary workspace cleanup");
        }
    }

    #[tokio::test]
    async fn open_session_removes_the_scratch_home_when_the_capture_fails() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::transcript(["fake-adapter 1.0.0"]).with_exit(success()));
        launcher.push(
            RefusingInitializeAgent {
                code: 1001,
                message: "cannot start",
            }
            .process(),
        );
        let out = temp_dir("acp-session-scratch-home-error");
        let workspace = temp_dir("acp-session-scratch-home-error-workspace");

        super::acp_under(
            launcher.clone(),
            logged_in_operator_environment(),
            &out,
            &workspace,
            "claude-agent-acp",
            None,
            true,
        )
        .await
        .expect_err("expected the initialize refusal to fail the capture");

        let root = assert_child_saw_only_scratch_locations(&launcher.launches());
        assert!(
            !root.exists(),
            "expected the scratch home {root:?} removed after a failed capture"
        );
        std::fs::remove_dir_all(out).expect("expected temporary capture cleanup");
        std::fs::remove_dir_all(workspace).expect("expected temporary workspace cleanup");
    }

    #[tokio::test]
    async fn a_capture_without_open_session_keeps_the_operators_environment() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::transcript(["1.18.30"]).with_exit(success()));
        launcher.push(FakeProcess::responding(|line| {
            if line.contains("\"method\":\"initialize\"") {
                return vec![String::from(
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#,
                )];
            }
            Vec::new()
        }));
        let out = temp_dir("acp-no-session-env");
        let workspace = temp_dir("acp-no-session-env-workspace");

        super::acp_under(
            launcher.clone(),
            logged_in_operator_environment(),
            &out,
            &workspace,
            "opencode",
            None,
            false,
        )
        .await
        .expect("expected an initialize-only capture");

        assert_eq!(
            launcher.launches()[0].env.get("HOME").map(String::as_str),
            Some("/real/operator/home"),
            "expected the reproducible capture, which opens no session, to run as before"
        );
        std::fs::remove_dir_all(out).expect("expected temporary capture cleanup");
        std::fs::remove_dir_all(workspace).expect("expected temporary workspace cleanup");
    }

    /// A fake launcher that answers the adapter version, the named vendor version, then `initialize`.
    fn adapter_over_vendor_launcher(vendor_stdout: &[&str]) -> Arc<FakeLauncher> {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(
            FakeProcess::transcript(["@agentclientprotocol/codex-acp 2.6.0"]).with_exit(success()),
        );
        launcher.push(
            FakeProcess::transcript(vendor_stdout.iter().copied().map(String::from))
                .with_exit(success()),
        );
        launcher.push(initialize_responder());
        launcher
    }

    fn initialize_responder() -> FakeProcess {
        FakeProcess::responding(|line| {
            if line.contains("\"method\":\"initialize\"") {
                return vec![String::from(
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}"#,
                )];
            }
            Vec::new()
        })
    }

    fn named_vendor_cli() -> String {
        std::env::temp_dir()
            .join("mea-vendor-install")
            .join("codex")
            .to_string_lossy()
            .into_owned()
    }

    fn read_version_file(out: &std::path::Path) -> serde_json::Value {
        let text = std::fs::read_to_string(out.join("contract/version.json"))
            .expect("expected a version file");
        serde_json::from_str(&text).expect("expected version.json to be JSON")
    }

    #[tokio::test]
    async fn named_vendor_cli_version_is_recorded_beside_the_adapter_version() {
        let launcher = adapter_over_vendor_launcher(&["codex-cli 0.166.0"]);
        let out = temp_dir("acp-vendor");
        let profile = mango_agent_acp::builtin_profile("codex-acp").expect("expected a profile");
        let vendor_cli = named_vendor_cli();

        acp_with(
            &host(Arc::clone(&launcher)),
            &out,
            profile.as_ref(),
            Some(&vendor_cli),
            None,
        )
        .await
        .expect("expected a capture that names the vendor CLI");

        let launches = launcher.launches();
        assert_eq!(
            launches[1].argv,
            vec![vendor_cli.clone(), String::from("--version")],
            "expected the second launch to be the named vendor CLI's --version"
        );
        let recorded = read_version_file(&out);
        assert_eq!(recorded["output"], "@agentclientprotocol/codex-acp 2.6.0");
        assert_eq!(
            recorded["vendorCli"],
            json!({
                "source": "named by the operator, not located by mea",
                "command": ["codex", "--version"],
                "output": "codex-cli 0.166.0",
            }),
            "expected the vendor CLI beneath the adapter in version.json, by file name only"
        );
        assert!(
            !recorded.to_string().contains("mea-vendor-install"),
            "expected no machine path in version.json | received {recorded}"
        );
        let manifest = std::fs::read_to_string(out.join("contract/manifest.json"))
            .expect("expected a manifest");
        assert!(
            manifest.contains("2.6.0") && !manifest.contains("0.166.0"),
            "expected capturedFrom to stay the adapter line | received {manifest}"
        );
        assert_eq!(launcher.live_children(), 0);
        std::fs::remove_dir_all(out).expect("expected temporary capture cleanup");
    }

    #[tokio::test]
    async fn version_file_without_a_named_vendor_cli_keeps_its_two_members() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::transcript(["1.18.30"]).with_exit(success()));
        launcher.push(initialize_responder());
        let out = temp_dir("acp-no-vendor");
        let profile = mango_agent_acp::builtin_profile("opencode").expect("expected a profile");

        acp_with(&host(launcher), &out, profile.as_ref(), None, None)
            .await
            .expect("expected a capture without a vendor CLI");

        let recorded = read_version_file(&out);
        let members: Vec<&str> = recorded
            .as_object()
            .expect("expected an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            members,
            ["command", "output"],
            "expected version.json members command, output | received {recorded}"
        );
        std::fs::remove_dir_all(out).expect("expected temporary capture cleanup");
    }

    #[tokio::test]
    async fn unusable_vendor_cli_answers_write_no_fixture_and_name_the_program() {
        let vendor_cli = named_vendor_cli();
        for (stdout, expected) in [
            (vec![], "a public probe which writes stdout"),
            (vec!["codex-cli 0.166.0", "extra line"], "2 lines"),
        ] {
            let launcher = adapter_over_vendor_launcher(&stdout);
            let out = temp_dir("acp-vendor-refused");
            let profile =
                mango_agent_acp::builtin_profile("codex-acp").expect("expected a profile");

            let refused = acp_with(
                &host(Arc::clone(&launcher)),
                &out,
                profile.as_ref(),
                Some(&vendor_cli),
                None,
            )
            .await
            .expect_err("expected an unusable vendor answer to be refused");

            let message = refused.to_string();
            // The refusal quotes the program with `{:?}`, which doubles the backslashes of a
            // Windows temp path, so the expected text is quoted the same way.
            let quoted = format!("{vendor_cli:?}");
            assert!(
                message.contains(&quoted) && message.contains(expected),
                "expected an error naming {quoted} and {expected:?} | received {message}"
            );
            assert!(
                !out.join("contract").exists(),
                "expected no fixture file after a refused vendor probe"
            );
            assert_eq!(launcher.live_children(), 0);
            std::fs::remove_dir_all(out).expect("expected temporary capture cleanup");
        }
    }

    #[test]
    fn vendor_cli_without_a_file_name_is_refused() {
        let refused = vendor_cli_command("..").expect_err("expected a nameless program refused");

        assert!(
            refused.to_string().contains("\"..\""),
            "expected the refusal to name the received value | received {refused}"
        );
    }

    #[tokio::test]
    async fn command_output_refuses_empty_stdout() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::transcript(Vec::<String>::new()).with_exit(success()));

        let error = command_output(
            &host(launcher),
            vec![String::from("example"), String::from("--version")],
            &[],
        )
        .await
        .expect_err("expected an empty probe to be refused");

        assert!(
            matches!(&error, Error::Protocol { expected, .. } if expected.contains("stdout")),
            "expected empty stdout to be refused, received {error:?}"
        );
    }

    #[tokio::test]
    async fn successful_public_command_requires_confirmed_tree_cleanup() {
        assert_public_capture_owner(false, false).await;
    }

    #[tokio::test]
    async fn failed_public_command_retains_cleanup_ownership() {
        assert_public_capture_owner(false, true).await;
    }

    #[tokio::test]
    async fn successful_acp_initialize_retains_cleanup_ownership() {
        assert_public_capture_owner(true, false).await;
    }

    #[tokio::test]
    async fn failed_acp_initialize_retains_cleanup_ownership() {
        assert_public_capture_owner(true, true).await;
    }

    async fn assert_public_capture_owner(acp: bool, failed: bool) {
        let process = if acp {
            if failed {
                FakeProcess::transcript(Vec::<String>::new())
            } else {
                FakeProcess::transcript([
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{},"authMethods":[]}}"#,
                ])
            }
        } else {
            FakeProcess::transcript([if failed { "oversized-line" } else { "v" }])
        }.with_exit(success());
        let launcher = CaptureCleanupLauncher::with_process(
            CaptureStop::Refuse(
                Error::Closed {
                    subject: "public capture stop",
                }
                .with_dispatch(Dispatch::Accepted),
            ),
            process,
        );
        let mut limits = Limits::default();
        if !acp && failed {
            limits.line.max_line_bytes = 4;
            limits.line.max_buffered_bytes = 8;
        }
        let host = capture_host_with_limits(launcher.clone(), limits);
        let result = if acp {
            let profile = mango_agent_acp::builtin_profile("opencode").expect("profile");
            acp_initialize(&host, &profile).await.map(|_| ())
        } else {
            super::command_lines(
                &host,
                vec![String::from("fixture"), String::from("--version")],
                &[],
            )
            .await
            .map(|_| ())
        };
        let control = launcher.control();
        control
            .inner
            .kill(CancelReason::Shutdown)
            .await
            .expect("test fake cleanup");
        assert_eq!(
            control.calls.load(Ordering::SeqCst),
            1,
            "public capture must confirm process-tree cleanup even after successful leader exit"
        );
        let error = result.expect_err("expected unconfirmed public capture cleanup");
        let retained = error
            .cleanup_control()
            .expect("public capture must retain recovery control");
        let expected: Arc<dyn ProcessControl> = control;
        assert!(Arc::ptr_eq(&retained, &expected));
        assert_eq!(error.dispatch(), Dispatch::Accepted);
        assert!(matches!(
            error.cause(),
            Error::Closed {
                subject: "public capture stop"
            }
        ));
        assert_eq!(launcher.inner.live_children(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn public_command_cleanup_obeys_host_stop_limits() {
        assert_public_capture_stop_bound(false).await;
    }

    #[tokio::test(start_paused = true)]
    async fn acp_initialize_cleanup_obeys_host_stop_limits() {
        assert_public_capture_stop_bound(true).await;
    }

    async fn assert_public_capture_stop_bound(acp: bool) {
        let process = FakeProcess::transcript(["oversized-line"]).with_exit(success());
        let launcher = CaptureCleanupLauncher::with_process(CaptureStop::Hold, process);
        let mut limits = Limits {
            kill_grace: std::time::Duration::from_millis(1),
            shutdown_timeout: std::time::Duration::from_millis(2),
            ..Limits::default()
        };
        limits.line.max_line_bytes = 4;
        limits.line.max_buffered_bytes = 8;
        let host = capture_host_with_limits(launcher.clone(), limits);
        let capture = async {
            if acp {
                let profile = mango_agent_acp::builtin_profile("opencode").expect("profile");
                acp_initialize(&host, &profile).await.map(|_| ())
            } else {
                super::command_lines(&host, vec![String::from("fixture")], &[])
                    .await
                    .map(|_| ())
            }
        };
        let result = tokio::time::timeout(std::time::Duration::from_millis(20), capture).await;
        let control = launcher.control();
        control
            .inner
            .kill(CancelReason::Shutdown)
            .await
            .expect("test fake cleanup");
        assert_eq!(control.calls.load(Ordering::SeqCst), 1);
        let error = result
            .expect("public capture stop must obey the host budget")
            .expect_err("expected recoverable cleanup timeout");
        let retained = error.cleanup_control().expect("timeout recovery control");
        let expected: Arc<dyn ProcessControl> = control;
        assert!(Arc::ptr_eq(&retained, &expected));
        assert!(matches!(error.cause(), Error::Timeout { .. }));
        assert_eq!(launcher.inner.live_children(), 0);
    }

    fn no_contract_replies(_: &str) -> Vec<String> {
        Vec::new()
    }

    /// Models a command that keeps running after its stdin closes, independently of pipe EOF.
    struct IndependentEofLauncher(Arc<CaptureCleanupLauncher>);

    #[async_trait::async_trait]
    impl ProcessLauncher for IndependentEofLauncher {
        async fn spawn(&self, spec: LaunchSpec) -> Result<ManagedProcess> {
            let mut child = self.0.spawn(spec).await?;
            child.stdin = Some(Box::new(IndependentEofInput {
                inner: child.stdin.take().expect("fake input"),
                closed: false,
            }));
            Ok(child)
        }
    }

    struct IndependentEofInput {
        inner: Box<dyn ByteSink>,
        closed: bool,
    }

    #[async_trait::async_trait]
    impl ByteSink for IndependentEofInput {
        async fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
            if self.closed {
                return Err(Error::Closed {
                    subject: "fake command input",
                });
            }
            self.inner.write_all(bytes).await
        }

        async fn close(&mut self) -> Result<()> {
            self.closed = true;
            Ok(())
        }
    }

    #[tokio::test]
    async fn cancelling_public_command_stops_its_owned_child() {
        assert_public_capture_cancellation(false).await;
    }

    #[tokio::test]
    async fn cancelling_acp_initialize_stops_its_owned_child() {
        assert_public_capture_cancellation(true).await;
    }

    async fn assert_public_capture_cancellation(acp: bool) {
        let launcher = CaptureCleanupLauncher::with_process(
            CaptureStop::End,
            FakeProcess::responding(no_contract_replies),
        );
        let port: Arc<dyn ProcessLauncher> = if acp {
            launcher.clone()
        } else {
            Arc::new(IndependentEofLauncher(launcher.clone()))
        };
        let host = capture_host_with_limits(port, Limits::default());
        let capture = tokio::spawn(async move {
            if acp {
                let profile = mango_agent_acp::builtin_profile("opencode").expect("profile");
                acp_initialize(&host, &profile).await.map(|_| ())
            } else {
                super::command_lines(&host, vec![String::from("fixture")], &[])
                    .await
                    .map(|_| ())
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), launcher.reads.notified())
            .await
            .expect("expected an observed public capture read");
        assert!(!capture.is_finished());
        let control = launcher.control();
        capture.abort();
        assert!(
            capture
                .await
                .expect_err("expected cancellation")
                .is_cancelled()
        );
        let stopped = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            control.finished.notified(),
        )
        .await;
        let calls = control.calls.load(Ordering::SeqCst);
        let live = launcher.inner.live_children();
        control
            .inner
            .kill(CancelReason::Shutdown)
            .await
            .expect("test fake cleanup");
        assert!(
            stopped.is_ok(),
            "cancelled public capture must stop its owned child"
        );
        assert_eq!(calls, 1);
        assert_eq!(live, 0);
        assert_eq!(launcher.inner.live_children(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_acp_initialize_reaps_the_named_fake_agent() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::responding(|_| Vec::new()));
        let profile =
            mango_agent_acp::builtin_profile("opencode").expect("expected OpenCode profile");
        let host = host(Arc::clone(&launcher));
        let capture = tokio::spawn(async move { acp_initialize(&host, profile.as_ref()).await });

        launcher.wait_for_written(1).await;
        tokio::time::advance(CONTRACT_TIMEOUT).await;
        let error = capture
            .await
            .expect("expected the capture task to finish")
            .expect_err("expected a silent ACP agent to time out");

        assert!(
            matches!(&error, Error::Timeout { operation, .. } if operation.contains("initialize")),
            "expected initialize to time out, received {error:?}"
        );
        assert_eq!(launcher.live_children(), 0, "expected timeout cleanup");
    }

    #[test]
    fn fixture_writers_keep_json_stable_and_copy_the_generated_schema() {
        let root = temp_dir("writer");
        let source = root.join("generated.json");
        std::fs::write(&source, "{\"title\":\"Initialize\"}\n").expect("expected source schema");
        let target = root.join("contract/schema.json");

        copy_generated_schema(&source, &target).expect("expected schema copy");
        write_json(
            &root.join("contract/manifest.json"),
            &json!({"z": 1, "a": 2}),
        )
        .expect("expected JSON fixture write");

        assert_eq!(
            std::fs::read_to_string(target).expect("expected copied schema"),
            "{\"title\":\"Initialize\"}\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("contract/manifest.json"))
                .expect("expected manifest"),
            "{\n  \"z\": 1,\n  \"a\": 2\n}\n"
        );
        std::fs::remove_dir_all(root).expect("expected temporary capture cleanup");
    }

    #[test]
    fn command_name_keeps_capture_errors_free_of_arguments() {
        let name = command_name(&[
            String::from("/opt/vendor/codex"),
            String::from("--api-key"),
            String::from("capture-argv-secret"),
        ]);

        assert_eq!(name, "codex");
        assert!(
            !name.contains("capture-argv-secret"),
            "expected no command argument in a capture diagnostic, received {name}"
        );
        assert_eq!(CONTRACT_TIMEOUT, std::time::Duration::from_secs(30));
    }

    fn success() -> ExitStatus {
        ExitStatus {
            code: Some(0),
            signal: None,
        }
    }
}
