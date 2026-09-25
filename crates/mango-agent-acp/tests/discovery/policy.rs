use super::*;

#[tokio::test]
async fn stdio_mcp_servers_are_sent_on_session_new() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(FakeAcpAgent::new().process());
    let command = std::env::current_exe()
        .expect("expected an absolute test executable path")
        .to_string_lossy()
        .into_owned();
    let mut request = mango_external_agents::OpenSession::new("mcp");
    request.mcp_servers.push(mango_external_agents::McpServer {
        name: String::from("docs"),
        transport: mango_external_agents::McpTransport::Stdio {
            command: command.clone(),
            args: Vec::new(),
            env: std::collections::BTreeMap::from([(
                String::from("DOCS_TOKEN"),
                String::from("test-token"),
            )]),
        },
    });
    let session = AcpHarness::builtin("cursor")
        .expect("expected Cursor profile")
        .open_session(&host(launcher.clone()), request)
        .await
        .expect("expected the ACP session to open");
    let request = launcher
        .written()
        .into_iter()
        .map(|line| serde_json::from_str::<serde_json::Value>(&line).expect("expected JSON-RPC"))
        .find(|message| message.get("method") == Some(&serde_json::json!("session/new")))
        .expect("expected session/new");
    assert_eq!(
        request["params"]["mcpServers"],
        serde_json::json!([{
            "name": "docs",
            "command": command,
            "args": [],
            "env": [{ "name": "DOCS_TOKEN", "value": "test-token" }],
        }]),
        "expected the supported stdio MCP server on session/new"
    );
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected close");
}

#[tokio::test]
async fn advertised_http_mcp_preserves_endpoint_and_headers() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(FakeAcpAgent::new().with_http_mcp().process());
    let mut request = mango_external_agents::OpenSession::new("mcp");
    request.mcp_servers.push(mango_external_agents::McpServer {
        name: String::from("remote"),
        transport: mango_external_agents::McpTransport::Http {
            url: String::from("https://mcp.example:8443/endpoint"),
            headers: std::collections::BTreeMap::from([(
                String::from("Authorization"),
                String::from("Bearer test-token"),
            )]),
        },
    });
    let session = AcpHarness::builtin("cursor")
        .expect("expected Cursor profile")
        .open_session(&host(launcher.clone()), request)
        .await
        .expect("expected the advertised HTTP MCP server to open");
    let request = launcher
        .written()
        .into_iter()
        .map(|line| serde_json::from_str::<serde_json::Value>(&line).expect("expected JSON-RPC"))
        .find(|message| message.get("method") == Some(&serde_json::json!("session/new")))
        .expect("expected session/new");
    assert_eq!(request["params"]["mcpServers"][0]["name"], "remote");
    assert_eq!(
        request["params"]["mcpServers"][0]["url"],
        "https://mcp.example:8443/endpoint"
    );
    assert_eq!(
        request["params"]["mcpServers"][0]["headers"][0],
        serde_json::json!({ "name": "Authorization", "value": "Bearer test-token" })
    );
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected close");
}

