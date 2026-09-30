//! What a host is told when the core refuses to publish something the app-server announced.
//!
//! The library treats every vendor id as untrusted: an id that is blank, longer than the bound, or
//! containing characters the core refuses is refused rather than shortened or repaired, because a
//! changed id would name a different object. Dropping the event in silence leaves a host rendering
//! a turn that is missing an activity, so the turn fails with a code that names the cause instead.

// Shared with the replay suite; this binary uses only the intercepting replay.
#[allow(dead_code)]
mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use mango_agent_codex::CodexHarness;
use mango_external_agents::testing::{Announcer, FakeLauncher};
use mango_external_agents::{
    CloseReason, EnvSource, Error, EventKind, Harness, HostContext, Limits, OpenSession, Session,
    Steer, SteerOutcome, SteerRejection, TurnId, TurnRequest,
};
use serde_json::{Value, json};
use support::{Transcript, workspace_path};

fn host(launcher: Arc<FakeLauncher>, limits: Limits) -> HostContext {
    HostContext::builder()
        .launcher(launcher)
        .cwd(workspace_path())
        .client_info("mango-test", "0.0.1")
        .environment(EnvSource::from_pairs([
            ("PATH", "/usr/bin"),
            ("CODEX_HOME", "/home/user/.codex"),
        ]))
        .limits(limits)
        .build()
        .expect("expected a valid host context")
}

/// Names the events a host receives, for a failure message.
fn shape(events: &[EventKind]) -> Vec<String> {
    events
        .iter()
        .map(|kind| match kind {
            EventKind::ActivityStarted { call_id, .. } => {
                format!("ActivityStarted({} chars)", call_id.chars().count())
            }
            EventKind::Error { error } => format!("Error({})", error.code.as_str()),
            other => format!("{other:?}"),
        })
        .collect()
}

/// A reasoning phase, then a command announced under `id`, as the app-server writes them.
fn reasoning_then_command(thread: &str, id: &str) -> Vec<String> {
    vec![
        json!({"method": "item/started", "params": {"threadId": thread,
            "turnId": "real-turn", "item": {"type": "reasoning", "id": "r1",
            "summary": [], "content": []}}})
        .to_string(),
        json!({"method": "item/started", "params": {"threadId": thread,
            "turnId": "real-turn", "item": {"type": "commandExecution", "id": id,
            "command": "ls", "status": "inProgress"}}})
        .to_string(),
    ]
}

/// One turn against a fake app-server, kept open so a test can look at the session afterwards.
struct Run {
    session: Arc<dyn Session>,
    events: Vec<EventKind>,
    interrupts: Arc<AtomicUsize>,
    steers: Arc<AtomicUsize>,
    announcer: Announcer,
    held_interrupt: Arc<Mutex<Option<Value>>>,
    thread: String,
}

