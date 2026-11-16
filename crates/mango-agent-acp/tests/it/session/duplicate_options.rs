//! Ambiguous native option IDs must be refused before host policy can select a wire ID.

use super::*;

#[tokio::test]
async fn duplicate_permission_options_are_cancelled_before_a_denying_broker_is_called() {
    let announcer = mango_external_agents::testing::Announcer::new();
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeAcpAgent::new()
            .with_updates(Vec::new())
            .staying_silent()
            .process()
            .announcing(announcer.clone()),
    );
    let broker = RecordingBroker::new(BrokerDecision::Deny {
        reason: String::from("host denies this operation"),
    });
    let host = HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("duplicate-option-tests", "0.0.0")
        .broker(Arc::new(broker.clone()))
        .build()
        .expect("host");
    let session = AcpHarness::new(profile())
        .open_session(
            &host,
            OpenSession::new("duplicate-options").with_configuration(permissive()),
        )
        .await
        .expect("session");
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "go"))
        .await
        .expect("turn");
    announcer.announce(
        serde_json::json!({
            "jsonrpc": "2.0", "id": 7779, "method": "session/request_permission",
            "params": {
                "sessionId": "sess_fake",
                "toolCall": { "toolCallId": "duplicate-call", "kind": "execute", "title": "Operation" },
                "options": [
                    { "optionId": "private-duplicate-option-id", "name": "Allow", "kind": "allow_once" },
                    { "optionId": "private-duplicate-option-id", "name": "Reject", "kind": "reject_once" }
                ]
            }
        })
        .to_string(),
    );
    let answer = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let written = launcher.written();
            if let Some(answer) = written.iter().find_map(|line| {
                let frame: serde_json::Value = serde_json::from_str(line).ok()?;
                (frame.get("id") == Some(&serde_json::json!(7779))).then_some(frame)
            }) {
                break answer;
            }
            launcher.wait_for_written(written.len() + 1).await;
        }
    })
    .await
    .expect("native permission response");
    session.close(CloseReason::Shutdown).await.expect("close");
    let seen = drain(&mut turn).await;
    assert_eq!(launcher.live_children(), 0);
    assert!(
        broker.requests().is_empty(),
        "duplicate native options must be refused before invoking the host broker"
    );
    assert_eq!(
        answer.pointer("/result/outcome/outcome"),
        Some(&serde_json::json!("cancelled")),
        "duplicate options must not produce any selected option"
    );
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, EventKind::ApprovalRequested { .. })),
        "an ambiguous approval must not reach the host"
    );
}
