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
        Err(error) => {
            assert!(matches!(error.cause(), Error::HostConfiguration {
                expected: "no MCP servers for a harness without MCP passthrough",
                received,
            } if received == "MCP server count 1"));
            assert_eq!(
                error.dispatch(),
                mango_external_agents::Dispatch::NotSubmitted
            );
        }
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
