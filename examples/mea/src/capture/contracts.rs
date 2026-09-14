//! Public, reproducible vendor contracts for the drift check.
//!
//! Transcript capture records a real conversation, so an account, a model, and a vendor release
//! can all change its bytes. This module records only commands that need no login and the ACP
//! handshake that opens no session. The resulting files are safe to regenerate in CI.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mango_external_agents::error::{Error, Result};
use mango_external_agents::launcher::TokioLauncher;
use mango_external_agents::transports::stdio;
use mango_external_agents::{CancelReason, EnvSource, ExecutablePath, HostContext, StdioSpec};
use serde_json::{Map, Value, json};

/// The built-in ACP profile that the drift workflow installs on every operating system.
pub const DEFAULT_ACP_CAPTURE_PROFILE: &str = "opencode";

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
/// This operation only reads the installed CLI's version and invokes the documented schema
/// generator in `workspace/codex-schema`.
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
/// The command sends no `authenticate` or `session/new` request. `profile_id` must name a profile
/// compiled into `mango-agent-acp`.
///
/// # Examples
///
/// `mea capture --harness acp --profile opencode --out fixtures/acp/opencode --workspace /tmp/mea-capture`
///
/// # Errors
///
/// Returns an error for an unknown profile, an unsuccessful probe, or a malformed JSON-RPC answer.
pub async fn acp(out_dir: &Path, workspace: &Path, profile_id: &str) -> Result<()> {
    let profile =
        mango_agent_acp::builtin_profile(profile_id).ok_or_else(|| Error::HostConfiguration {
            expected: "a built-in ACP profile",
            received: profile_id.to_owned(),
        })?;
    acp_with(&capture_host(workspace)?, out_dir, profile.as_ref()).await
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
    write_json(
        &contract_dir(out_dir).join("manifest.json"),
        &json!({
            "format": 1,
            "probes": ["claude --version", "claude --help"],
            "reproducible": true,
        }),
    )
}

async fn codex_contract_with(host: &HostContext, out_dir: &Path, workspace: &Path) -> Result<()> {
    let version = command_output(
        host,
        vec![String::from("codex"), String::from("--version")],
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
    write_json(
        &contract.join("manifest.json"),
        &json!({
            "format": 1,
            "probes": [
                "codex --version",
                "codex app-server generate-json-schema --out <workspace>/codex-schema"
            ],
            "reproducible": true,
            "schemaFiles": ["initialize-params.schema.json", "initialize-response.schema.json"],
        }),
    )
}

async fn acp_with(
    host: &HostContext,
    out_dir: &Path,
    profile: &mango_agent_acp::AcpProfile,
) -> Result<()> {
    let version = command_output(
        host,
        profile.resolved_version_argv(&ExecutablePath::default()),
        profile.vendor_environment_keys,
    )
    .await?;
    let response = acp_initialize(host, profile).await?;
    let contract = contract_dir(out_dir);
    write_json(&contract.join("initialize.json"), &response)?;
    write_json(
        &contract.join("version.json"),
        &json!({
            "command": profile.resolved_version_argv(&ExecutablePath::default()),
            "output": version,
        }),
    )?;
    write_json(
        &contract.join("manifest.json"),
        &json!({
            "format": 1,
            "profile": profile.id.as_str(),
            "probe": "initialize",
            "reproducible": true,
            "sessionOpened": false,
        }),
    )
}

fn capture_host(workspace: &Path) -> Result<HostContext> {
    HostContext::builder()
        .launcher(Arc::new(TokioLauncher::new()))
        .cwd(workspace.to_path_buf())
        .environment(EnvSource::from_process())
        .client_info("mea", env!("CARGO_PKG_VERSION"))
        .build()
}

async fn command_output(
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
    let control = Arc::clone(&transport.control);
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
    if outcome.is_err() {
        let _ = control.kill(CancelReason::Shutdown).await;
    }
    outcome
}

async fn acp_initialize(
    host: &HostContext,
    profile: &mango_agent_acp::AcpProfile,
) -> Result<Value> {
    let argv = profile.resolved_argv(&ExecutablePath::default());
    let transport = stdio::open(
        host,
        &StdioSpec::new(argv),
        &ExecutablePath::default(),
        profile.vendor_environment_keys,
    )
    .await?;
    let control = Arc::clone(&transport.control);
    let (mut sender, mut receiver) = transport.link.split();
    let outcome = async {
        sender
            .send(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": 1,
                        "clientCapabilities": {"fs": {}, "terminal": false},
                        "clientInfo": {"name": "mea", "version": env!("CARGO_PKG_VERSION")},
                    },
                })
                .to_string(),
            )
            .await?;
        tokio::time::timeout(CONTRACT_TIMEOUT, async {
            loop {
                let Some(line) = receiver.recv().await? else {
                    return Err(Error::Link {
                        peer: profile.display_name.clone(),
                        message: String::from("exited before answering initialize"),
                    });
                };
                let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if frame.get("id") == Some(&json!(1)) {
                    return Ok(frame);
                }
            }
        })
        .await
        .map_err(|_| Error::Timeout {
            operation: format!("{} initialize", profile.display_name),
            after: CONTRACT_TIMEOUT,
        })?
    }
    .await;
    let stopped = control.kill(CancelReason::Shutdown).await;
    match outcome {
        Ok(response) => {
            stopped?;
            normalize_acp_initialize(response)
        }
        Err(error) => {
            let _ = stopped;
            Err(error)
        }
    }
}

fn normalize_acp_initialize(frame: Value) -> Result<Value> {
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

fn command_name(argv: &[String]) -> String {
    argv.join(" ")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{
        CONTRACT_TIMEOUT, acp_initialize, acp_with, claude_surface, claude_with, command_name,
        command_output, copy_generated_schema, normalize_acp_initialize, write_json,
    };
    use mango_external_agents::testing::{FakeLauncher, FakeProcess};
    use mango_external_agents::{EnvSource, ExitStatus, HostContext};
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

        assert!(error.to_string().contains("protocolVersion"), "{error}");
        assert!(!error.to_string().contains("private"), "{error}");
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

        acp_with(&host(Arc::clone(&launcher)), &out, profile.as_ref())
            .await
            .expect("expected an ACP public contract capture");

        assert_eq!(launcher.launches()[0].argv, vec!["opencode", "--version"]);
        let writes = launcher.written();
        assert_eq!(writes.len(), 1);
        assert!(writes[0].contains("\"method\":\"initialize\""));
        assert!(!writes[0].contains("authenticate"));
        assert!(!writes[0].contains("session/new"));
        assert_eq!(launcher.live_children(), 0);
        std::fs::remove_dir_all(out).expect("expected temporary capture cleanup");
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

        assert!(error.to_string().contains("stdout"), "{error}");
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_acp_initialize_reaps_the_named_fake_agent() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::responding(|_| Vec::new()));
        let profile =
            mango_agent_acp::builtin_profile("opencode").expect("expected OpenCode profile");
        let host = host(Arc::clone(&launcher));
        let capture = tokio::spawn(async move { acp_initialize(&host, profile.as_ref()).await });

        tokio::task::yield_now().await;
        tokio::time::advance(CONTRACT_TIMEOUT).await;
        let error = capture
            .await
            .expect("expected the capture task to finish")
            .expect_err("expected a silent ACP agent to time out");

        assert!(error.to_string().contains("initialize"), "{error}");
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
    fn command_name_preserves_the_exact_argument_order() {
        assert_eq!(
            command_name(&[String::from("codex"), String::from("app-server")]),
            "codex app-server"
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
