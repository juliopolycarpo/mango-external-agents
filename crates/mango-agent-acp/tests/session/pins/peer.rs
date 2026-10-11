//! What the connection does with traffic that is legal JSON-RPC and unusual: errors with a null
//! `data`, batches of responses, bursts past a cap that the client keeps up with.

use super::*;

/// A notification the client has no handler for, `padding` bytes of it in one string member.
fn unknown_notification(padding: usize) -> String {
    serde_json::json!({
        "jsonrpc": "2.0", "method": "$/vendor/heartbeat",
        "params": { "beat": "x".repeat(padding) }
    })
    .to_string()
}

/// A JSON-RPC error reply with these members under `error`.
fn error_reply(id: &serde_json::Value, error: serde_json::Value) -> String {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": error }).to_string()
}

/// A peer that advertises `session/list` and answers it with whatever `listing` returns.
fn listing_peer(
    listing: impl Fn(&serde_json::Value) -> Vec<String> + Send + Sync + 'static,
) -> FakeProcess {
    ScriptedPeer::new(move |frame| match frame["method"].as_str() {
        Some("session/list") => listing(frame),
        _ => Vec::new(),
    })
    .with_session_capabilities(serde_json::json!({ "list": {} }))
    .process()
}

/// An explicit `"data": null` on an agent's error says nothing, so the refusal reads as one
/// without data: the message is the agent's own, with nothing appended to it.
#[tokio::test]
async fn an_agents_refusal_with_null_data_keeps_its_message_alone() {
    let (session, _launcher) = open_under(
        listing_peer(|frame| {
            vec![error_reply(
                &frame["id"],
                serde_json::json!({ "code": -31_004, "message": "no workspace", "data": null }),
            )]
        }),
        Limits::default(),
    )
    .await;

    let error = refusal(session.list_sessions(Default::default()).await);
    let Error::Vendor(failure) = error.cause() else {
        panic!("expected Error::Vendor | received: {error:?}");
    };
    let received = (
        failure.code.as_str(),
        failure.request_id.as_deref(),
        failure.vendor_code.as_deref(),
        failure.message.as_str(),
    );
    let expected = (
        "acp-request-failed",
        Some("session/list"),
        Some("-31004"),
        "no workspace",
    );
    assert_eq!(
        received, expected,
        "expected (code, request_id, vendor_code, message): {expected:?} | received: {received:?}"
    );
}

