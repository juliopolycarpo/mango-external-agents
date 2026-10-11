//! The stretch that fails a turn from inside a notification call runs whole.
//!
//! [`report_refused_event`](super::report_refused_event) takes what the turn owes (its reducer,
//! its cancel reason, its parked questions), flushes the approval resolutions, closes what the
//! agent left running and commits the failure. The flush waits for the approval delivery gate,
//! which another task can hold, so the call can be parked between the take and the commit. The
//! JSON-RPC peer cancels a parked notification call when the agent passes an inbound budget and
//! when the connection is closed. Each test parks the call there, ends the connection one of
//! those two ways, and then lets the call go on.
//!
//! They go through a real connection, so the call under test is the peer's own notification
//! call and the hold it asks for is the peer's own.

use std::sync::Arc;
use std::time::{Duration, Instant};

use mango_external_agents::testing::{Announcer, FakeLauncher, FakeProcess};
use mango_external_agents::{
    ActivityStatus, AttemptId, CancelReason, Configuration, EventKind, EventReceiver, EventSink,
    HarnessIdentity, HostContext, LaunchSpec, Limits, ProcessLauncher, SessionIds, SessionSnapshot,
    TerminalStatus, TransportKind, TransportSelection, TurnId,
};

use super::{ConnectionHandle, SessionId, SessionState, TurnHandle, drive};

/// The most notifications the connection under test may hold for its handler.
const MESSAGE_BUDGET: usize = 4;

/// One running turn on a live connection to a fake agent the test speaks through.
struct RunningTurn {
    launcher: FakeLauncher,
    host: HostContext,
    state: Arc<SessionState>,
    connection: Arc<ConnectionHandle>,
    agent: Announcer,
    turn: TurnHandle,
    events: EventReceiver,
}

/// Polls `holds` until it is true, or fails naming `what`.
///
/// A bounded poll, not a settle window: it returns the moment the condition holds.
async fn until(what: &str, mut holds: impl FnMut() -> bool) {
    let begun = Instant::now();
    while !holds() {
        assert!(
            begun.elapsed() < Duration::from_secs(5),
            "expected {what} | received: not within 5s"
        );
        tokio::task::yield_now().await;
    }
}

/// A `session/update` line carrying `update`.
fn update_line(update: serde_json::Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0", "method": "session/update",
        "params": { "sessionId": "session-1", "update": update }
    })
    .to_string()
}

/// A tool call the agent leaves running under `id`.
fn running_call(id: &str) -> String {
    update_line(serde_json::json!({
        "sessionUpdate": "tool_call", "toolCallId": id, "title": "Run tests",
        "kind": "execute", "status": "in_progress",
    }))
}

/// One batch line of twice [`MESSAGE_BUDGET`] notifications, which no queue here can take.
fn flood() -> String {
    let heartbeat = serde_json::json!({ "jsonrpc": "2.0", "method": "$/vendor/heartbeat" });
    serde_json::Value::Array(vec![heartbeat; 2 * MESSAGE_BUDGET]).to_string()
}