/// Bad MCP entries are a host configuration error, before an ACP child can receive one.
#[tokio::test]
async fn malformed_mcp_servers_are_not_submitted_or_spawned() {
    let cases = [
        vec![mango_external_agents::McpServer::stdio(
            "",
            "/usr/bin/docs-mcp",
        )],
        vec![mango_external_agents::McpServer::stdio(
            "docs",
            "relative-mcp",
        )],
        vec![
            mango_external_agents::McpServer::stdio("docs", "/usr/bin/docs-mcp"),
            mango_external_agents::McpServer::stdio("docs", "/usr/bin/other-mcp"),
        ],
        vec![mango_external_agents::McpServer::stdio(
            "docs",
            "/usr/bin/docs\u{1b}mcp",
        )],
        vec![mango_external_agents::McpServer::stdio(
            "docs\u{1b}",
            "/usr/bin/docs-mcp",
        )],
        vec![mango_external_agents::McpServer {
            name: String::from("docs"),
            transport: mango_external_agents::McpTransport::Stdio {
                command: String::from("/usr/bin/docs-mcp"),
                args: Vec::new(),
                env: std::collections::BTreeMap::from([(
                    String::from("BAD=NAME"),
                    String::from("token"),
                )]),
            },
        }],
        vec![mango_external_agents::McpServer {
            name: String::from("docs"),
            transport: mango_external_agents::McpTransport::Stdio {
                command: String::from("/usr/bin/docs-mcp"),
                args: Vec::new(),
                env: std::collections::BTreeMap::from([(
                    String::from("DOCS_TOKEN"),
                    String::from("token\r\nforged"),
                )]),
            },
        }],
        vec![mango_external_agents::McpServer {
            name: String::from("docs"),
            transport: mango_external_agents::McpTransport::Http {
                url: String::from("https://mcp.example/path"),
                headers: std::collections::BTreeMap::from([(
                    String::from("X: forged"),
                    String::from("value"),
                )]),
            },
        }],
        vec![mango_external_agents::McpServer {
            name: String::from("docs"),
            transport: mango_external_agents::McpTransport::Http {
                url: String::from("https://mcp.example/path"),
                headers: std::collections::BTreeMap::from([(
                    String::from("Authorization"),
                    String::from("ok\r\nX-Injected: yes"),
                )]),
            },
        }],
        vec![mango_external_agents::McpServer {
            name: String::from("docs"),
            transport: mango_external_agents::McpTransport::Http {
                url: String::from("http://?query"),
                headers: std::collections::BTreeMap::new(),
            },
        }],
        vec![mango_external_agents::McpServer {
            name: String::from("docs"),
            transport: mango_external_agents::McpTransport::Http {
                url: String::from("https://mcp.example:not-a-port/path"),
                headers: std::collections::BTreeMap::new(),
            },
        }],
        vec![mango_external_agents::McpServer {
            name: String::from("docs"),
            transport: mango_external_agents::McpTransport::Http {
                url: String::from("https://[::1/path"),
                headers: std::collections::BTreeMap::new(),
            },
        }],
    ];
    for servers in cases {
        let launcher = Arc::new(FakeLauncher::new());
        let mut request = mango_external_agents::OpenSession::new("mcp");
        request.mcp_servers = servers;
        let result = AcpHarness::builtin("cursor")
            .expect("expected Cursor profile")
            .open_session(&host(launcher.clone()), request)
            .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("expected invalid MCP settings to be refused before launch"),
        };
        assert_eq!(
            error.dispatch(),
            mango_external_agents::Dispatch::NotSubmitted,
            "expected a retry-safe pre-launch refusal, received {error:?}"
        );
        assert!(
            matches!(
                error.cause(),
                mango_external_agents::Error::HostConfiguration { .. }
            ),
            "expected a typed host configuration refusal, received {error:?}"
        );
        assert!(
            launcher.launches().is_empty(),
            "expected no ACP child for invalid MCP settings, received {:?}",
            launcher.launches()
        );
    }
}

#[tokio::test]
async fn strict_resume_is_a_typed_refusal_when_the_agent_does_not_advertise_it() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(FakeAcpAgent::new().without_load_session().process());
    let result = AcpHarness::builtin("cursor")
        .expect("expected Cursor profile")
        .open_session(
            &host(launcher),
            mango_external_agents::OpenSession::new("resume")
                .resuming("agent-session", mango_external_agents::ResumeMode::Strict),
        )
        .await;

    assert!(
        matches!(
            result,
            Err(Error::NotSupported {
                capability: mango_external_agents::Capability::Resume
            })
        ),
        "expected the advertised missing resume capability"
    );
}

/// A strict resume asks for that conversation or nothing. When the agent advertised
/// `session/load` and then refused it, the refusal is the answer: no fresh session is opened in
/// its place, and the child is ended.
#[tokio::test]
async fn strict_resume_returns_the_agents_refusal_of_session_load() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(
        FakeAcpAgent::new()
            .refusing_load_session(-32002, "thread expired")
            .process(),
    );
    let result = AcpHarness::builtin("cursor")
        .expect("expected Cursor profile")
        .open_session(
            &host(launcher.clone()),
            mango_external_agents::OpenSession::new("resume")
                .resuming("agent-session", mango_external_agents::ResumeMode::Strict),
        )
        .await;

    let Err(error) = result else {
        panic!("expected a strict resume to fail on a refused session/load, received a session");
    };
    assert!(
        matches!(error.cause(), Error::Vendor(vendor) if vendor.vendor_code.as_deref() == Some("-32002")),
        "expected the agent's own refusal, received {error:?}"
    );
    let written = launcher.written();
    assert!(written.iter().any(|line| line.contains("\"session/load\"")));
    assert!(
        !written.iter().any(|line| line.contains("\"session/new\"")),
        "expected no fresh session in place of a strict resume"
    );
    assert_eq!(
        launcher.live_children(),
        0,
        "expected the refused open to end the child"
    );
}

