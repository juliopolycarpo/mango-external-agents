//! A hold asked for inside `on_request_handoff`, the call the worker awaits for a question the
//! peer asked: the ends of a connection that cancel the call wait for the hold as they do for a
//! notification call's.

use super::outbox_tests::{eventually, until};
use super::*;
use crate::jsonrpc::{CancellationHold, Responder, WireOptions};
use std::sync::atomic::AtomicUsize;
use std::sync::{Mutex as StdMutex, OnceLock, PoisonError, Weak};

/// What the handler under test does inside one `on_request_handoff` call.
#[derive(Clone, Copy, Default)]
struct Script {
    /// Asks for a hold around the stretch it must finish.
    hold: bool,
    /// Waits at a gate before asking for the hold.
    park_before_hold: bool,
    /// Keeps its hold past its own end and returns at once.
    keep_hold: bool,
    /// Closes its own client inside the stretch.
    close_inside: bool,
}

/// A handler that claims, commits and releases across awaits, inside the hand-off of a
/// question, and records how far each call got.
struct HandOffHolder {
    script: Script,
    client: OnceLock<Weak<Client>>,
    begun: AtomicUsize,
    asked_for_hold: AtomicUsize,
    granted: AtomicUsize,
    committed: AtomicUsize,
    /// Whether the hold granted said it was holding.
    holding: StdMutex<Vec<bool>>,
    kept: StdMutex<Vec<CancellationHold>>,
    before_hold: tokio::sync::Semaphore,
    inside: tokio::sync::Semaphore,
    terminations: StdMutex<Vec<PeerTermination>>,
    /// How many stretches had been committed when each termination was told.
    committed_at_termination: StdMutex<Vec<usize>>,
    terminated: Notify,
}

impl HandOffHolder {
    fn count(counter: &AtomicUsize) -> usize {
        counter.load(Ordering::Acquire)
    }

    /// (calls begun, holds granted, stretches committed)
    fn progress(&self) -> (usize, usize, usize) {
        (
            Self::count(&self.begun),
            Self::count(&self.granted),
            Self::count(&self.committed),
        )
    }

    async fn at_gate(gate: &tokio::sync::Semaphore) {
        gate.acquire()
            .await
            .expect("expected the gate to stay open for the whole test")
            .forget();
    }
}

#[async_trait::async_trait]
impl PeerHandler for HandOffHolder {
    async fn on_notification(&self, _method: String, _params: Value) {}

    async fn on_request_handoff(&self, _method: String, _params: Value, responder: Responder) {
        self.begun.fetch_add(1, Ordering::AcqRel);
        let client = || self.client.get().and_then(Weak::upgrade);
        if self.script.park_before_hold {
            Self::at_gate(&self.before_hold).await;
        }
        let hold = if self.script.hold {
            self.asked_for_hold.fetch_add(1, Ordering::AcqRel);
            let client = client().expect("expected the client to outlive the call's first step");
            let hold = client.hold_cancellation().await;
            self.granted.fetch_add(1, Ordering::AcqRel);
            self.holding
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(hold.is_holding());
            Some(hold)
        } else {
            None
        };
        if self.script.keep_hold {
            self.kept
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend(hold);
            let _ = responder.answer(ServerRequestOutcome::Answer(Value::Null));
            return;
        }
        if !self.script.close_inside {
            Self::at_gate(&self.inside).await;
        }
        if self.script.close_inside {
            let client = client().expect("expected the client to outlive the stretch");
            let _ = client.close().await;
        }
        self.committed.fetch_add(1, Ordering::AcqRel);
        drop(hold);
        let _ = responder.answer(ServerRequestOutcome::Answer(Value::Null));
    }

    async fn on_terminated(&self, termination: PeerTermination) {
        self.committed_at_termination
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Self::count(&self.committed));
        self.terminations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(termination);
        self.terminated.notify_one();
    }
}

const GRACE: Duration = Duration::from_millis(200);
const UPDATE: &str = r#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#;

fn ask() -> String {
    json!({ "jsonrpc": "2.0", "id": 1, "method": "ask", "params": {} }).to_string()
}

