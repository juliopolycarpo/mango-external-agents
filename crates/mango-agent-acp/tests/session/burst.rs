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
/// when it fits the message budget. The burst arrives as one batch, so it is queued whole before
/// the SDK actor can drain any of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_notification_burst_larger_than_the_request_cap_completes_the_turn() {
    let (session, launcher) = open_with(
        FakeAcpAgent::new()
            .with_updates(chunks(400))
            .batching_updates(),
        Limits::default(),
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
    let summary: Vec<String> = events
        .iter()
        .map(|kind| format!("{kind:?}").chars().take(80).collect())
        .collect();
    assert!(
        matches!(events.last(), Some(EventKind::Completed))
            && events
                .iter()
                .any(|kind| matches!(kind, EventKind::TextDelta { .. }))
            && !events
                .iter()
                .any(|kind| matches!(kind, EventKind::Cancelled { .. })),
        "expected streamed text then completion for a 400-notification batch under the default 64-request cap, received {summary:?}"
    );
    assert_eq!(
        session.snapshot().status,
        SessionStatus::Ready,
        "expected the session to survive a 400-notification batch"
    );
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

/// Opens a session under `limits`, runs one turn against `agent`, and asserts the turn ends with a
/// transport budget error naming `received` and `expected`, then that the session closes with no
/// live child.
async fn assert_budget_failure(
    agent: FakeAcpAgent,
    limits: Limits,
    received: &str,
    expected: &str,
) {
    let (session, launcher) = open_with(agent, limits).await;
    let mut lifecycle = session.subscribe();
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "stream too much"))
        .await
        .expect("expected a turn");

    let events = drain(&mut turn).await;
    let summary: Vec<String> = events
        .iter()
        .map(|kind| format!("{kind:?}").chars().take(200).collect())
        .collect();
    assert!(
        !events.iter().any(|kind| matches!(
            kind,
            EventKind::TextDelta { .. } | EventKind::Cancelled { .. }
        )),
        "expected neither refused text nor a cancellation, received {summary:?}"
    );
    let errors: Vec<&mango_external_agents::VendorError> = events
        .iter()
        .filter_map(|kind| match kind {
            EventKind::Error { error } => Some(error),
            _ => None,
        })
        .collect();
    assert!(
        errors.len() == 1
            && errors[0].code.as_str() == "acp-transport-overflow"
            && errors[0].message.contains(received)
            && errors[0].message.contains(expected),
        "expected one acp-transport-overflow error naming {received:?} and {expected:?}, received {summary:?}"
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

/// A frame larger than the byte budget ends the connection, and the turn reports the budget with
/// the bytes received and the cap rather than a cancellation.
#[tokio::test]
async fn an_oversized_notification_fails_the_turn_and_releases_the_child() {
    let budget = 64 * 1024;
    let text = "x".repeat(budget);
    assert_budget_failure(
        FakeAcpAgent::new().with_updates(vec![serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": text }
        })]),
        Limits {
            turn_buffer_bytes: budget,
            ..Limits::default()
        },
        "received ",
        &format!("expected at most {budget} bytes queued from the ACP agent, received "),
    )
    .await;
}

/// A batch with more messages than `max(turn_channel_capacity, max_pending_requests)` ends the
/// connection, and the turn reports the message count and the cap rather than a cancellation.
#[tokio::test]
async fn a_batch_over_the_message_budget_fails_the_turn_and_releases_the_child() {
    assert_budget_failure(
        FakeAcpAgent::new()
            .with_updates(chunks(50))
            .batching_updates(),
        Limits {
            turn_channel_capacity: 8,
            max_pending_requests: 4,
            ..Limits::default()
        },
        "received 50",
        "expected at most 8 JSON-RPC messages",
    )
    .await;
}
