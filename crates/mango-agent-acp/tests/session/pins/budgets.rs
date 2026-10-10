//! The bounds on what may be outstanding or queued at once, in either direction, and what a host
//! is told when one is passed.

use super::*;

use mango_external_agents::ByteSink;

/// `max_pending_requests` bounds the requests that are not prompts. With the one permit held by
/// a request the agent has not answered, the next request is refused with the limit and never
/// written, while a prompt is still admitted and runs to its end: a turn has its own single slot
/// and takes no permit.
#[tokio::test]
async fn a_request_past_the_cap_is_refused_while_a_prompt_is_still_admitted() {
    let (session, launcher) = open_under(
        FakeAcpAgent::new().holding_listing().process(),
        Limits {
            max_pending_requests: 1,
            ..Limits::default()
        },
    )
    .await;
    let session: Arc<dyn Session> = Arc::from(session);
    let held = {
        let session = Arc::clone(&session);
        tokio::spawn(async move { session.list_sessions(Default::default()).await })
    };
    eventually("the held session/list on the wire", &launcher, || {
        wire_labels(&launcher)
            .contains(&String::from("session/list"))
            .then_some(())
    })
    .await;

    let refused = refusal(session.list_sessions(Default::default()).await);
    assert_limit_exceeded(&refused, ("outstanding ACP requests", 1, 2));

    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "say hello"))
        .await
        .unwrap_or_else(|error| {
            panic!("expected a prompt admitted past the cap | received: {error:?}")
        });
    let events = drain(&mut turn).await;
    assert_events(
        "the last event of the prompt admitted past the cap",
        &events[events.len().saturating_sub(1)..],
        &["Completed"],
    );
    assert_wire_after(
        &launcher,
        "session/new",
        &["session/list", "session/prompt"],
    );
    assert!(
        !held.is_finished(),
        "expected the held request: still pending | received: finished"
    );
    held.abort();
}

/// The other direction of the same rule: a prompt in flight holds no request permit, so with a
/// cap of one a request sent during the turn is admitted and answered.
#[tokio::test]
async fn a_prompt_in_flight_does_not_use_the_request_cap() {
    let (session, _launcher) = open_under(
        FakeAcpAgent::new()
            .with_updates(Vec::new())
            .staying_silent()
            .listing_sessions()
            .process(),
        Limits {
            max_pending_requests: 1,
            ..Limits::default()
        },
    )
    .await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "wait"))
        .await
        .expect("expected a turn");

    let listed = session.list_sessions(Default::default()).await;
    let received = listed.as_ref().map(|page| page.sessions.len()).ok();
    let expected = Some(1);
    assert_eq!(
        received, expected,
        "expected the rows listed while the prompt is in flight: {expected:?} | received: {received:?} from {listed:?}"
    );

    session
        .cancel(CancelReason::Requested)
        .await
        .expect("expected the cancel to be queued");
    assert_events(
        "events",
        &drain(&mut turn).await,
        &["TurnStarted", "Cancelled(Requested)", "Completed"],
    );
}

/// How many messages the flooding peers put in one batch: past the default 1,024-message budget.
const FLOOD: usize = 1_100;

/// One JSON-RPC batch line of [`FLOOD`] notifications.
fn flood() -> String {
    let batch: Vec<serde_json::Value> = (0..FLOOD)
        .map(|_| {
            serde_json::json!({
                "jsonrpc": "2.0", "method": "session/update",
                "params": { "sessionId": "sess_fake", "update": text_chunk("x") }
            })
        })
        .collect();
    serde_json::Value::Array(batch).to_string()
}

/// Asserts `error` is the incoming message budget naming the default limit and [`FLOOD`].
#[track_caller]
fn assert_incoming_message_overflow(error: &Error) {
    assert_limit_exceeded(
        error,
        ("JSON-RPC messages queued from the ACP agent", 1_024, FLOOD),
    );
}

/// An agent that floods before the handshake fails the open with the budget it passed, naming
/// the limit and the count received, and the child it launched is reaped.
#[tokio::test]
async fn a_flood_before_the_handshake_fails_the_open_with_the_message_budget() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeAcpAgent::new().process().with_greeting([flood()]));
    let error = refusal(
        AcpHarness::new(profile())
            .open_session(
                &host_with(&launcher, Limits::default()),
                OpenSession::new("chat-1"),
            )
            .await,
    );
    assert_incoming_message_overflow(&error);
    assert_no_live_children("after the failed open", &launcher);
}