/// A client that hands the peer's questions over, and holds one notification for a handler
/// that is busy with a question.
fn connect(link: &ScriptedLink, script: Script) -> (Arc<HandOffHolder>, Arc<Client>) {
    let handler = Arc::new(HandOffHolder {
        script,
        client: OnceLock::new(),
        begun: AtomicUsize::new(0),
        asked_for_hold: AtomicUsize::new(0),
        granted: AtomicUsize::new(0),
        committed: AtomicUsize::new(0),
        holding: StdMutex::new(Vec::new()),
        kept: StdMutex::new(Vec::new()),
        before_hold: tokio::sync::Semaphore::new(0),
        inside: tokio::sync::Semaphore::new(0),
        terminations: StdMutex::new(Vec::new()),
        committed_at_termination: StdMutex::new(Vec::new()),
        terminated: Notify::new(),
    });
    let client = Arc::new(Client::connect_with(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        ClientOptions::new("ACP agent").with_max_pending_notifications(1),
        WireOptions::new().with_request_handoff(),
    ));
    let _ = handler.client.set(Arc::downgrade(&client));
    (handler, client)
}

/// Puts the handler inside its first hand-off, at the gate in its stretch.
async fn inside_the_stretch(link: &ScriptedLink, handler: &HandOffHolder) {
    link.push_line(ask());
    let wanted = usize::from(handler.script.hold);
    until("the handler to be inside its stretch", || {
        handler.progress().0 == 1 && handler.progress().1 == wanted
    })
    .await;
}

fn spawn_close(client: &Arc<Client>) -> tokio::task::JoinHandle<crate::Result<()>> {
    let client = Arc::clone(client);
    tokio::spawn(async move { client.close().await })
}

const HOLDING: Script = Script {
    hold: true,
    park_before_hold: false,
    keep_hold: false,
    close_inside: false,
};

/// A hold asked for inside a hand-off is granted: it says it is holding.
#[tokio::test(start_paused = true)]
async fn a_hold_asked_for_inside_a_hand_off_is_a_hold() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(&link, HOLDING);
    inside_the_stretch(&link, &handler).await;

    let holding = handler
        .holding
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    assert_eq!(
        holding,
        vec![true],
        "expected the hold asked for inside the hand-off to say it holds: [true] | received {holding:?}"
    );
    handler.inside.add_permits(1);
    client.close().await.expect("expected a clean close");
}

/// A close waits for a hand-off that holds its cancellation off, and is over when the stretch
/// is.
#[tokio::test(start_paused = true)]
async fn a_close_waits_for_a_stretch_a_hand_off_holds_its_cancellation_off() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(&link, HOLDING);
    inside_the_stretch(&link, &handler).await;

    let began = tokio::time::Instant::now();
    let closing = spawn_close(&client);
    tokio::time::sleep(GRACE / 2).await;
    let waiting = (closing.is_finished(), handler.progress());
    assert_eq!(
        waiting,
        (false, (1, 1, 0)),
        "expected the close waiting on the stretch: (false, (1, 1, 0)) | received {waiting:?}"
    );
    handler.inside.add_permits(1);
    eventually("the close", closing)
        .await
        .expect("expected the close task to finish")
        .expect("expected a clean close");
    let observed = (began.elapsed(), handler.progress());
    assert_eq!(
        observed,
        (GRACE / 2, (1, 1, 1)),
        "expected the close over when the stretch was committed: (100ms, (1, 1, 1)) | received {observed:?}"
    );
}

/// A hand-off that holds nothing is cancelled by a close at once, as a notification call is.
#[tokio::test(start_paused = true)]
async fn a_close_does_not_wait_for_a_hand_off_that_holds_nothing() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(
        &link,
        Script {
            hold: false,
            ..HOLDING
        },
    );
    inside_the_stretch(&link, &handler).await;

    let began = tokio::time::Instant::now();
    eventually("the close", client.close())
        .await
        .expect("expected a clean close");
    let observed = (began.elapsed(), handler.progress());
    assert_eq!(
        observed,
        (Duration::ZERO, (1, 0, 0)),
        "expected the close over at once, the call cancelled: (0s, (1, 0, 0)) | received {observed:?}"
    );
}