impl Run {
    /// Starts a turn whose start answer is followed by `frames`, then delivers `paced` one line at
    /// a time with a pause between them, so the handler keeps up with the connection. With
    /// `hold_interrupt` the server takes a `turn/interrupt` and says nothing until
    /// [`Self::report_turn_over`].
    async fn start(
        frames: impl Fn(&str) -> Vec<String> + Send + Sync + 'static,
        paced: impl FnOnce(&str) -> Vec<String>,
        limits: Limits,
        hold_interrupt: bool,
    ) -> Self {
        let transcript = Transcript::load("turn");
        let thread = transcript
            .thread_id()
            .expect("expected the recording to name its thread");
        let interrupts = Arc::new(AtomicUsize::new(0));
        let steers = Arc::new(AtomicUsize::new(0));
        let held_interrupt = Arc::new(Mutex::new(None::<Value>));
        let announcer = Announcer::new();
        let launcher = Arc::new(FakeLauncher::new());
        let (counted, steered, held, own) = (
            Arc::clone(&interrupts),
            Arc::clone(&steers),
            Arc::clone(&held_interrupt),
            thread.clone(),
        );
        launcher.push(
            transcript
                .as_process_intercepting(move |frame| {
                    let request = frame["id"].clone();
                    match frame.get("method").and_then(Value::as_str) {
                        Some("turn/start") => {
                            let mut lines = vec![
                                json!({"id": request, "result": {"turn": {"id": "real-turn"}}})
                                    .to_string(),
                            ];
                            lines.extend(frames(&own));
                            Some(lines)
                        }
                        Some("turn/steer") => {
                            steered.fetch_add(1, Ordering::SeqCst);
                            Some(vec![
                                json!({"id": request, "result": {"turnId": "real-turn"}})
                                    .to_string(),
                            ])
                        }
                        Some("turn/interrupt") => {
                            counted.fetch_add(1, Ordering::SeqCst);
                            if hold_interrupt {
                                *held.lock().unwrap_or_else(PoisonError::into_inner) =
                                    Some(request);
                                return Some(Vec::new());
                            }
                            Some(vec![
                                json!({"id": request, "result": {}}).to_string(),
                                json!({"method": "turn/completed", "params": {"threadId": own,
                                    "turn": {"id": "real-turn", "status": "interrupted"}}})
                                .to_string(),
                            ])
                        }
                        _ => None,
                    }
                })
                .announcing(announcer.clone()),
        );
        let session: Arc<dyn Session> = Arc::from(
            CodexHarness::new()
                .open_session(&host(launcher, limits), OpenSession::new("chat-1"))
                .await
                .expect("expected a session"),
        );
        let mut turn = session
            .start_turn(TurnRequest::new("host-turn", "hello"))
            .await
            .expect("expected the turn to start");
        for line in paced(&thread) {
            announcer.announce(line);
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        let mut events = Vec::new();
        // A turn that never ends is the failure under test, so a quiet stream is read as "no
        // terminal yet" and reported with what did arrive, not as a timeout.
        while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(3), turn.recv()).await
        {
            events.push(event.kind);
        }
        Self {
            session,
            events,
            interrupts,
            steers,
            announcer,
            held_interrupt,
            thread,
        }
    }

