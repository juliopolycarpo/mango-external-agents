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

/// The text of every `TextDelta`, in the order the host received them.
fn delta_texts(events: &[EventKind]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|kind| match kind {
            EventKind::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Asserts that a turn fed [`chunks`]`(expected)` delivered every one of them once, in the order
/// the agent sent them, with the last one directly ahead of `Completed` and nothing after it.
///
/// A dropped tail and a terminal that overtook the last update both fail here: the first as a
/// count, the second as the event found where the last chunk belongs.
fn assert_every_chunk_precedes_completion(events: &[EventKind], expected: usize) {
    let received = delta_texts(events);
    let tail: Vec<&str> = received.iter().rev().take(3).rev().copied().collect();
    assert_eq!(
        received.len(),
        expected,
        "expected {expected} text deltas before the terminal | received {} ending with {tail:?}",
        received.len()
    );
    for (index, text) in received.iter().enumerate() {
        let wanted = format!("chunk {index}");
        assert_eq!(
            *text, wanted,
            "expected text delta {index}: {wanted:?} | received: {text:?}"
        );
    }
    let summary = |kind: &EventKind| match kind {
        EventKind::TextDelta { text } => format!("TextDelta({text:?})"),
        other => format!("{other:?}").chars().take(120).collect(),
    };
    let closing: Vec<String> = events.iter().rev().take(2).rev().map(summary).collect();
    let last_chunk = format!("TextDelta({:?})", format!("chunk {}", expected - 1));
    assert_eq!(
        closing,
        [last_chunk.clone(), String::from("Completed")],
        "expected the turn to close with [{last_chunk}, Completed] | received {closing:?}"
    );
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
    assert_every_chunk_precedes_completion(&events, 400);
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

/// How many updates the stalled-host turn streams, one frame each, ahead of its prompt response.
///
/// Under the default 64-message cap together with that response, so the whole turn can sit queued
/// at the SDK boundary without tripping the transport budget.
const STALLED_BURST: usize = 48;

/// Waits for the turn's terminal to be committed without reading a single event of it.
///
/// The stall is this condition, not a number of yields: the host takes nothing off its stream
/// until the library has finished the turn behind it.
async fn terminal_committed_while_unread(
    turn: &TurnStream,
) -> mango_external_agents::TerminalStatus {
    let waited = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(status) = turn.terminal_status() {
                return status;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    waited.unwrap_or_else(|_| {
        panic!(
            "expected a committed terminal while the host is not reading | received: none within 5s"
        )
    })
}

/// A burst of updates directly followed by the prompt response, to a host that has stopped reading:
/// the response ends the turn, so a terminal committed ahead of an update still in dispatch would
/// silently drop that update rather than reorder it. Every update must be buffered, the last one
/// included, before the terminal is.
///
/// The channel holds exactly what the turn emits, so it is full and unread when the response
/// lands. One slot fewer ends the turn as `stream-overflow` instead, which the terminal assertion
/// names, so a host that was in fact reading could not pass this by accident.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_burst_ending_in_the_prompt_response_reaches_a_stalled_host_whole_and_in_order() {
    let (session, launcher) = open_with(
        FakeAcpAgent::new().with_updates(chunks(STALLED_BURST)),
        Limits {
            // `TurnStarted` and every chunk; the terminal has a reserve of its own.
            turn_channel_capacity: STALLED_BURST + 1,
            ..Limits::default()
        },
    )
    .await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "stream to nobody"))
        .await
        .expect("expected a turn");

    let status = terminal_committed_while_unread(&turn).await;
    assert_eq!(
        status,
        mango_external_agents::TerminalStatus::Completed,
        "expected the unread turn's terminal: Completed | received: {status:?}"
    );

    let events = drain(&mut turn).await;
    assert_every_chunk_precedes_completion(&events, STALLED_BURST);
    let status = session.snapshot().status;
    assert_eq!(
        status,
        SessionStatus::Ready,
        "expected session status after the stalled turn: Ready | received: {status:?}"
    );
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected a clean close");
    assert_eq!(
        launcher.live_children(),
        0,
        "expected live children after close: 0 | received: {}",
        launcher.live_children()
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

/// A host may set an absurdly large request cap to mean "no cap". That must not panic the open
/// after the child has launched and leak the child: the admission semaphore cannot hold more than
/// `Semaphore::MAX_PERMITS`, so the cap is clamped to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_cap_of_usize_max_opens_runs_a_turn_and_reaps_the_child() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeAcpAgent::new().process());
    let host = HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .limits(Limits {
            max_pending_requests: usize::MAX,
            ..Limits::default()
        })
        .build()
        .expect("expected a host");
    let opened = tokio::spawn(async move {
        AcpHarness::new(profile())
            .open_session(&host, OpenSession::new("chat-1"))
            .await
    })
    .await;
    let session = match opened {
        Ok(Ok(session)) => session,
        Ok(Err(error)) => panic!(
            "expected open to succeed | received {error:?} (live children {})",
            launcher.live_children()
        ),
        Err(error) => panic!(
            "expected open to succeed | received {} (live children {})",
            if error.is_panic() {
                "panic"
            } else {
                "cancellation"
            },
            launcher.live_children()
        ),
    };
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "say hello"))
        .await
        .expect("expected a turn under usize::MAX");
    let events = drain(&mut turn).await;
    assert!(
        matches!(events.last(), Some(EventKind::Completed)),
        "expected the turn to complete under usize::MAX | received {:?}",
        events.last()
    );
    session.close(CloseReason::Requested).await.expect("close");
    tokio::time::timeout(Duration::from_secs(5), async {
        while launcher.live_children() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "expected live children: 0 after close | received {}",
            launcher.live_children()
        )
    });
}