/// A stretch that never finishes costs a close one `shutdown_timeout`, and is cancelled then.
#[tokio::test(start_paused = true)]
async fn a_close_waits_one_shutdown_timeout_for_a_hand_off_that_never_finishes() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(&link, HOLDING);
    inside_the_stretch(&link, &handler).await;

    let began = tokio::time::Instant::now();
    eventually("the close", client.close())
        .await
        .expect("expected a clean close");
    let observed = (began.elapsed(), handler.progress());
    assert_eq!(
        observed,
        (GRACE, (1, 1, 0)),
        "expected the close over when the grace ran out, the stretch cancelled: (200ms, (1, 1, 0)) | received {observed:?}"
    );
}

/// The peer passes the notification budget while the handler is inside a hand-off it holds
/// the cancellation off for. The stretch is let finish, and the handler is told the connection
/// ended the moment it has.
#[tokio::test(start_paused = true)]
async fn an_overflow_waits_for_a_stretch_a_hand_off_holds_its_cancellation_off() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(&link, HOLDING);
    inside_the_stretch(&link, &handler).await;
    for _ in 0..3 {
        link.push_line(UPDATE);
    }
    until("the overflow to decide the cancellation", || {
        client.is_closed() && client.state.cancelling_handler.load(Ordering::SeqCst)
    })
    .await;

    tokio::time::sleep(GRACE / 2).await;
    let waiting = (
        handler.progress(),
        handler
            .terminations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len(),
    );
    assert_eq!(
        waiting,
        ((1, 1, 0), 0),
        "expected the stretch still in progress and no termination yet: ((1, 1, 0), 0) | received {waiting:?}"
    );

    let opened = tokio::time::Instant::now();
    handler.inside.add_permits(1);
    eventually("the termination", handler.terminated.notified()).await;
    let told = (
        opened.elapsed(),
        handler
            .terminations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone(),
        handler
            .committed_at_termination
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone(),
    );
    assert!(
        matches!(
            &told,
            (Duration::ZERO, terminations, committed)
                if matches!(&terminations[..], [PeerTermination::NotificationBackpressure { .. }])
                    && committed == &[1]
        ),
        "expected one termination, the moment the stretch was committed: (0s, [NotificationBackpressure], [1]) | received {told:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// A hold asked for once the cancellation has been decided is never granted: the call ends
/// there, before the stretch it wanted to finish.
#[tokio::test]
async fn a_hold_asked_for_in_a_hand_off_after_the_cancellation_was_decided_is_never_granted() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(
        &link,
        Script {
            park_before_hold: true,
            ..HOLDING
        },
    );
    link.push_line(ask());
    until("the handler to be inside its call", || {
        handler.progress().0 == 1
    })
    .await;

    // What a close and an overflow do first: decide, and look for a hold.
    let held = client.state.cancellation_held();
    assert!(
        !held,
        "expected no hold out when the cancellation was decided: false | received true"
    );
    handler.before_hold.add_permits(1);
    until("the handler to ask for its hold", || {
        HandOffHolder::count(&handler.asked_for_hold) == 1
    })
    .await;
    // Asking and being refused are one step of the handler's task, so it is refused by now.
    client.state.stop_notifications().await;
    let progress = handler.progress();
    assert_eq!(
        progress,
        (1, 0, 0),
        "expected the hold never granted and the stretch never begun: (1, 0, 0) | received {progress:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// A hold kept past the hand-off that was granted it holds nothing once that call has
/// returned: a close does not wait for it.
#[tokio::test(start_paused = true)]
async fn a_hold_kept_past_its_hand_off_holds_nothing_once_the_call_has_returned() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(
        &link,
        Script {
            keep_hold: true,
            ..HOLDING
        },
    );
    link.push_line(ask());
    until("the call to have returned and answered", || {
        !link.sent().is_empty()
    })
    .await;

    let began = tokio::time::Instant::now();
    eventually("the close", client.close())
        .await
        .expect("expected a clean close");
    let kept = handler
        .kept
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .len();
    let observed = (
        began.elapsed(),
        kept,
        client.state.holds.out.load(Ordering::SeqCst),
    );
    assert_eq!(
        observed,
        (Duration::ZERO, 1, 0),
        "expected the close over at once, the hold kept and counting for nothing: (0s, 1, 0) | received {observed:?}"
    );
}

/// A close made from inside a held hand-off cannot wait for the call it is made from, and does
/// not: it returns at once.
#[tokio::test(start_paused = true)]
async fn a_close_made_from_inside_a_held_hand_off_does_not_wait_for_it() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(
        &link,
        Script {
            close_inside: true,
            ..HOLDING
        },
    );
    let began = tokio::time::Instant::now();
    link.push_line(ask());
    until("the close made inside the stretch to have returned", || {
        client.is_closed() && handler.progress().0 == 1
    })
    .await;
    eventually("the client to finish closing", client.close())
        .await
        .expect("expected a clean close");

    let observed = (began.elapsed() < GRACE, handler.progress());
    assert_eq!(
        observed,
        (true, (1, 1, 0)),
        "expected a close made from inside the hand-off to take less than the grace, and to end the call at its await before the stretch was committed: (true, (1, 1, 0)) | received {observed:?} after {:?}",
        began.elapsed()
    );
}