/// A named peer that holds every `session/list` it is asked until it has `held` of them, then
/// answers them all in one JSON-RPC batch line.
#[derive(Clone)]
struct BatchingListPeer {
    held: usize,
    waiting: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl BatchingListPeer {
    fn answering_once_asked(held: usize) -> Self {
        Self {
            held,
            waiting: Arc::default(),
        }
    }

    fn process(&self) -> FakeProcess {
        let peer = self.clone();
        listing_peer(move |frame| peer.answer(frame))
    }

    fn answer(&self, frame: &serde_json::Value) -> Vec<String> {
        let mut waiting = self.waiting.lock().expect("expected the held requests");
        waiting.push(frame["id"].clone());
        if waiting.len() < self.held {
            return Vec::new();
        }
        let batch: Vec<serde_json::Value> = waiting
            .drain(..)
            .map(|id| {
                serde_json::json!({
                    "jsonrpc": "2.0", "id": id, "result": { "sessions": [] }
                })
            })
            .collect();
        vec![serde_json::Value::Array(batch).to_string()]
    }
}

/// Responses are not notifications: a batch of three answers to three admitted requests fits a
/// session whose turn channel holds one event, because an answer takes the place its own request
/// reserved and none of the notification budget.
#[tokio::test]
async fn a_batch_of_answers_to_admitted_requests_fits_a_one_event_turn_channel() {
    let (session, _launcher) = open_under(
        BatchingListPeer::answering_once_asked(3).process(),
        Limits {
            max_pending_requests: 4,
            turn_channel_capacity: 1,
            ..Limits::default()
        },
    )
    .await;

    let (first, second, third) = tokio::join!(
        session.list_sessions(Default::default()),
        session.list_sessions(Default::default()),
        session.list_sessions(Default::default()),
    );
    let received: Vec<Option<usize>> = [first, second, third]
        .iter()
        .map(|page| page.as_ref().ok().map(|page| page.sessions.len()))
        .collect();
    let expected = [Some(0), Some(0), Some(0)];
    assert_eq!(
        received, expected,
        "expected three empty pages from one batch of answers: {expected:?} | received: {received:?}"
    );
    assert_status(
        "after the batch",
        session.snapshot().status,
        SessionStatus::Ready,
    );
}

/// A larger outgoing budget buys nothing incoming: with `outbound_buffer_bytes` at four times
/// `turn_buffer_bytes`, a frame from the agent over `turn_buffer_bytes` still fails the request
/// it arrives under with the incoming byte budget.
#[tokio::test]
async fn an_outbound_override_does_not_raise_the_incoming_byte_budget() {
    const BUDGET: usize = 64 * 1024;
    let oversized = unknown_notification(BUDGET);
    let size = oversized.len();
    let launcher = FakeLauncher::new();
    launcher.push(listing_peer(move |_| vec![oversized.clone()]));
    let host = HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .limits(Limits {
            turn_buffer_bytes: BUDGET,
            ..Limits::default()
        })
        .outbound_buffer_bytes(4 * BUDGET)
        .build()
        .expect("expected a host");
    let session = AcpHarness::new(profile())
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");

    let error = refusal(session.list_sessions(Default::default()).await);
    assert_limit_exceeded(&error, ("bytes queued from the ACP agent", BUDGET, size));
}

/// The incoming byte budget is for what is queued, not for what has passed: three frames in a
/// row, each most of the budget, are all taken because each is handled before the next is read.
/// The request they arrive under is then answered.
#[tokio::test]
async fn bytes_the_client_has_handled_are_back_in_the_incoming_budget() {
    const BUDGET: usize = 64 * 1024;
    let large = unknown_notification(BUDGET * 3 / 4);
    let (session, _launcher) = open_under(
        listing_peer(move |frame| {
            vec![
                large.clone(),
                large.clone(),
                large.clone(),
                reply(&frame["id"], serde_json::json!({ "sessions": [] })),
            ]
        }),
        Limits {
            turn_buffer_bytes: BUDGET,
            ..Limits::default()
        },
    )
    .await;

    let listed = session.list_sessions(Default::default()).await;
    let received = listed.as_ref().map(|page| page.sessions.len()).ok();
    assert_eq!(
        received,
        Some(0),
        "expected the listing answered after three frames of 3/4 of the budget each: Some(0) | received: {received:?} from {listed:?}"
    );
}

/// How many notifications the keeping-up burst writes, as separate lines: several times the
/// 8-message budget it runs under.
const KEPT_UP_BURST: usize = 400;

/// The message budget bounds what waits for the client, not how much an agent may say. A burst
/// of single-line notifications fifty times the budget, written in one go, costs the session
/// nothing: each line is handled before the next is counted.
#[tokio::test]
async fn a_burst_of_lines_past_the_message_budget_is_handled_as_it_is_read() {
    let (session, _launcher) = open_under(
        listing_peer(|frame| {
            let mut lines: Vec<String> = (0..KEPT_UP_BURST)
                .map(|_| unknown_notification(8))
                .collect();
            lines.push(reply(&frame["id"], serde_json::json!({ "sessions": [] })));
            lines
        }),
        Limits {
            turn_channel_capacity: 8,
            max_pending_requests: 2,
            ..Limits::default()
        },
    )
    .await;

    let listed = session.list_sessions(Default::default()).await;
    let received = listed.as_ref().map(|page| page.sessions.len()).ok();
    assert_eq!(
        received,
        Some(0),
        "expected the listing answered behind {KEPT_UP_BURST} single-line notifications under an 8-message budget: Some(0) | received: {received:?} from {listed:?}"
    );
    assert_status(
        "after the burst",
        session.snapshot().status,
        SessionStatus::Ready,
    );
}

/// A request with a method the client does not know is refused with `-32601` on its own id even
/// when it arrives behind a long run of updates, and the turn carries every one of them.
#[tokio::test]
async fn an_unknown_request_behind_a_run_of_updates_is_still_refused_as_method_not_found() {
    const UPDATES: usize = 128;
    let ask = serde_json::json!({
        "jsonrpc": "2.0", "id": 999, "method": "session/unsupported",
        "params": { "sessionId": "sess_fake" }
    });
    let updates: Vec<serde_json::Value> = (0..UPDATES)
        .map(|index| text_chunk(&format!("chunk {index}")))
        .collect();
    let (session, launcher) = open(
        FakeAcpAgent::new()
            .with_updates(updates)
            .writing_mid_turn(UPDATES, ask.to_string()),
        permissive(),
    )
    .await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "say a lot"))
        .await
        .expect("expected a turn");

    let events = drain(&mut turn).await;
    let deltas = events
        .iter()
        .filter(|kind| matches!(kind, EventKind::TextDelta { .. }))
        .count();
    let received = (deltas, event_labels(&events).last().cloned());
    let expected = (UPDATES, Some(String::from("Completed")));
    assert_eq!(
        received, expected,
        "expected (text deltas, last event): {expected:?} | received: {received:?}"
    );
    eventually("the refusal on the wire", &launcher, || {
        (!wire_after(&launcher, "session/prompt").is_empty()).then_some(())
    })
    .await;
    assert_wire_after(&launcher, "session/prompt", &["error -32601 for 999"]);
}
