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

/// A host close that races the overflow's cleanup owns the turn's terminal; it must still report
/// the budget rather than cancelling with the close's own reason.
#[tokio::test]
async fn a_close_racing_a_budget_overflow_still_reports_the_budget() {
    let inner = FakeLauncher::new();
    inner.push(
        FakeAcpAgent::new()
            .with_updates(chunks(50))
            .batching_updates()
            .process(),
    );
    let launcher = GatedLauncher::new(inner.clone(), false);
    let host = HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .limits(Limits {
            turn_channel_capacity: 8,
            max_pending_requests: 4,
            kill_grace: Duration::from_millis(10),
            shutdown_timeout: Duration::from_secs(5),
            ..Limits::default()
        })
        .build()
        .expect("expected a host");
    let session: Arc<dyn Session> = Arc::from(
        AcpHarness::new(profile())
            .open_session(&host, OpenSession::new("chat-1"))
            .await
            .expect("expected a session"),
    );
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "stream too much"))
        .await
        .expect("expected a turn");
    tokio::time::timeout(Duration::from_secs(5), launcher.wait_for_kill())
        .await
        .expect("expected the overflow cleanup to reach the held process kill");

    let closing = Arc::clone(&session);
    let mut close = tokio::spawn(async move { closing.close(CloseReason::Requested).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut close)
            .await
            .is_err(),
        "expected close waiter: pending while the kill is held | received: completed early"
    );
    launcher.release();
    tokio::time::timeout(Duration::from_secs(5), close)
        .await
        .expect("expected close to settle after release")
        .expect("expected the close task to join")
        .expect("expected a clean close");

    let events = drain(&mut turn).await;
    let summary: Vec<String> = events
        .iter()
        .map(|kind| format!("{kind:?}").chars().take(200).collect())
        .collect();
    let codes: Vec<&str> = events
        .iter()
        .filter_map(|kind| match kind {
            EventKind::Error { error } => Some(error.code.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        codes == ["acp-transport-overflow"]
            && !events
                .iter()
                .any(|kind| matches!(kind, EventKind::Cancelled { .. })),
        "expected one acp-transport-overflow error and no cancellation, received {summary:?}"
    );
    assert_eq!(session.snapshot().status, SessionStatus::Closed);
    assert_eq!(
        inner.live_children(),
        0,
        "expected live children: 0 | received: {}",
        inner.live_children()
    );
}

/// Tool output an agent re-sends whole with every line, as ACP's replace-the-collection rule has it.
fn streamed_output(count: usize) -> Vec<serde_json::Value> {
    let mut frames = vec![serde_json::json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "call_build",
        "title": "Run `cargo build`",
        "kind": "execute",
        "status": "in_progress"
    })];
    frames.extend((0..count).map(|line| {
        serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_build",
            "content": [{
                "type": "content",
                "content": { "type": "text", "text": format!("{line:04} {}", "x".repeat(2_000)) }
            }]
        })
    }));
    frames.push(serde_json::json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": "call_build",
        "status": "completed"
    }));
    frames
}

/// A build log streamed through one call is the case that spent a host's persisted-payload budget
/// on copies of the same output: 300 full replacements of a 2 KB log are over a megabyte that a
/// host stores and immediately overwrites. Coalesced, the host receives a handful of updates, the
/// last of them carrying the final output, and the turn completes.
#[tokio::test]
async fn streaming_tool_output_reaches_the_host_coalesced() {
    let (session, _launcher) = open_with(
        FakeAcpAgent::new().with_updates(streamed_output(300)),
        Limits::default(),
    )
    .await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "build it"))
        .await
        .expect("expected a turn");
    let events = drain(&mut turn).await;

    let updates: Vec<&mango_external_agents::ActivityUpdate> = events
        .iter()
        .filter_map(|kind| match kind {
            EventKind::ActivityUpdated { update, .. } => Some(update),
            _ => None,
        })
        .collect();
    assert!(
        matches!(events.last(), Some(EventKind::Completed)),
        "expected the turn to complete, received {:?}",
        events.last()
    );
    assert!(
        updates.len() <= 4,
        "expected 300 replacements coalesced to a handful of updates, received {}",
        updates.len()
    );
    let last_output = updates.last().and_then(|update| match &update.content {
        Some(mango_external_agents::ActivityContent::Output { text }) => Some(text.as_str()),
        _ => None,
    });
    assert!(
        last_output.is_some_and(|text| text.starts_with("0299 ")),
        "expected the final output delivered, received {:?}",
        last_output.map(|text| text.chars().take(8).collect::<String>())
    );
}