/// A handler that only knows `on_request`, which the default `on_request_handoff` asks inline,
/// on the task that runs the notifications.
struct OnRequestHolder {
    client: OnceLock<Weak<Client>>,
    holding: StdMutex<Vec<bool>>,
    inside: tokio::sync::Semaphore,
    committed: AtomicUsize,
}

#[async_trait::async_trait]
impl PeerHandler for OnRequestHolder {
    async fn on_notification(&self, _method: String, _params: Value) {}

    async fn on_request(
        &self,
        _method: String,
        _params: Value,
        _id: RequestId,
    ) -> ServerRequestOutcome {
        let client = self
            .client
            .get()
            .and_then(Weak::upgrade)
            .expect("expected the client to outlive the call's first step");
        let hold = client.hold_cancellation().await;
        drop(client);
        self.holding
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(hold.is_holding());
        HandOffHolder::at_gate(&self.inside).await;
        self.committed.fetch_add(1, Ordering::AcqRel);
        drop(hold);
        ServerRequestOutcome::Answer(Value::Null)
    }
}

/// The `on_request` that the default hand-off asks is part of the hand-off call, so a hold
/// asked for in it is a hold: a close waits for the stretch.
#[tokio::test(start_paused = true)]
async fn a_hold_asked_for_in_the_on_request_the_default_hand_off_asks_is_a_hold() {
    let link = ScriptedLink::new();
    let handler = Arc::new(OnRequestHolder {
        client: OnceLock::new(),
        holding: StdMutex::new(Vec::new()),
        inside: tokio::sync::Semaphore::new(0),
        committed: AtomicUsize::new(0),
    });
    let client = Arc::new(Client::connect_with(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        ClientOptions::new("ACP agent"),
        WireOptions::new().with_request_handoff(),
    ));
    let _ = handler.client.set(Arc::downgrade(&client));
    link.push_line(ask());
    until("the handler to be inside its stretch", || {
        !handler
            .holding
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    })
    .await;

    let began = tokio::time::Instant::now();
    let closing = spawn_close(&client);
    tokio::time::sleep(GRACE / 2).await;
    let waiting = (
        closing.is_finished(),
        HandOffHolder::count(&handler.committed),
    );
    assert_eq!(
        waiting,
        (false, 0),
        "expected the close waiting on the stretch asked inside on_request: (false, 0) | received {waiting:?}"
    );
    handler.inside.add_permits(1);
    eventually("the close", closing)
        .await
        .expect("expected the close task to finish")
        .expect("expected a clean close");
    let holding = handler
        .holding
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let observed = (
        began.elapsed(),
        holding,
        HandOffHolder::count(&handler.committed),
    );
    assert_eq!(
        observed,
        (GRACE / 2, vec![true], 1),
        "expected the close over when the stretch was committed, the hold granted: (100ms, [true], 1) | received {observed:?}"
    );
}