impl RunningTurn {
    async fn start() -> Self {
        let agent = Announcer::new();
        let launcher = FakeLauncher::new();
        launcher.push(FakeProcess::responding(|_| Vec::new()).announcing(agent.clone()));
        let host = HostContext::builder()
            .launcher(Arc::new(launcher.clone()))
            .cwd(std::env::temp_dir())
            .client_info("acp-client-tests", "0.1.0")
            .limits(Limits {
                turn_channel_capacity: MESSAGE_BUDGET,
                max_pending_requests: 1,
                ..Limits::default()
            })
            .build()
            .expect("expected a host context");
        let core_state = mango_external_agents::SessionState::new(
            Arc::clone(host.clock()),
            SessionSnapshot::opening(
                SessionIds {
                    session_id: SessionId::new("session-1"),
                    native_session_id: String::from("native-1"),
                },
                HarnessIdentity::claude(),
                TransportSelection::new(None, TransportKind::Acp),
                std::time::SystemTime::UNIX_EPOCH,
            ),
        );
        let state = Arc::new(SessionState::new(
            SessionId::new("session-1"),
            &host,
            Configuration::default(),
            core_state,
        ));
        let process = launcher
            .spawn(LaunchSpec {
                argv: vec![String::from("fake-acp")],
                cwd: host.cwd().to_path_buf(),
                env: Default::default(),
                stdin: true,
                hide_window: true,
            })
            .await
            .expect("expected the fake child to launch");
        let launched = crate::transport::frame(process, &host).expect("expected a framed child");
        let connection = Arc::new(drive(launched, Arc::clone(&state)));
        // Wider than the connection's message budget, so the stream never refuses what the
        // refusal has to say.
        let (sink, events) = EventSink::new(
            SessionId::new("session-1"),
            TurnId::new("turn-1"),
            AttemptId::default(),
            Arc::clone(host.clock()),
            16,
        );
        let turn = state
            .begin_turn(sink, None)
            .expect("expected the turn to be admitted");
        Self {
            launcher,
            host,
            state,
            connection,
            agent,
            turn,
            events,
        }
    }

    /// Parks the notification call inside the stretch: the turn's reducer is taken, and the
    /// flush that follows waits on the delivery gate the returned guard holds.
    async fn park_the_refusal_mid_commit(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.agent.announce(running_call("call-0"));
        until("the running call reduced", || {
            self.state.open_call_count() == 1
        })
        .await;
        let delivery = self.turn.approvals.hold_delivery().await;
        // A blank call id is a value the core refuses to publish, which is what fails the turn.
        self.agent.announce(running_call("   "));
        until("the refusal to have taken the turn's reducer", || {
            self.state.open_call_count() == 0
        })
        .await;
        delivery
    }

    /// What the host was told, as the closes and the terminal only.
    fn closes_and_terminal(&mut self) -> Vec<String> {
        let mut sequence = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            sequence.push(match event.kind {
                EventKind::ActivityCompleted { call_id, result } => {
                    format!("completed {call_id} {:?}", result.status)
                }
                EventKind::Error { error } => format!("error {}", error.code.as_str()),
                _ => continue,
            });
        }
        sequence
    }

    /// The failure code the turn ended with, or what it is doing instead.
    fn outcome(&self) -> String {
        match self.events.terminal_status() {
            Some(TerminalStatus::Failed { code }) => code.as_str().to_owned(),
            Some(other) => format!("{other:?}"),
            None => String::from("no terminal"),
        }
    }

    /// Asserts the stretch ran whole: the call the agent left running closed as failed, then the
    /// refusal's own terminal.
    fn assert_the_refusal_was_committed(&mut self, after: &str) {
        let outcome = self.outcome();
        let received = self.closes_and_terminal();
        let expected = [
            format!("completed call-0 {:?}", ActivityStatus::Failed),
            String::from("error acp-refused-event"),
        ];
        assert_eq!(
            (outcome.as_str(), received.as_slice()),
            ("acp-refused-event", expected.as_slice()),
            "expected (terminal, closes and terminal) after {after}: (\"acp-refused-event\", {expected:?}) | received: ({outcome:?}, {received:?})"
        );
    }
}

/// An agent that passes the inbound message budget while the refusal is being committed ends the
/// connection, and the peer stops the handler's task without draining it. The commit is under a
/// hold, so that end waits for it: the host is told `acp-refused-event`, with the running call
/// closed first, and not left with a turn whose reducer was taken and whose terminal never came
/// from the call that took it.
#[tokio::test]
async fn an_inbound_overflow_during_the_refusal_commit_waits_for_it() {
    let mut running = RunningTurn::start().await;
    let delivery = running.park_the_refusal_mid_commit().await;

    running.agent.announce(flood());
    until("the overflow to have ended the connection", || {
        running.connection.overflow().is_some()
    })
    .await;
    drop(delivery);
    running.connection.ended().cancelled().await;

    running.assert_the_refusal_was_committed("an inbound overflow");
}