/// A request in flight when the agent floods is failed with the same budget, not with a closed
/// link or a timeout, and the session ends `Closed` with its child reaped.
#[tokio::test]
async fn a_flood_under_a_request_fails_it_with_the_message_budget() {
    let peer = ScriptedPeer::new(|frame| match frame["method"].as_str() {
        Some("session/list") => vec![flood()],
        _ => Vec::new(),
    })
    .with_session_capabilities(serde_json::json!({ "list": {} }));
    let (session, launcher) = open_under(peer.process(), Limits::default()).await;
    let mut lifecycle = session.subscribe();

    let error = refusal(session.list_sessions(Default::default()).await);
    assert_incoming_message_overflow(&error);

    assert_status(
        "after the overflow",
        status_once_settled(&mut lifecycle).await,
        SessionStatus::Closed,
    );
    assert_no_live_children("after the overflow", &launcher);
}

/// One frame larger than `turn_buffer_bytes` fails the request it arrives under with the byte
/// budget, naming the limit and the size of the frame.
#[tokio::test]
async fn a_frame_over_the_byte_budget_fails_the_request_with_the_byte_budget() {
    const BUDGET: usize = 64 * 1024;
    let oversized = serde_json::json!({
        "jsonrpc": "2.0", "method": "session/update",
        "params": { "sessionId": "sess_fake", "update": text_chunk(&"x".repeat(BUDGET)) }
    })
    .to_string();
    let size = oversized.len();
    let peer = ScriptedPeer::new(move |frame| match frame["method"].as_str() {
        Some("session/list") => vec![oversized.clone()],
        _ => Vec::new(),
    })
    .with_session_capabilities(serde_json::json!({ "list": {} }));
    let (session, launcher) = open_under(
        peer.process(),
        Limits {
            turn_buffer_bytes: BUDGET,
            ..Limits::default()
        },
    )
    .await;

    let error = refusal(session.list_sessions(Default::default()).await);
    assert_limit_exceeded(&error, ("bytes queued from the ACP agent", BUDGET, size));
    eventually("the child reaped after the overflow", &launcher, || {
        (launcher.live_children() == 0).then_some(())
    })
    .await;
}

/// A launcher whose child stops reading its stdin once a line containing `after` has landed:
/// that line is delivered, and the next write never returns.
///
/// Models an agent that is alive and still writing but no longer drains its input, which is the
/// only way a host's outgoing queue can fill.
#[derive(Clone)]
struct StallingLauncher {
    inner: FakeLauncher,
    after: &'static str,
}

impl StallingLauncher {
    fn new(inner: FakeLauncher, after: &'static str) -> Self {
        Self { inner, after }
    }
}

#[async_trait::async_trait]
impl ProcessLauncher for StallingLauncher {
    async fn spawn(
        &self,
        spec: mango_external_agents::LaunchSpec,
    ) -> mango_external_agents::Result<ManagedProcess> {
        let mut process = self.inner.spawn(spec).await?;
        process.stdin = process.stdin.take().map(|inner| -> Box<dyn ByteSink> {
            Box::new(StallingStdin {
                inner,
                after: self.after,
                stalled: false,
            })
        });
        Ok(process)
    }
}

struct StallingStdin {
    inner: Box<dyn ByteSink>,
    after: &'static str,
    stalled: bool,
}

#[async_trait::async_trait]
impl ByteSink for StallingStdin {
    async fn write_all(&mut self, bytes: &[u8]) -> mango_external_agents::Result<()> {
        if self.stalled {
            return std::future::pending().await;
        }
        self.stalled = String::from_utf8_lossy(bytes).contains(self.after);
        self.inner.write_all(bytes).await
    }

    async fn close(&mut self) -> mango_external_agents::Result<()> {
        self.inner.close().await
    }
}

/// The outgoing byte budget of the stalled-writer tests: room for the handshake and a prompt,
/// and far less than the replies the flooding agent asks for.
const OUTBOUND: usize = 4 * 1024;

fn stalling_host(launcher: &StallingLauncher, limits: Limits) -> HostContext {
    HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .limits(limits)
        .outbound_buffer_bytes(OUTBOUND)
        .build()
        .expect("expected a host")
}

