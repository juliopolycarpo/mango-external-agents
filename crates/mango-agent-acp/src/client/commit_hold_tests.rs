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
