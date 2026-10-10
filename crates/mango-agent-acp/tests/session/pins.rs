//! Connection behaviour a host can observe, pinned through the crate's own surface.
//!
//! Every test here drives `AcpHarness` and a `Session` over the launcher fakes and reads only what
//! a host or an agent can see: the typed error, the turn's events, the session status, the lines
//! written to the agent's stdin and the number of live children. None of them names a type of the
//! JSON-RPC library underneath, so they hold whichever one carries the connection.

use super::*;

#[path = "pins/budgets.rs"]
mod budgets;
#[path = "pins/close.rs"]
mod close;
#[path = "pins/failures.rs"]
mod failures;
#[path = "pins/wire.rs"]
mod wire;

/// How long a poll may wait on the wall clock before it fails, paused runtime or not.
const POLL_BOUND: Duration = Duration::from_secs(10);

/// A host over `launcher` with these limits.
fn host_with(launcher: &FakeLauncher, limits: Limits) -> HostContext {
    HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .limits(limits)
        .build()
        .expect("expected a host")
}

/// Opens a session on `process` under `limits`, at the default permission level.
async fn open_under(process: FakeProcess, limits: Limits) -> (Box<dyn Session>, FakeLauncher) {
    let launcher = FakeLauncher::new();
    launcher.push(process);
    let session = AcpHarness::new(profile())
        .open_session(
            &host_with(&launcher, limits),
            OpenSession::new("chat-1").with_configuration(permissive()),
        )
        .await
        .unwrap_or_else(|error| panic!("expected a session | received: {error:?}"));
    (session, launcher)
}

/// Polls `probe` until it answers, or fails naming `what` and everything written so far.
///
/// A bounded poll, not a settle window: it returns the moment the condition holds. The bound is
/// wall-clock time so it also ends under a paused runtime, where a yield loop never lets the
/// virtual clock move.
async fn eventually<T>(
    what: &str,
    launcher: &FakeLauncher,
    mut probe: impl FnMut() -> Option<T>,
) -> T {
    let begun = std::time::Instant::now();
    loop {
        if let Some(found) = probe() {
            return found;
        }
        assert!(
            begun.elapsed() < POLL_BOUND,
            "expected {what} | received these written lines instead: {:?}",
            wire_labels(launcher)
        );
        tokio::task::yield_now().await;
    }
}

/// Starts a turn, retrying while the previous turn's slot is still being released.
///
/// A turn's terminal is published just ahead of the release of its slot, so a host that starts
/// the next turn the moment it reads `Completed` can still be told the session is busy.
async fn start_when_free(session: &dyn Session, turn_id: &str, input: &str) -> TurnStream {
    let begun = std::time::Instant::now();
    loop {
        match session.start_turn(TurnRequest::new(turn_id, input)).await {
            Ok(turn) => return turn,
            Err(error) if matches!(error.cause(), Error::Busy) && begun.elapsed() < POLL_BOUND => {
                tokio::task::yield_now().await;
            }
            Err(error) => panic!("expected {turn_id} to be admitted | received: {error:?}"),
        }
    }
}

/// Every line the client wrote to the agent, parsed. A line that is not JSON reads as `null`.
fn wire(launcher: &FakeLauncher) -> Vec<serde_json::Value> {
    launcher
        .written()
        .iter()
        .map(|line| serde_json::from_str(line).unwrap_or(serde_json::Value::Null))
        .collect()
}

/// One written frame as a short label: its method, or what kind of reply it is and to which id.
///
/// A permission answer shows its outcome, because the outcome is what a cancelling client owes.
fn wire_label(frame: &serde_json::Value) -> String {
    if let Some(method) = frame.get("method").and_then(serde_json::Value::as_str) {
        return method.to_owned();
    }
    let id = frame.get("id").cloned().unwrap_or(serde_json::Value::Null);
    if let Some(outcome) = frame
        .pointer("/result/outcome/outcome")
        .and_then(serde_json::Value::as_str)
    {
        return format!("outcome {outcome} for {id}");
    }
    match frame.pointer("/error/code") {
        Some(code) => format!("error {code} for {id}"),
        None => format!("result for {id}"),
    }
}

/// Every written frame as a [`wire_label`].
fn wire_labels(launcher: &FakeLauncher) -> Vec<String> {
    wire(launcher).iter().map(wire_label).collect()
}