/// A negotiated absence of session/load conclusively rules out resume before a load is attempted.
#[tokio::test]
async fn fallback_starts_new_when_the_agent_does_not_advertise_load_session() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(FakeAcpAgent::new().without_load_session().process());
    let session = AcpHarness::builtin("cursor")
        .expect("expected Cursor profile")
        .open_session(
            &host(launcher.clone()),
            mango_external_agents::OpenSession::new("resume")
                .resuming("agent-session", mango_external_agents::ResumeMode::Fallback),
        )
        .await
        .expect("expected a conclusive absence of loadSession to start fresh");

    assert!(!session.snapshot().resumed);
    assert_ne!(session.ids().native_session_id, "agent-session");
    assert!(
        session
            .snapshot()
            .fallback_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("loadSession")),
        "expected a stated fallback reason"
    );
    let written = launcher.written();
    assert!(written.iter().any(|line| line.contains("\"session/new\"")));
    assert!(!written.iter().any(|line| line.contains("\"session/load\"")));
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected close");
}

/// A resumed remote session runs under the directory the current host authorized.
///
/// Its saved ACP id is opaque history, not authority to reuse the directory from a prior hub.
#[tokio::test]
async fn session_load_uses_the_hosts_authorized_working_directory() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(FakeAcpAgent::new().process());
    let workspace = std::env::temp_dir().join("authorized-workspace");
    let host = HostContext::builder()
        .launcher(launcher.clone())
        .cwd(&workspace)
        .client_info("discovery-tests", "0.1.0")
        .build()
        .expect("expected a host");
    let session = AcpHarness::builtin("cursor")
        .expect("expected Cursor profile")
        .open_session(
            &host,
            mango_external_agents::OpenSession::new("resume").resuming(
                "remote-agent-session",
                mango_external_agents::ResumeMode::Strict,
            ),
        )
        .await
        .expect("expected the saved ACP session to load");
    let request = launcher
        .written()
        .into_iter()
        .map(|line| serde_json::from_str::<serde_json::Value>(&line).expect("expected JSON-RPC"))
        .find(|message| message.get("method") == Some(&serde_json::json!("session/load")))
        .expect("expected session/load");
    assert_eq!(
        request["params"]["cwd"],
        serde_json::json!(workspace),
        "expected the current host directory rather than a saved remote context"
    );
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected close");
}

/// A fallback resume starts a fresh conversation, and the host is told why.
///
/// The reason is built from the error's typed fields rather than copied out of its diagnostic
/// text: `Display` reports only metadata for anything a vendor filled in, so a reason taken from
/// there says "vendor failure" and leaves the host with a session whose history vanished for no
/// stated cause.
#[tokio::test]
async fn a_fallback_resume_reports_why_the_load_failed() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(
        FakeAcpAgent::new()
            .refusing_load_session(-32002, "thread expired")
            .process(),
    );
    let session = AcpHarness::builtin("cursor")
        .expect("expected Cursor profile")
        .open_session(
            &host(launcher),
            mango_external_agents::OpenSession::new("resume")
                .resuming("agent-session", mango_external_agents::ResumeMode::Fallback),
        )
        .await
        .expect("expected the fallback to open a fresh session");

    let info = session.snapshot();
    assert!(!info.resumed, "expected a fresh conversation");
    let reason = info
        .fallback_reason
        .as_deref()
        .expect("expected the host to be told why the resume did not happen");
    assert!(
        reason.starts_with("session/load "),
        "expected the failed operation to be named, received {reason:?}"
    );
    assert!(
        reason.contains("refused by the vendor"),
        "expected the vendor refusal to be explained, received {reason:?}"
    );
    assert!(
        !reason.contains("thread expired"),
        "expected the vendor's own text to stay out of the reason, received {reason:?}"
    );
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected close");
}

#[test]
fn builtin_profiles_do_not_forward_credentials() {
    for profile in mango_agent_acp::builtin_profiles() {
        assert!(
            profile
                .vendor_environment_keys
                .iter()
                .all(|key| !key.ends_with("_API_KEY")),
            "expected no credential forwarding for {}, received {:?}",
            profile.id,
            profile.vendor_environment_keys
        );
    }
}
