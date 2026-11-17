//! The bytes of the per-thread `config` on `thread/start` and `thread/resume`.
//!
//! `config` carries `serde_json` objects, which a build sorts by member name unless some crate in
//! it turns on `serde_json/preserve_order` (`mango-agent-acp` does, through its schema crate).
//! Both requests write every object sorted, so the text below is the same in either build:
//! `cargo nextest run -p mango-agent-codex` runs it without the feature, and
//! `cargo nextest run -p mango-agent-codex --features serde_json/preserve_order` with it.

use super::*;

/// Three servers, listed so that neither the host's order (`remote, docs, bare`) nor the order a
/// stdio entry is built in (`command, args, env`) is the sorted one.
fn servers_in_host_order() -> Vec<McpServer> {
    vec![
        McpServer {
            name: String::from("remote"),
            transport: McpTransport::Http {
                url: String::from("https://example.com/mcp"),
                headers: [(String::from("X-Docs-Key"), String::from("header-secret"))].into(),
            },
        },
        McpServer {
            name: String::from("docs"),
            transport: McpTransport::Stdio {
                command: String::from("docs-mcp"),
                args: vec![String::from("--stdio"), String::from("--another")],
                env: [(String::from("DOCS_KEY"), String::from("server-secret"))].into(),
            },
        },
        McpServer::stdio("bare", "bare-mcp"),
    ]
}

/// The `config` text both requests carry for [`servers_in_host_order`] and an effort of `high`.
const CONFIG: &str = concat!(
    r#"{"mcp_servers":{"#,
    r#""bare":{"args":[],"command":"bare-mcp","env":{}},"#,
    r#""docs":{"args":["--stdio","--another"],"command":"docs-mcp","env":{"DOCS_KEY":"server-secret"}},"#,
    r#""remote":{"http_headers":{"X-Docs-Key":"header-secret"},"url":"https://example.com/mcp"}"#,
    r#"},"model_reasoning_effort":"high"}"#,
);

fn configured_open(request: OpenSession) -> OpenSession {
    let mut request = request.with_mcp_servers(servers_in_host_order());
    request.configuration =
        ConfigurationPatch::new().effort(ConfigurationChange::Set(String::from("high")));
    request
}

/// The text of the `params` member of the one written line that calls `method`.
fn params_text(written: &[String], method: &str) -> String {
    let needle = format!(r#""method":"{method}","params":"#);
    let line = written
        .iter()
        .find(|line| line.contains(&needle))
        .unwrap_or_else(|| panic!("expected a {method} request | received {written:?}"));
    let (_, params) = line
        .split_once(&needle)
        .unwrap_or_else(|| panic!("expected {needle} in {line}"));
    params
        .strip_suffix('}')
        .unwrap_or_else(|| panic!("expected the frame to end its params | received {line}"))
        .to_owned()
}

#[tokio::test]
async fn thread_start_writes_its_config_in_one_member_order() {
    let (host, launcher) = host_replaying(&["handshake"]);
    let _session = CodexHarness::new()
        .open_session(&host, configured_open(OpenSession::new("chat-1")))
        .await
        .expect("expected supported host MCP configuration");

    let received = params_text(&launcher.written(), "thread/start");
    let expected = format!(
        r#"{{"cwd":{},"config":{CONFIG}}}"#,
        serde_json::Value::from(workspace_path().to_string_lossy().into_owned())
    );
    assert_eq!(
        received, expected,
        "expected thread/start params with a sorted config: {expected} | received: {received}"
    );
}

#[tokio::test]
async fn thread_resume_writes_its_config_in_one_member_order() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(
        ResumeSuccessServer {
            read_workspace: workspace_path().to_string_lossy().into_owned(),
            resume_id: None,
            resume_workspace: None,
        }
        .process(),
    );
    let (host, launcher) = with_launcher(launcher, None);
    let native_id = Transcript::load("handshake")
        .thread_id()
        .expect("expected a captured thread id");
    let _session = CodexHarness::new()
        .open_session(
            &host,
            configured_open(
                OpenSession::new("chat-1")
                    .resuming(&native_id, mango_external_agents::ResumeMode::Strict),
            ),
        )
        .await
        .expect("expected the host MCP override on resume");

    let received = params_text(&launcher.written(), "thread/resume");
    let expected = format!(
        r#"{{"threadId":{},"cwd":{},"config":{CONFIG},"excludeTurns":true}}"#,
        serde_json::Value::from(native_id),
        serde_json::Value::from(workspace_path().to_string_lossy().into_owned())
    );
    assert_eq!(
        received, expected,
        "expected thread/resume params with a sorted config: {expected} | received: {received}"
    );
}