/// A close that arrives while the refusal is being committed cancels the handler's task. Under
/// the hold it waits for the commit first, within the time a close may take.
#[tokio::test]
async fn a_close_during_the_refusal_commit_waits_for_it() {
    let mut running = RunningTurn::start().await;
    let delivery = running.park_the_refusal_mid_commit().await;

    let closing = {
        let connection = Arc::clone(&running.connection);
        tokio::spawn(async move { connection.shutdown(CancelReason::Shutdown).await })
    };
    until("the close to have begun", || {
        running.connection.client.is_closed()
    })
    .await;
    drop(delivery);
    closing
        .await
        .expect("expected the closing task to join")
        .expect("expected the shutdown to succeed");

    running.assert_the_refusal_was_committed("a close");
}

/// A `session/request_permission` line under `id`, offering one way to allow and one to refuse.
fn ask_line(id: u32) -> String {
    serde_json::json!({
        "jsonrpc": "2.0", "id": id, "method": "session/request_permission",
        "params": {
            "sessionId": "session-1",
            "toolCall": { "toolCallId": "call-ask", "title": "Run tests" },
            "options": [
                { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
                { "optionId": "reject", "name": "Reject", "kind": "reject_once" },
            ],
        }
    })
    .to_string()
}

/// A host that answers a question after the connection ended under it, and before the turn's
/// owner has swept it, is not told the agent failed: the answer came too late, which is
/// `AlreadyResolved`, and the question it was shown is closed as withdrawn so its dialog does
/// not stay open.
#[tokio::test]
async fn a_host_answer_after_the_connection_ended_closes_the_question_as_withdrawn() {
    let mut running = RunningTurn::start().await;
    running.agent.announce(ask_line(77));
    let question = loop {
        let event = tokio::time::timeout(Duration::from_secs(5), running.events.recv())
            .await
            .expect("expected the question announced within 5s")
            .expect("expected an open stream");
        if let EventKind::ApprovalRequested { request } = event.kind {
            break request;
        }
    };

    running.agent.announce(flood());
    running.connection.ended().cancelled().await;

    let refusal = question.deny().expect("expected a refusing option");
    let answered = running.state.answer(&refusal);
    running
        .turn
        .approvals
        .flush(&running.turn.sink)
        .await
        .expect("expected the resolution delivered");
    let mut resolutions = Vec::new();
    while let Ok(event) = running.events.try_recv() {
        if let EventKind::ApprovalResolved { decision, .. } = event.kind {
            resolutions.push(format!("{:?}", decision.source));
        }
    }
    let received = (format!("{answered:?}"), resolutions);
    let expected = (
        String::from("Ok(AlreadyResolved)"),
        vec![String::from("Cancelled")],
    );
    assert_eq!(
        received, expected,
        "expected (the answer's result, resolutions shown to the host): {expected:?} | received: {received:?}"
    );
}

/// The same late answer once the turn itself is over, which is the answer a host gives in the
/// moment between the turn's terminal and the sweep of its questions: `Session::respond` sends
/// the agent `cancelled` there, and a connection that does not take it is not the agent's
/// failure either.
#[tokio::test]
async fn a_host_answer_after_the_turn_and_the_connection_ended_is_not_an_error() {
    let mut running = RunningTurn::start().await;
    running.agent.announce(ask_line(78));
    let question = loop {
        let event = tokio::time::timeout(Duration::from_secs(5), running.events.recv())
            .await
            .expect("expected the question announced within 5s")
            .expect("expected an open stream");
        if let EventKind::ApprovalRequested { request } = event.kind {
            break request;
        }
    };
    running.agent.announce(flood());
    running.connection.ended().cancelled().await;
    running.turn.finish();

    let refusal = question.deny().expect("expected a refusing option");
    let answered = running.state.answer(&refusal);
    running
        .turn
        .approvals
        .flush(&running.turn.sink)
        .await
        .expect("expected the resolution delivered");
    let mut resolutions = Vec::new();
    while let Ok(event) = running.events.try_recv() {
        if let EventKind::ApprovalResolved { decision, .. } = event.kind {
            resolutions.push(format!("{:?}", decision.source));
        }
    }
    let received = (format!("{answered:?}"), resolutions);
    let expected = (
        String::from("Ok(AlreadyResolved)"),
        vec![String::from("Cancelled")],
    );
    assert_eq!(
        received, expected,
        "expected (the answer's result, resolutions shown to the host): {expected:?} | received: {received:?}"
    );
}

/// A request made on a connection that has already ended is refused with what ended it, and
/// marked not submitted: none of it was queued, so a host may send it again elsewhere.
#[tokio::test]
async fn a_request_refused_by_an_ended_connection_is_marked_not_submitted() {
    let running = RunningTurn::start().await;
    running.agent.announce(flood());
    running.connection.ended().cancelled().await;

    let refused = super::submit(
        &running.connection,
        agent_client_protocol_schema::v1::ListSessionsRequest::new(),
    )
    .err()
    .expect("expected the request refused");
    let received = (
        matches!(
            refused.cause(),
            mango_external_agents::Error::LimitExceeded { .. }
        ),
        refused.dispatch(),
    );
    let expected = (true, mango_external_agents::Dispatch::NotSubmitted);
    assert_eq!(
        received, expected,
        "expected (the budget that ended the connection, dispatch): {expected:?} | received: {received:?} from {refused:?}"
    );
}

/// A `session/request_permission` line under `id` that offers no way to refuse.
fn ask_without_a_refusal(id: u32) -> String {
    serde_json::json!({
        "jsonrpc": "2.0", "id": id, "method": "session/request_permission",
        "params": {
            "sessionId": "session-1",
            "toolCall": { "toolCallId": "call-ask", "title": "Run tests" },
            "options": [{ "optionId": "allow", "name": "Allow", "kind": "allow_once" }],
        }
    })
    .to_string()
}

impl RunningTurn {
    /// Has the agent ask a question it cannot be refused, and returns it once the host has
    /// been shown it.
    async fn asked_without_a_refusal(
        &mut self,
        id: u32,
    ) -> mango_external_agents::PermissionRequest {
        self.agent.announce(ask_without_a_refusal(id));
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), self.events.recv())
                .await
                .expect("expected the question announced within 5s")
                .expect("expected an open stream");
            if let EventKind::ApprovalRequested { request } = event.kind {
                return request;
            }
        }
    }

    /// Makes the parked question `id` one whose time has run out.
    fn expire(&self, id: &str) {
        let now = std::time::SystemTime::now();
        self.state
            .lock_pending()
            .get_mut(id)
            .expect("expected the question parked")
            .deadline = mango_external_agents::approval::ApprovalDeadline::new(now, now)
            .expect("expected an elapsed deadline");
    }
}