/// An agent that stops reading its stdin while it keeps asking for things makes the client's
/// replies pile up. The queue is bounded: the connection fails with the outgoing byte budget
/// instead of growing, the turn reports that budget, and teardown does not wait on the writer.
#[tokio::test(start_paused = true)]
async fn replies_queued_behind_a_stalled_agent_fail_the_turn_with_the_outgoing_budget() {
    let asks = 200;
    let agent = (0..asks).fold(
        FakeAcpAgent::new()
            .with_updates(Vec::new())
            .never_finishing_turns(),
        |agent, index| {
            agent.writing_mid_turn(
                0,
                serde_json::json!({
                    "jsonrpc": "2.0", "id": 7_000 + index, "method": "fs/read_text_file",
                    "params": { "sessionId": "sess_fake", "path": "/etc/hostname" }
                })
                .to_string(),
            )
        },
    );
    let inner = FakeLauncher::new();
    inner.push(agent.process());
    let launcher = StallingLauncher::new(inner.clone(), "\"session/prompt\"");
    let limits = Limits::default();
    let session = AcpHarness::new(profile())
        .open_session(
            &stalling_host(&launcher, limits),
            OpenSession::new("chat-1"),
        )
        .await
        .expect("expected a session");
    let mut lifecycle = session.subscribe();

    let begun = tokio::time::Instant::now();
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "go"))
        .await
        .expect("expected a turn");
    let events = drain(&mut turn).await;
    assert_events(
        "events",
        &events,
        &["TurnStarted", "Error(acp-transport-overflow)"],
    );
    let Some(EventKind::Error { error }) = events.last() else {
        unreachable!("the events above end in an error");
    };
    let prefix = format!("expected at most {OUTBOUND} bytes queued for the ACP agent, received ");
    let received: Option<usize> = error
        .message
        .strip_prefix(&prefix)
        .and_then(|count| count.parse().ok());
    assert!(
        received.is_some_and(|count| count > OUTBOUND),
        "expected a message {prefix:?} followed by a count over {OUTBOUND} | received: {:?}",
        error.message
    );

    let status = status_once_settled(&mut lifecycle).await;
    let waited = begun.elapsed();
    assert_status("after the overflow", status, SessionStatus::Closed);
    assert_no_live_children("after the overflow", &inner);
    assert!(
        waited <= limits.shutdown_timeout,
        "expected teardown within shutdown_timeout ({:?}) | received: {waited:?}",
        limits.shutdown_timeout
    );
}

/// A close does not wait for a writer that will never finish: with the prompt stuck in a write
/// the agent is not reading, `close` still returns within `shutdown_timeout`, the turn ends
/// cancelled with the close's reason and the child is reaped.
#[tokio::test(start_paused = true)]
async fn a_close_does_not_wait_for_a_write_the_agent_never_reads() {
    let inner = FakeLauncher::new();
    inner.push(FakeAcpAgent::new().process());
    let launcher = StallingLauncher::new(inner.clone(), "\"session/new\"");
    let limits = Limits::default();
    let session = AcpHarness::new(profile())
        .open_session(
            &stalling_host(&launcher, limits),
            OpenSession::new("chat-1"),
        )
        .await
        .expect("expected a session");
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "never read"))
        .await
        .expect("expected a turn");

    let begun = tokio::time::Instant::now();
    let closed = session.close(CloseReason::Requested).await;
    let received = (closed.is_ok(), begun.elapsed() <= limits.shutdown_timeout);
    let expected = (true, true);
    assert_eq!(
        received,
        expected,
        "expected (close succeeded, within {:?}): {expected:?} | received: {received:?} from {closed:?} after {:?}",
        limits.shutdown_timeout,
        begun.elapsed()
    );
    assert_events(
        "events",
        &drain(&mut turn).await,
        &["TurnStarted", "Cancelled(Requested)", "Completed"],
    );
    // The stalled prompt never landed.
    assert_wire_after(&inner, "session/new", &[]);
    assert_status(
        "after close",
        session.snapshot().status,
        SessionStatus::Closed,
    );
    assert_no_live_children("after close", &inner);
}

/// How many questions the asking peer raises at once, and how many a session may hold.
const ASKS: usize = 5;
const PARKED_CAP: usize = 2;

