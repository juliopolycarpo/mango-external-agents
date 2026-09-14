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
        Err(Error::HostConfiguration { expected, .. }) => assert!(expected.contains("MCP")),
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