/// A question that runs out of time with no refusing option stops the turn. When the
/// connection has ended and cannot carry `session/cancel`, that is not the agent's error and
/// not the caller's: the question was withdrawn, so the answer reads as too late, and the
/// failed cancel is recorded on the turn for its owner to report, as a host's own is.
#[tokio::test]
async fn an_expiry_cancel_the_connection_cannot_carry_is_recorded_on_the_turn() {
    let mut running = RunningTurn::start().await;
    let question = running.asked_without_a_refusal(81).await;
    running.agent.announce(flood());
    running.connection.ended().cancelled().await;
    running.expire(question.id().as_str());

    let answered = running.state.respond_pending(
        question.id().as_str(),
        crate::permission::cancelled(),
        mango_external_agents::DecisionSource::Expired,
    );
    let recorded = running
        .turn
        .cancel_failure
        .lock()
        .expect("expected the failure slot")
        .clone();
    let received = (
        format!("{answered:?}"),
        recorded,
        running.turn.cancellation.is_cancelled(),
    );
    let expected = (
        String::from("Ok(AlreadyResolved)"),
        Some(String::from("ACP session/cancel could not be queued")),
        true,
    );
    assert_eq!(
        received, expected,
        "expected (the result, the failure recorded on the turn, the turn told to stop): {expected:?} | received: {received:?}"
    );
}

