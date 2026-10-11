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
//!
//! The last two tests are the other order: the end is decided before the call asks for its
//! hold, which is then refused, and the call must have taken nothing.

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

/// What a fake agent and a session state bound to it are made of, before a connection is driven.
struct Launch {
    launcher: FakeLauncher,
    host: HostContext,
    state: Arc<SessionState>,
    agent: Announcer,
    framed: crate::transport::LaunchedAgent,
}

/// Launches the fake agent and builds the session state its updates are for.
async fn launch() -> Launch {
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
    let framed = crate::transport::frame(process, &host).expect("expected a framed child");
    Launch {
        launcher,
        host,
        state,
        agent,
        framed,
    }
}

/// Admits the turn the agent's updates belong to. The stream is wider than the connection's
/// message budget, so it never refuses what a refusal has to say.
fn begin_turn(host: &HostContext, state: &SessionState) -> (TurnHandle, EventReceiver) {
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
    (turn, events)
}

impl RunningTurn {
    async fn start() -> Self {
        let Launch {
            launcher,
            host,
            state,
            agent,
            framed,
        } = launch().await;
        let connection = Arc::new(drive(framed, Arc::clone(&state)));
        let (turn, events) = begin_turn(&host, &state);
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

impl RunningTurn {
    /// A sink for one more turn on this session.
    fn next_sink(&self, turn_id: &str) -> EventSink {
        EventSink::new(
            SessionId::new("session-1"),
            TurnId::new(turn_id),
            AttemptId::default(),
            Arc::clone(self.host.clock()),
            16,
        )
        .0
    }
}

/// A turn that ended with an answer gives its slot back without waiting for any teardown. On
/// a connection that is still up the next turn is admitted; on one that ended behind the
/// answer, admission is closed before the slot is free, so the next turn is refused as closed
/// whether or not the lifecycle watcher has run yet.
#[tokio::test]
async fn a_slot_released_on_an_ended_connection_admits_no_further_turn() {
    let live = RunningTurn::start().await;
    super::release_turn(&live.state, &live.connection, &live.turn);
    let admitted = live.state.begin_turn(live.next_sink("turn-2"), None);

    let ended = RunningTurn::start().await;
    ended.agent.announce(flood());
    ended.connection.ended().cancelled().await;
    super::release_turn(&ended.state, &ended.connection, &ended.turn);
    let refused = ended.state.begin_turn(ended.next_sink("turn-2"), None);

    let received = (
        admitted.is_ok(),
        matches!(&refused, Err(mango_external_agents::Error::Closed { .. })),
    );
    assert_eq!(
        received,
        (true, true),
        "expected (next turn admitted on a live connection, refused as closed on an ended one): (true, true) | received: {received:?} from {:?}",
        refused.map(|_| "an installed turn")
    );
}

/// Parks every notification call ahead of the real handler, for as long as a test holds the
/// gate, so a test can decide the end of the connection inside a call that has begun and has
/// not yet asked for its hold.
struct ParkedBeforeTheCall {
    inner: super::AcpHandler,
    gate: Arc<tokio::sync::Mutex<()>>,
    /// Notification calls begun, counted before the gate.
    entered: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl mango_external_agents::jsonrpc::PeerHandler for ParkedBeforeTheCall {
    async fn on_notification(&self, method: String, params: serde_json::Value) {
        self.entered
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        drop(self.gate.lock().await);
        self.inner.on_notification(method, params).await;
    }

    async fn on_request_handoff(
        &self,
        method: String,
        params: serde_json::Value,
        responder: mango_external_agents::jsonrpc::Responder,
    ) {
        self.inner
            .on_request_handoff(method, params, responder)
            .await;
    }

    fn on_request_past_cap(
        &self,
        method: &str,
        params: &serde_json::Value,
        id: &mango_external_agents::jsonrpc::RequestId,
    ) -> Option<mango_external_agents::jsonrpc::ServerRequestOutcome> {
        self.inner.on_request_past_cap(method, params, id)
    }

    async fn on_terminated(&self, termination: mango_external_agents::jsonrpc::PeerTermination) {
        self.inner.on_terminated(termination).await;
    }
}

/// A turn running on a connection the test drives itself, through [`ParkedBeforeTheCall`].
struct GatedTurn {
    state: Arc<SessionState>,
    client: Arc<mango_external_agents::jsonrpc::Client>,
    agent: Announcer,
    turn: TurnHandle,
    events: EventReceiver,
    gate: Arc<tokio::sync::Mutex<()>>,
    entered: Arc<std::sync::atomic::AtomicUsize>,
    /// Kept so the fake child outlives the connection.
    _control: Arc<dyn mango_external_agents::ProcessControl>,
}

impl GatedTurn {
    /// The connection as `drive` makes it, with the handler wrapped.
    async fn start() -> Self {
        let Launch {
            host,
            state,
            agent,
            framed,
            ..
        } = launch().await;
        let gate = Arc::new(tokio::sync::Mutex::new(()));
        let entered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let handler = Arc::new(ParkedBeforeTheCall {
            inner: super::AcpHandler {
                state: Arc::clone(&state),
                ended: mango_external_agents::CancelToken::new(),
            },
            gate: Arc::clone(&gate),
            entered: Arc::clone(&entered),
        });
        let limits = *host.limits();
        let crate::transport::LaunchedAgent {
            link,
            control,
            outbound_buffer_bytes,
        } = framed;
        let client = Arc::new(mango_external_agents::jsonrpc::Client::connect_with(
            link,
            handler,
            crate::transport::client_options(&limits),
            crate::transport::wire_options(&limits, outbound_buffer_bytes),
        ));
        state.attach_peer(&client);
        let (turn, events) = begin_turn(&host, &state);
        Self {
            state,
            client,
            agent,
            turn,
            events,
            gate,
            entered,
            _control: control,
        }
    }

    /// Notification calls begun so far.
    fn calls_begun(&self) -> usize {
        self.entered.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Leaves a running call and a parked question for the refusal to take, then parks the next
    /// update, whose blank call id the core refuses, ahead of the real handler, decides the
    /// end and lets the call go on. It is then at the request for its hold, which never
    /// returns.
    async fn refused_at_its_hold(&mut self) {
        self.agent.announce(running_call("call-0"));
        until("the running call reduced", || {
            self.state.open_call_count() == 1
        })
        .await;
        self.agent.announce(ask_line(5));
        until("the question parked", || self.state.pending_count() == 1).await;

        let gate = Arc::clone(&self.gate).lock_owned().await;
        self.agent.announce(running_call("   "));
        until("the refused update's call begun", || {
            self.calls_begun() == 2
        })
        .await;
        let held = self.client.decide_cancellation();
        assert!(
            !held,
            "expected the end decided with no stretch held: false | received: {held}"
        );
        drop(gate);
        // The call took the gate when it was let go and drops it ahead of the real handler,
        // which runs on to its request for the hold in the same poll; taking the gate again
        // waits behind all of that.
        drop(self.gate.lock().await);
    }

    /// What the call left the turn: its running calls, its parked questions, its terminal.
    fn left(&self) -> (usize, usize, String) {
        let terminal = match self.events.terminal_status() {
            Some(status) => format!("{status:?}"),
            None => String::from("no terminal"),
        };
        (
            self.state.open_call_count(),
            self.state.pending_count(),
            terminal,
        )
    }

    /// The turn's owner settling it after the connection ended, as the prompt does: the
    /// questions still parked are withdrawn and their resolutions flushed, whatever the turn
    /// still owes closes as failed, then one terminal commits. The terminal is a stand-in with
    /// its own code: the owner's real cause is not what is under test. Returns what the host was
    /// told, resolutions, closes and terminal only.
    async fn settled_by_its_owner(&mut self) -> Vec<String> {
        if let Some((_, _, mut closing)) = self.state.prepare_terminal_matching(&self.turn) {
            let _ = self.turn.approvals.flush(&self.turn.sink).await;
            for kind in closing.finish_with(ActivityStatus::Failed) {
                let _ = self.turn.sink.emit(kind).await;
            }
        }
        let _ = self
            .turn
            .sink
            .fail(mango_external_agents::VendorError::new(
                mango_external_agents::ErrorCode::from_static("acp-link-closed"),
                "the connection ended under the turn",
            ))
            .await;
        let mut sequence = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            sequence.push(match event.kind {
                EventKind::ApprovalResolved { decision, .. } => {
                    format!("resolved {:?}", decision.source)
                }
                EventKind::ActivityCompleted { call_id, result } => {
                    format!("completed {call_id} {:?}", result.status)
                }
                EventKind::Error { error } => format!("error {}", error.code.as_str()),
                _ => continue,
            });
        }
        sequence
    }
}

/// What the host is told when the refusal's hold is refused: the call took nothing, so the
/// turn's owner still finds the running call to close and the question to withdraw, and the
/// host gets the owner's one terminal.
fn the_owners_settlement() -> Vec<String> {
    vec![
        String::from("resolved Cancelled"),
        format!("completed call-0 {:?}", ActivityStatus::Failed),
        String::from("error acp-link-closed"),
    ]
}

/// The end is decided before the refusal asks for its hold, and the agent then passes an
/// inbound budget. The call never gets past the ask, so it has taken nothing, and the
/// overflow ends the connection at once, with no stretch to wait for.
#[tokio::test(start_paused = true)]
async fn a_refusal_whose_hold_is_refused_takes_nothing_and_an_overflow_leaves_the_turn_to_its_owner()
 {
    let mut gated = GatedTurn::start().await;
    let grace = gated.state.limits.shutdown_timeout;
    gated.refused_at_its_hold().await;
    let before = gated.left();
    let expected = (1, 1, String::from("no terminal"));
    assert_eq!(
        before, expected,
        "expected (running calls, parked questions, terminal) after the hold was refused: {expected:?} | received: {before:?}"
    );

    let began = tokio::time::Instant::now();
    gated.agent.announce(flood());
    until("the overflow to have ended the connection", || {
        gated.client.ended().is_some()
    })
    .await;
    let ended_after = began.elapsed();
    let told = gated.settled_by_its_owner().await;
    let expected = the_owners_settlement();
    assert!(
        told == expected && ended_after < grace,
        "expected after the overflow: {expected:?} within {grace:?} | received: {told:?} after {ended_after:?}"
    );
}

/// The same for a close of the connection: its wait is for holds, and none was granted, so it
/// returns at once and the owner settles what the call left untouched.
#[tokio::test(start_paused = true)]
async fn a_refusal_whose_hold_is_refused_takes_nothing_and_a_close_leaves_the_turn_to_its_owner() {
    let mut gated = GatedTurn::start().await;
    let grace = gated.state.limits.shutdown_timeout;
    gated.refused_at_its_hold().await;
    let before = gated.left();
    let expected = (1, 1, String::from("no terminal"));
    assert_eq!(
        before, expected,
        "expected (running calls, parked questions, terminal) after the hold was refused: {expected:?} | received: {before:?}"
    );

    let began = tokio::time::Instant::now();
    let closed = gated.client.close().await;
    let closed_after = began.elapsed();
    let told = gated.settled_by_its_owner().await;
    let expected = the_owners_settlement();
    assert!(
        closed.is_ok() && told == expected && closed_after < grace,
        "expected after the close: Ok(()), {expected:?} within {grace:?} | received: {closed:?}, {told:?} after {closed_after:?}"
    );
}
