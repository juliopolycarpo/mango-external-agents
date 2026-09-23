use super::*;

fn chunks(count: usize) -> Vec<serde_json::Value> {
    (0..count)
        .map(|index| {
            serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": format!("chunk {index}") }
            })
        })
        .collect()
}

async fn open_with(agent: FakeAcpAgent, limits: Limits) -> (Box<dyn Session>, FakeLauncher) {
    let launcher = FakeLauncher::new();
    launcher.push(agent.process());
    let host = HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .limits(limits)
        .build()
        .expect("expected a host");
    let session = AcpHarness::new(profile())
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    (session, launcher)
}

/// Notifications are not requests: a burst larger than the request cap must not kill the session
/// when it fits the frame budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_notification_burst_larger_than_the_request_cap_completes_the_turn() {
    let (session, launcher) = open_with(
        FakeAcpAgent::new().with_updates(chunks(400)),
        Limits {
            max_pending_requests: 2,
            ..Limits::default()
        },
    )
    .await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "stream a lot"))
        .await
        .expect("expected a turn");
    // Let the whole burst arrive while the host is not yet reading its turn.
    for _ in 0..500 {
        tokio::task::yield_now().await;
    }

    let events = drain(&mut turn).await;
    assert!(
        matches!(events.last(), Some(EventKind::Completed)),
        "expected a completed turn after 400 notifications with max_pending_requests 2, received {events:?}"
    );
    assert_eq!(session.snapshot().status, SessionStatus::Ready);
    tokio::time::timeout(
        Duration::from_secs(5),
        session.close(CloseReason::Requested),
    )
    .await
    .expect("expected close to finish")
    .expect("expected a clean close");
    assert_eq!(session.snapshot().status, SessionStatus::Closed);
    assert_eq!(
        launcher.live_children(),
        0,
        "expected no live child after close"
    );
}

/// A frame larger than the byte budget ends the connection, and the session owns the cleanup: the
/// turn ends with an error naming the budget, the session closes and no child outlives it.
#[tokio::test]
async fn an_oversized_notification_fails_the_turn_and_releases_the_child() {
    let budget = 64 * 1024;
    let text = "x".repeat(budget);
    let (session, launcher) = open_with(
        FakeAcpAgent::new().with_updates(vec![serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": text }
        })]),
        Limits {
            turn_buffer_bytes: budget,
            ..Limits::default()
        },
    )
    .await;
    let mut lifecycle = session.subscribe();
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "stream too much"))
        .await
        .expect("expected a turn");

    let events = drain(&mut turn).await;
    // The refusal is asserted with its text in the transport tests; here the frame must never reach
    // the turn, and the turn must still end exactly once.
    let summary: Vec<String> = events
        .iter()
        .map(|kind| format!("{kind:?}").chars().take(80).collect())
        .collect();
    assert!(
        !events
            .iter()
            .any(|kind| matches!(kind, EventKind::TextDelta { .. })),
        "expected the {budget}-byte-budget refusal to keep the oversized chunk out of the turn, received {summary:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|kind| matches!(kind, EventKind::Completed | EventKind::Error { .. }))
            .count(),
        1,
        "expected exactly one terminal after the transport failure, received {summary:?}"
    );
    assert_eq!(
        status_once_settled(&mut lifecycle).await,
        SessionStatus::Closed
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while launcher.live_children() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected no live child after the transport failure");
}
