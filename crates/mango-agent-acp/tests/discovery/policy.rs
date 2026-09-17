use super::*;

#[tokio::test]
async fn unsupported_host_mcp_servers_are_refused_before_launch() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(FakeAcpAgent::new().process());
    let mut request = mango_external_agents::OpenSession::new("mcp");
    request
        .mcp_servers
        .push(mango_external_agents::McpServer::stdio("docs", "docs-mcp"));
    let result = AcpHarness::builtin("cursor")
        .expect("expected Cursor profile")
        .open_session(&host(launcher.clone()), request)
        .await;
    match result {
        Err(Error::HostConfiguration {
            expected: "no MCP servers for a harness without MCP passthrough",
            received,
        }) => assert_eq!(received, "MCP server count 1"),
        Err(error) => panic!("expected an unsupported MCP configuration, received {error}"),
        Ok(session) => {
            session
                .close(CloseReason::Requested)
                .await
                .expect("expected close");
            panic!("expected an unsupported MCP configuration, received an opened session");
        }
    }
    assert!(
        launcher.launches().is_empty(),
        "expected refusal before launch"
    );
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