/// A named ACP peer that raises [`ASKS`] permission requests at once under one prompt and ends
/// the turn as cancelled when it reads `session/cancel`.
#[derive(Clone, Default)]
struct AskingPeer {
    prompt: Arc<Mutex<Option<serde_json::Value>>>,
}

impl AskingPeer {
    fn process(&self) -> FakeProcess {
        let peer = self.clone();
        ScriptedPeer::new(move |frame| peer.answer(frame)).process()
    }

    fn answer(&self, frame: &serde_json::Value) -> Vec<String> {
        let mut prompt = self.prompt.lock().expect("expected the prompt slot");
        match frame["method"].as_str() {
            Some("session/prompt") => {
                *prompt = frame.get("id").cloned();
                (1..=ASKS).map(Self::ask).collect()
            }
            Some("session/cancel") => prompt
                .take()
                .map(|id| reply(&id, serde_json::json!({ "stopReason": "cancelled" })))
                .into_iter()
                .collect(),
            _ => Vec::new(),
        }
    }

    fn ask(number: usize) -> String {
        serde_json::json!({
            "jsonrpc": "2.0", "id": 9_000 + number, "method": "session/request_permission",
            "params": {
                "sessionId": "sess_fake",
                "toolCall": {
                    "toolCallId": format!("call_{number}"), "kind": "execute",
                    "title": format!("ask {number}")
                },
                "options": [
                    { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
                    { "optionId": "reject", "name": "Reject", "kind": "reject_once" },
                ],
            }
        })
        .to_string()
    }
}

/// A session parks at most `max_pending_requests` questions. An agent that raises more at once
/// has the first ones shown to the host and every one past the cap answered at once with ACP's
/// `cancelled` outcome: a result, not a JSON-RPC error, so the agent's tool call returns
/// instead of waiting on a question nobody will see. The turn carries on.
#[tokio::test]
async fn questions_past_the_cap_are_withdrawn_at_once_and_the_rest_reach_the_host() {
    let (session, launcher) = open_under(
        AskingPeer::default().process(),
        Limits {
            max_pending_requests: PARKED_CAP,
            ..Limits::default()
        },
    )
    .await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "ask a lot"))
        .await
        .expect("expected a turn");

    let mut asked = Vec::new();
    while asked.len() < PARKED_CAP {
        let seen = read_until(&mut turn, "an approval request", |kind| {
            matches!(kind, EventKind::ApprovalRequested { .. })
        })
        .await;
        if let Some(EventKind::ApprovalRequested { request }) = seen.last() {
            asked.push(request.title.clone());
        }
    }
    // Each question is announced by a task of its own, so the two can reach the host in
    // either order; which two they are is what the cap decides.
    asked.sort();
    let expected = ["ask 1", "ask 2"];
    assert_eq!(
        asked, expected,
        "expected the questions the host was asked: {expected:?} | received: {asked:?}"
    );
    let refused = [
        "outcome cancelled for 9003",
        "outcome cancelled for 9004",
        "outcome cancelled for 9005",
    ];
    eventually("the questions past the cap answered", &launcher, || {
        (wire_after(&launcher, "session/prompt").len() >= refused.len()).then_some(())
    })
    .await;
    assert_wire_after(&launcher, "session/prompt", &refused);

    session
        .cancel(CancelReason::Requested)
        .await
        .expect("expected the cancel to be queued");
    let rest = event_labels(&drain(&mut turn).await);
    let received = (
        rest.iter()
            .filter(|label| *label == "ApprovalRequested")
            .count(),
        rest.last().map(String::as_str),
    );
    let expected = (0, Some("Completed"));
    assert_eq!(
        received, expected,
        "expected (further approval requests, last event): {expected:?} | received: {received:?} in {rest:?}"
    );
    // The two parked questions leave a map, so their withdrawals can be written in either
    // order; `session/cancel` follows both.
    let mut settled = wire_after(&launcher, "outcome cancelled for 9005");
    let last = settled.pop().unwrap_or_default();
    settled.sort();
    settled.push(last);
    let expected = [
        "outcome cancelled for 9001",
        "outcome cancelled for 9002",
        "session/cancel",
    ];
    assert_eq!(
        settled, expected,
        "expected the parked questions withdrawn, sorted, and then the last frame: {expected:?} | received: {settled:?}"
    );
}