/// The expiry's `session/cancel` belongs to the turn the question was asked in. When that turn
/// has ended and another runs by the time the cancel would be sent, nothing is sent and the
/// running turn is left alone: its questions stay parked and it is not told to stop.
#[tokio::test]
async fn an_expiry_cancel_is_not_sent_once_another_turn_is_running() {
    let mut running = RunningTurn::start().await;
    let question = running.asked_without_a_refusal(82).await;
    let expired = running
        .state
        .lock_pending()
        .remove(question.id().as_str())
        .expect("expected the question parked");

    running.state.release_turn_matching(&running.turn);
    let (sink, mut events) = EventSink::new(
        SessionId::new("session-1"),
        TurnId::new("turn-2"),
        AttemptId::default(),
        Arc::clone(running.host.clock()),
        16,
    );
    let next = running
        .state
        .begin_turn(sink, None)
        .expect("expected the next turn admitted");
    running.agent.announce(ask_line(83));
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("expected the next turn's question announced within 5s")
            .expect("expected an open stream");
        if matches!(event.kind, EventKind::ApprovalRequested { .. }) {
            break;
        }
    }

    running.state.cancel_expired(expired);

    // A marker queued after the call: whatever the call queued is written ahead of it.
    running
        .connection
        .client
        .submit_notification("x/marker", serde_json::json!({}))
        .expect("expected the marker queued")
        .await
        .expect("expected the marker written");
    let cancels = running
        .launcher
        .written()
        .iter()
        .filter(|line| line.contains("session/cancel"))
        .count();
    let received = (
        cancels,
        next.cancellation.is_cancelled(),
        running.state.pending_count(),
    );
    let expected = (0, false, 1);
    assert_eq!(
        received, expected,
        "expected (session/cancel frames written, the running turn told to stop, its questions parked): {expected:?} | received: {received:?}"
    );
}

/// A `session/update` whose tool call id is a number, which no schema this build knows reads.
fn unreadable_update() -> String {
    update_line(serde_json::json!({ "sessionUpdate": "tool_call", "toolCallId": 7 }))
}

/// Updates this client cannot read are skipped, not silent: the host is told once per turn, in
/// words that carry nothing of the frame. They are not progress, so only the update that can be
/// read restarts the idle deadline. The turn carries on and that update is reduced as usual.
#[tokio::test]
async fn unreadable_updates_are_announced_once_and_are_not_progress() {
    let mut running = RunningTurn::start().await;
    let before = *running.state.activity.borrow();
    for _ in 0..3 {
        running.agent.announce(unreadable_update());
    }
    running.agent.announce(running_call("call-0"));
    until("the readable update reduced", || {
        running.state.open_call_count() == 1
    })
    .await;

    let mut notices = Vec::new();
    while let Ok(event) = running.events.try_recv() {
        if let EventKind::Notice {
            severity, title, ..
        } = event.kind
        {
            notices.push(format!("{severity:?}: {title}"));
        }
    }
    let touches = running.state.activity.borrow().wrapping_sub(before);
    let received = (touches, notices, running.outcome());
    let expected = (
        1,
        vec![String::from(
            "Warning: The agent sent an update this client could not read",
        )],
        String::from("no terminal"),
    );
    assert_eq!(
        received, expected,
        "expected (frames counted as progress, notices shown, the turn's outcome): {expected:?} | received: {received:?}"
    );
}