/// The labels of everything written after the first frame whose label is `marker`.
fn wire_after(launcher: &FakeLauncher, marker: &str) -> Vec<String> {
    let labels = wire_labels(launcher);
    let Some(position) = labels.iter().position(|label| label == marker) else {
        panic!("expected a written {marker} frame | received: {labels:?}");
    };
    labels[position + 1..].to_vec()
}

/// Asserts the frames written after the first `marker` frame are exactly `expected`.
#[track_caller]
fn assert_wire_after(launcher: &FakeLauncher, marker: &str, expected: &[&str]) {
    let received = wire_after(launcher, marker);
    assert_eq!(
        received, expected,
        "expected frames after {marker}: {expected:?} | received: {received:?}"
    );
}

/// Asserts the first frame written after the first `marker` frame is `expected`.
#[track_caller]
fn assert_first_frame_after(launcher: &FakeLauncher, marker: &str, expected: &str) {
    let received = wire_after(launcher, marker);
    assert_eq!(
        received.first().map(String::as_str),
        Some(expected),
        "expected the first frame after {marker}: {expected:?} | received: {received:?}"
    );
}

/// Asserts `events`, as [`event_labels`], are exactly `expected`.
#[track_caller]
fn assert_events(what: &str, events: &[EventKind], expected: &[&str]) {
    let received = event_labels(events);
    assert_eq!(
        received, expected,
        "expected {what}: {expected:?} | received: {received:?}"
    );
}

/// Asserts the session's status is `expected`.
#[track_caller]
fn assert_status(what: &str, received: SessionStatus, expected: SessionStatus) {
    assert_eq!(
        received, expected,
        "expected session status {what}: {expected:?} | received: {received:?}"
    );
}

/// Asserts `error` is `LimitExceeded` with exactly this `(subject, limit, received)`.
#[track_caller]
fn assert_limit_exceeded(error: &Error, expected: (&str, usize, usize)) {
    let Error::LimitExceeded {
        subject,
        limit,
        received,
    } = error.cause()
    else {
        panic!(
            "expected LimitExceeded (subject, limit, received): {expected:?} | received: {error:?}"
        );
    };
    let received = (*subject, *limit, *received);
    assert_eq!(
        received, expected,
        "expected LimitExceeded (subject, limit, received): {expected:?} | received: {received:?}"
    );
}

/// Asserts no child of `launcher` is alive.
#[track_caller]
fn assert_no_live_children(what: &str, launcher: &FakeLauncher) {
    let received = launcher.live_children();
    assert_eq!(
        received, 0,
        "expected live children {what}: 0 | received: {received}"
    );
}

/// Asserts the frames written after `marker` are the request `method` and then one
/// `$/cancel_request` naming that request's own id, and nothing else.
///
/// This is what an abandoned request leaves on the wire today: the client tells the agent which
/// request it stopped waiting for before it ends the connection.
#[track_caller]
fn assert_abandoned_request_was_cancelled_on_the_wire(
    launcher: &FakeLauncher,
    marker: &str,
    method: &str,
) {
    let frames = wire(launcher);
    let labels: Vec<String> = frames.iter().map(wire_label).collect();
    let Some(position) = labels.iter().position(|label| label == marker) else {
        panic!("expected a written {marker} frame | received: {labels:?}");
    };
    let after = &frames[position + 1..];
    let received: Vec<String> = after
        .iter()
        .map(|frame| match frame["method"].as_str() {
            Some("$/cancel_request") => format!(
                "$/cancel_request for {}",
                frame["params"]["requestId"] == after[0]["id"]
            ),
            _ => wire_label(frame),
        })
        .collect();
    let expected = [method.to_owned(), String::from("$/cancel_request for true")];
    assert_eq!(
        received, expected,
        "expected frames after {marker}, with whether the cancel names the request's id: {expected:?} | received: {received:?}"
    );
}