    /// Waits, bounded, until the session has sent a `turn/interrupt`.
    async fn until_interrupted(&self) -> usize {
        for _ in 0..300 {
            if self.interrupts.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        self.interrupts.load(Ordering::SeqCst)
    }

    /// Answers a held interrupt and reports the turn over, as Codex does once it has stopped.
    fn report_turn_over(&self) {
        let request = self
            .held_interrupt
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .expect("expected the server to hold an interrupt to answer");
        self.announcer
            .announce(json!({"id": request, "result": {}}).to_string());
        self.announcer.announce(
            json!({"method": "turn/completed", "params": {"threadId": self.thread,
                "turn": {"id": "real-turn", "status": "interrupted"}}})
            .to_string(),
        );
    }

    async fn close(&self) {
        self.session
            .close(CloseReason::Shutdown)
            .await
            .expect("expected the session to close");
    }
}

/// The terminal's error code, when the turn ended in a failure.
fn failure_code(events: &[EventKind]) -> Option<&str> {
    match events.last() {
        Some(EventKind::Error { error }) => Some(error.code.as_str()),
        _ => None,
    }
}

async fn assert_refused_and_interrupted(id: String, what: &str) {
    let run = Run::start(
        move |thread| reasoning_then_command(thread, &id),
        |_| Vec::new(),
        Limits::default(),
        false,
    )
    .await;
    let shape = shape(&run.events);
    assert_eq!(
        failure_code(&run.events),
        Some("codex-refused-event"),
        "expected {what} to fail the turn with codex-refused-event | received {shape:?}"
    );
    assert!(
        !run.events
            .iter()
            .any(|kind| matches!(kind, EventKind::ActivityStarted { .. })),
        "expected no activity the core refused to publish for {what} | received {shape:?}"
    );
    assert!(
        run.events
            .iter()
            .any(|kind| matches!(kind, EventKind::ReasoningEnded)),
        "expected the open reasoning phase closed before the failure for {what} | received {shape:?}"
    );
    let interrupts = run.until_interrupted().await;
    assert_eq!(
        interrupts, 1,
        "expected the native turn to be interrupted once after {what} | received {interrupts} interrupts in {shape:?}"
    );
    run.close().await;
}

#[tokio::test]
async fn an_activity_id_longer_than_the_bound_fails_the_turn_with_a_named_code() {
    assert_refused_and_interrupted("x".repeat(300), "a 300 character id").await;
}

#[tokio::test]
async fn a_blank_activity_id_fails_the_turn_the_same_way() {
    assert_refused_and_interrupted(String::from("   "), "a blank id").await;
}

#[tokio::test]
async fn an_activity_id_with_characters_the_core_refuses_fails_the_turn_the_same_way() {
    // A bidirectional override is stripped from text, and an id is refused instead of repaired.
    assert_refused_and_interrupted(
        String::from("call\u{202E}-1"),
        "an id containing a bidirectional override",
    )
    .await;
}

/// The failure is committed on the stream at once, but the native turn is still running until
/// Codex says otherwise, so a new turn must not be admitted on top of it.
#[tokio::test]
async fn admission_stays_held_until_codex_reports_the_turn_over() {
    let run = Run::start(
        |thread| reasoning_then_command(thread, &"x".repeat(300)),
        |_| Vec::new(),
        Limits::default(),
        true,
    )
    .await;
    let shape = shape(&run.events);
    assert_eq!(
        failure_code(&run.events),
        Some("codex-refused-event"),
        "expected the turn to fail on the refused id | received {shape:?}"
    );
    assert_eq!(
        run.until_interrupted().await,
        1,
        "expected the native turn to be interrupted | received {shape:?}"
    );

    let while_running = run
        .session
        .start_turn(TurnRequest::new("second-turn", "again"))
        .await;
    assert!(
        matches!(&while_running, Err(error) if matches!(error.cause(), Error::Busy)),
        "expected a second turn to be refused as Busy while Codex has not reported the first over | received {:?}",
        while_running.as_ref().map(|_| "an accepted turn")
    );

    run.report_turn_over();
    let mut last = String::from("no attempt made");
    let mut admitted = false;
    for _ in 0..200 {
        match run
            .session
            .start_turn(TurnRequest::new("third-turn", "again"))
            .await
        {
            Err(error) if matches!(error.cause(), Error::Busy) => {
                last = String::from("Busy");
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            other => {
                last = format!("{:?}", other.as_ref().map(|_| "an accepted turn"));
                admitted = true;
                break;
            }
        }
    }
    assert!(
        admitted,
        "expected admission to be released once Codex reported the turn over | last start result: {last}"
    );
    run.close().await;
}

/// A delta for the session's own turn, as the app-server writes it.
fn delta(thread: &str, round: usize) -> String {
    json!({"method": "item/agentMessage/delta", "params": {"threadId": thread,
        "turnId": "real-turn", "itemId": "m", "delta": format!("chunk {round}")}})
    .to_string()
}

/// A transcript that fills its queue while the host reads nothing ends in the sink's own
/// `stream-overflow`, not in a refused event and not in a call failure. The frames arrive one at a
/// time, as they do from a working app-server, so the connection's own bound is never the one hit.
#[tokio::test]
async fn a_transcript_that_overflows_ends_in_stream_overflow() {
    let limits = Limits {
        turn_channel_capacity: 16,
        ..Limits::default()
    };
    let run = Run::start(
        |_| Vec::new(),
        |thread| (0..40).map(|round| delta(thread, round)).collect(),
        limits,
        false,
    )
    .await;
    let shape = shape(&run.events);
    assert_eq!(
        failure_code(&run.events),
        Some("stream-overflow"),
        "expected the overflow the sink committed to be the terminal | received {shape:?}"
    );
    assert_eq!(
        run.events
            .iter()
            .filter(|kind| matches!(kind, EventKind::Error { .. }))
            .count(),
        1,
        "expected exactly one failure | received {shape:?}"
    );
    run.close().await;
}

/// The connection bounds the notifications it holds for a busy handler by the same limit. A burst
/// that passes it ends the connection, and the turn with it, under the call-failure code and the
/// connection-terminated vendor code, before the sink's queue is ever full.
#[tokio::test]
async fn a_burst_past_the_connections_own_bound_ends_the_connection_not_the_stream() {
    let limits = Limits {
        turn_channel_capacity: 8,
        ..Limits::default()
    };
    let run = Run::start(
        |thread| (0..40).map(|round| delta(thread, round)).collect(),
        |_| Vec::new(),
        limits,
        false,
    )
    .await;
    let shape = shape(&run.events);
    assert_eq!(
        failure_code(&run.events),
        Some("codex-call-failed"),
        "expected a burst past the connection's bound to end the connection | received {shape:?}"
    );
    let Some(EventKind::Error { error }) = run.events.last() else {
        panic!("expected a failure terminal | received {shape:?}");
    };
    assert_eq!(
        error.vendor_code.as_deref(),
        Some("connection-terminated"),
        "expected the connection-terminated vendor code | received {:?}",
        error.vendor_code
    );
    run.close().await;
}

/// An approval the host was shown and has not answered, then a refused event.
fn approval_request(thread: &str) -> String {
    json!({"id": 900, "method": "item/commandExecution/requestApproval",
        "params": {"threadId": thread, "turnId": "real-turn", "itemId": "c0", "command": "pwd"}})
    .to_string()
}

/// The host must not be left holding a prompt that never resolves: what the turn asked is settled
/// before the failure that ends it, as it is before an ordinary terminal.
#[tokio::test]
async fn a_pending_approval_is_resolved_before_the_refusal_failure() {
    let run = Run::start(
        |_| Vec::new(),
        |thread| {
            let mut lines = vec![approval_request(thread)];
            lines.extend(reasoning_then_command(thread, &"x".repeat(300)));
            lines
        },
        Limits::default(),
        false,
    )
    .await;
    let shape = shape(&run.events);
    let requested = run
        .events
        .iter()
        .position(|kind| matches!(kind, EventKind::ApprovalRequested { .. }));
    let resolved = run
        .events
        .iter()
        .position(|kind| matches!(kind, EventKind::ApprovalResolved { .. }));
    let failure = run
        .events
        .iter()
        .position(|kind| matches!(kind, EventKind::Error { .. }));
    assert!(
        requested.is_some() && resolved.is_some() && resolved < failure,
        "expected the approval resolved before the failure | received {shape:?}"
    );
    assert_eq!(
        failure_code(&run.events),
        Some("codex-refused-event"),
        "expected the turn to end in codex-refused-event | received {shape:?}"
    );
    run.close().await;
}

/// The stream has ended, so the turn is no longer one the host can steer, even while Codex has
/// not yet reported it over.
#[tokio::test]
async fn a_steer_after_the_refusal_failure_is_rejected_without_reaching_codex() {
    let run = Run::start(
        |thread| reasoning_then_command(thread, &"x".repeat(300)),
        |_| Vec::new(),
        Limits::default(),
        true,
    )
    .await;
    let shape = shape(&run.events);
    assert_eq!(
        failure_code(&run.events),
        Some("codex-refused-event"),
        "expected the turn to fail on the refused id | received {shape:?}"
    );
    let steered = run
        .session
        .steer(Steer {
            turn_id: TurnId::new("host-turn"),
            native_turn_id: String::from("real-turn"),
            input: String::from("also this"),
        })
        .await;
    assert!(
        matches!(
            &steered,
            Ok(SteerOutcome::Rejected {
                reason: SteerRejection::TurnAlreadyCompleted
            })
        ),
        "expected a steer after the failure to be rejected as already completed | received {steered:?}"
    );
    let reached = run.steers.load(Ordering::SeqCst);
    assert_eq!(
        reached, 0,
        "expected no turn/steer to reach Codex after the failure | received {reached}"
    );
    run.close().await;
}