/// Each event as a short label: a text delta shows its text, a cancellation its reason and an
/// error its code.
fn event_labels(events: &[EventKind]) -> Vec<String> {
    events
        .iter()
        .map(|kind| match kind {
            EventKind::TurnStarted { .. } => String::from("TurnStarted"),
            EventKind::TextDelta { text } => format!("TextDelta({text})"),
            EventKind::Cancelled { reason } => format!("Cancelled({reason:?})"),
            EventKind::Error { error } => format!("Error({})", error.code.as_str()),
            EventKind::ApprovalRequested { .. } => String::from("ApprovalRequested"),
            EventKind::ApprovalResolved { .. } => String::from("ApprovalResolved"),
            EventKind::Completed => String::from("Completed"),
            other => format!("{other:?}").chars().take(60).collect(),
        })
        .collect()
}

/// Reads `turn` up to and including the first event `wanted` accepts, failing at a terminal.
async fn read_until(
    turn: &mut TurnStream,
    what: &str,
    mut wanted: impl FnMut(&EventKind) -> bool,
) -> Vec<EventKind> {
    let mut seen = Vec::new();
    while let Some(event) = turn.recv().await {
        let terminal = event.is_terminal();
        let found = wanted(&event.kind);
        seen.push(event.kind);
        if found {
            return seen;
        }
        assert!(
            !terminal,
            "expected {what} before the turn ended | received: {:?}",
            event_labels(&seen)
        );
    }
    panic!(
        "expected {what} | received a stream that ended after: {:?}",
        event_labels(&seen)
    );
}

/// Reads `turn` to its terminal under a bound of the test's own, for a turn that ends later than
/// the shared `drain` allows. Fails naming what arrived when the bound passes first.
async fn drain_within(turn: &mut TurnStream, bound: Duration) -> Vec<EventKind> {
    let mut seen = Vec::new();
    let ended = tokio::time::timeout(bound, async {
        while let Some(event) = turn.recv().await {
            let terminal = event.is_terminal();
            seen.push(event.kind);
            if terminal {
                return;
            }
        }
    })
    .await;
    assert!(
        ended.is_ok(),
        "expected a terminal within {bound:?} | received: {:?}",
        event_labels(&seen)
    );
    seen
}

/// One text chunk as a `session/update` payload.
fn text_chunk(text: &str) -> serde_json::Value {
    serde_json::json!({
        "sessionUpdate": "agent_message_chunk",
        "content": { "type": "text", "text": text }
    })
}

/// A JSON-RPC success reply.
fn reply(id: &serde_json::Value, result: serde_json::Value) -> String {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

/// What a [`ScriptedPeer`] does with one frame it was written: the lines it writes back.
type Script = Arc<dyn Fn(&serde_json::Value) -> Vec<String> + Send + Sync>;

/// A named ACP peer that opens a session the way any agent does and hands every other line it
/// is written to a script, so one test can say exactly what the agent does next.
///
/// The script receives the parsed frame and returns the lines the agent writes back.
#[derive(Clone)]
struct ScriptedPeer {
    session_capabilities: serde_json::Value,
    script: Script,
}

impl ScriptedPeer {
    fn new(script: impl Fn(&serde_json::Value) -> Vec<String> + Send + Sync + 'static) -> Self {
        Self {
            session_capabilities: serde_json::json!({}),
            script: Arc::new(script),
        }
    }

    /// Advertises these `sessionCapabilities`, such as `{"close": {}}`.
    fn with_session_capabilities(mut self, capabilities: serde_json::Value) -> Self {
        self.session_capabilities = capabilities;
        self
    }

    fn process(&self) -> FakeProcess {
        let peer = self.clone();
        FakeProcess::responding(move |line| peer.answer(line))
    }

    fn answer(&self, line: &str) -> Vec<String> {
        let Ok(frame) = serde_json::from_str::<serde_json::Value>(line) else {
            return Vec::new();
        };
        let id = frame.get("id").cloned().unwrap_or(serde_json::Value::Null);
        match frame.get("method").and_then(serde_json::Value::as_str) {
            Some("initialize") => vec![reply(
                &id,
                serde_json::json!({
                    "protocolVersion": 1,
                    "agentInfo": { "name": "scripted-peer", "version": "1" },
                    "agentCapabilities": {
                        "loadSession": true,
                        "promptCapabilities": { "image": true, "embeddedContext": true },
                        "sessionCapabilities": self.session_capabilities,
                    },
                    "authMethods": [],
                }),
            )],
            Some("session/new") => {
                vec![reply(&id, serde_json::json!({ "sessionId": "sess_fake" }))]
            }
            _ => (self.script)(&frame),
        }
    }
}
