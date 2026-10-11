//! What becomes of a notification call in progress, and of the handler's `on_terminated`, when
//! the connection is cut short: by the peer passing a budget, by a close, by a drop.

use super::outbox_tests::{
    eventually, gated, limits, link_held, one_termination, options, until, within,
};
use super::*;
use crate::jsonrpc::{CancellationHold, HANDLING_NOTIFICATION_FOR, NotificationCall, WireOptions};
use std::sync::atomic::AtomicUsize;
use std::sync::{Mutex as StdMutex, OnceLock, PoisonError, Weak};

/// What the handler under test does inside one `on_notification` call.
#[derive(Clone, Copy, Default)]
struct Script {
    /// Waits at a gate before asking for the hold.
    park_before_hold: bool,
    /// Asks for a hold around the stretch it must finish.
    hold: bool,
    /// The stretch takes a few turns of the scheduler; otherwise it waits at a gate.
    quick: bool,
    /// The first call keeps its hold past its own end and returns at once.
    keep_first_hold: bool,
    /// Forgets its hold where it would have dropped it.
    forget_hold: bool,
    /// Asks for a second hold inside the stretch, as a helper it calls might.
    nested_hold: bool,
    /// Drops the nested hold where the stretch is committed and the outer one at the end of
    /// the call; otherwise the other way round.
    nested_dropped_first: bool,
    /// Hands its hold to someone else as soon as it has it, and goes on with the stretch.
    hold_elsewhere: bool,
    /// Asks for a hold of a second stretch once the first is committed and let go.
    second_stretch: bool,
    /// Panics inside the stretch.
    panic_inside: bool,
    /// Makes a request and queues a notification inside the stretch.
    ask_inside: bool,
    /// Closes its own client inside the stretch.
    close_inside: bool,
    /// Waits at a gate nobody opens once the stretch is done and the hold dropped.
    park_after: bool,
    /// Waits at a gate inside `on_terminated`.
    park_termination: bool,
    /// Closes its own client inside `on_terminated`.
    close_in_termination: bool,
}

/// A handler that claims, commits and releases across awaits, the way one that keeps a turn's
/// terminal does, and records how far each call got.
struct HoldingHandler {
    script: Script,
    client: OnceLock<Weak<Client>>,
    begun: AtomicUsize,
    /// Questions from the peer put to the handler.
    asked: AtomicUsize,
    /// Calls whose future is gone, returned or dropped part-way.
    ended: AtomicUsize,
    /// How many calls had ended when the handler was told the connection did.
    ended_at_termination: AtomicUsize,
    asked_for_hold: AtomicUsize,
    granted: AtomicUsize,
    committed: AtomicUsize,
    before_hold: tokio::sync::Semaphore,
    /// Holds kept past the call that asked for them.
    kept: StdMutex<Vec<CancellationHold>>,
    inside: tokio::sync::Semaphore,
    after: tokio::sync::Semaphore,
    /// What the calls made inside the stretch returned.
    outcomes: StdMutex<Vec<String>>,
    /// Each termination the handler was told, with how many stretches had been committed then.
    terminations: StdMutex<Vec<(PeerTermination, usize)>>,
    terminated: Notify,
    termination_gate: tokio::sync::Semaphore,
    terminations_returned: AtomicUsize,
    termination_returned: Notify,
}

impl HoldingHandler {
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

    fn terminations(&self) -> Vec<(PeerTermination, usize)> {
        self.terminations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    async fn at_gate(gate: &tokio::sync::Semaphore) {
        gate.acquire()
            .await
            .expect("expected the gate to stay open for the whole test")
            .forget();
    }
}

#[async_trait::async_trait]
impl PeerHandler for HoldingHandler {
    async fn on_notification(&self, _method: String, _params: Value) {
        let first = self.begun.fetch_add(1, Ordering::AcqRel) == 0;
        let _ended = CountOnDrop(&self.ended);
        // Taken for each use and let go again: a handler that kept its client across a wait
        // would keep it from ever being dropped.
        let client = || self.client.get().and_then(Weak::upgrade);
        if client().is_none() {
            return;
        }
        if self.script.park_before_hold {
            Self::at_gate(&self.before_hold).await;
        }
        let mut hold = if self.script.hold {
            self.asked_for_hold.fetch_add(1, Ordering::AcqRel);
            let client = client().expect("expected the client to outlive the call's first step");
            let hold = client.hold_cancellation().await;
            drop(client);
            self.granted.fetch_add(1, Ordering::AcqRel);
            Some(hold)
        } else {
            None
        };
        if first && self.script.keep_first_hold {
            self.kept
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend(hold);
            return;
        }
        if self.script.hold_elsewhere {
            self.kept
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend(hold.take());
        }
        if self.script.quick {
            for _ in 0..3 {
                tokio::task::yield_now().await;
            }
        } else {
            Self::at_gate(&self.inside).await;
        }
        let mut nested = if self.script.nested_hold {
            let client = client().expect("expected the client to outlive the stretch");
            Some(client.hold_cancellation().await)
        } else {
            None
        };
        assert!(
            !self.script.panic_inside,
            "the handler under test panics on purpose"
        );
        if self.script.ask_inside {
            let client = client().expect("expected the client to outlive the stretch");
            let asked = client.request::<_, Value>("session/list", json!({})).await;
            let queued = client.submit_notification("session/cancel", json!({}));
            self.outcomes
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend([format!("{asked:?}"), format!("{:?}", queued.map(drop))]);
        }
        if self.script.close_inside {
            let client = client().expect("expected the client to outlive the stretch");
            let _ = client.close().await;
        }
        self.committed.fetch_add(1, Ordering::AcqRel);
        if self.script.forget_hold {
            std::mem::forget(hold.take());
        } else if self.script.nested_dropped_first {
            drop(nested.take());
        } else {
            drop(hold.take());
        }
        if self.script.second_stretch {
            self.asked_for_hold.fetch_add(1, Ordering::AcqRel);
            let client = client().expect("expected the client to outlive the stretch");
            let _second = client.hold_cancellation().await;
            self.granted.fetch_add(1, Ordering::AcqRel);
        }
        if self.script.park_after {
            Self::at_gate(&self.after).await;
        }
        // Whichever of the two holds is still out goes with the call.
        drop((hold, nested));
    }

    async fn on_request(
        &self,
        _method: String,
        _params: Value,
        _id: RequestId,
    ) -> ServerRequestOutcome {
        self.asked.fetch_add(1, Ordering::AcqRel);
        ServerRequestOutcome::Answer(Value::Null)
    }

    async fn on_terminated(&self, termination: PeerTermination) {
        self.ended_at_termination
            .store(Self::count(&self.ended), Ordering::Release);
        let committed = Self::count(&self.committed);
        self.terminations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((termination, committed));
        self.terminated.notify_one();
        if self.script.park_termination {
            Self::at_gate(&self.termination_gate).await;
        }
        if self.script.close_in_termination
            && let Some(client) = self.client.get().and_then(Weak::upgrade)
        {
            let _ = client.close().await;
        }
        self.terminations_returned.fetch_add(1, Ordering::AcqRel);
        self.termination_returned.notify_one();
    }
}

/// Counts a call as ended when its future goes, however it goes.
struct CountOnDrop<'a>(&'a AtomicUsize);

impl Drop for CountOnDrop<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

const GRACE: Duration = Duration::from_millis(200);
const UPDATE: &str = r#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#;

fn connect_with(
    link: &ScriptedLink,
    script: Script,
    options: ClientOptions,
    wire: WireOptions,
) -> (Arc<HoldingHandler>, Arc<Client>) {
    let handler = Arc::new(HoldingHandler {
        script,
        client: OnceLock::new(),
        begun: AtomicUsize::new(0),
        asked: AtomicUsize::new(0),
        ended: AtomicUsize::new(0),
        ended_at_termination: AtomicUsize::new(usize::MAX),
        asked_for_hold: AtomicUsize::new(0),
        granted: AtomicUsize::new(0),
        committed: AtomicUsize::new(0),
        before_hold: tokio::sync::Semaphore::new(0),
        kept: StdMutex::new(Vec::new()),
        inside: tokio::sync::Semaphore::new(0),
        after: tokio::sync::Semaphore::new(0),
        outcomes: StdMutex::new(Vec::new()),
        terminations: StdMutex::new(Vec::new()),
        terminated: Notify::new(),
        termination_gate: tokio::sync::Semaphore::new(0),
        terminations_returned: AtomicUsize::new(0),
        termination_returned: Notify::new(),
    });
    let client = Arc::new(Client::connect_with(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options,
        wire,
    ));
    let _ = handler.client.set(Arc::downgrade(&client));
    (handler, client)
}

/// A client that holds one notification for a handler that is busy with another.
fn connect(link: &ScriptedLink, script: Script) -> (Arc<HoldingHandler>, Arc<Client>) {
    connect_with(
        link,
        script,
        ClientOptions::new("ACP agent").with_max_pending_notifications(1),
        WireOptions::new(),
    )
}

const HOLDING: Script = Script {
    park_before_hold: false,
    hold: true,
    quick: false,
    keep_first_hold: false,
    forget_hold: false,
    nested_hold: false,
    nested_dropped_first: false,
    hold_elsewhere: false,
    second_stretch: false,
    panic_inside: false,
    ask_inside: false,
    close_inside: false,
    park_after: false,
    park_termination: false,
    close_in_termination: false,
};

const NOT_HOLDING: Script = Script {
    hold: false,
    ..HOLDING
};

/// Puts the handler inside its first call, at the gate in its stretch.
async fn inside_the_stretch(link: &ScriptedLink, handler: &HoldingHandler) {
    link.push_line(UPDATE);
    let wanted = usize::from(handler.script.hold);
    until("the handler to be inside its stretch", || {
        handler.progress().0 == 1 && handler.progress().1 == wanted
    })
    .await;
}

/// The same, with the peer then passing the notification budget behind the call.
async fn overflow_behind_the_stretch(
    link: &ScriptedLink,
    handler: &HoldingHandler,
    client: &Client,
) {
    inside_the_stretch(link, handler).await;
    for _ in 0..3 {
        link.push_line(UPDATE);
    }
    // Closed is not yet decided: the reader raises the flag a step later.
    until("the overflow to decide the cancellation", || {
        client.is_closed() && client.state.cancelling_handler.load(Ordering::SeqCst)
    })
    .await;
}

fn spawn_close(client: &Arc<Client>) -> tokio::task::JoinHandle<crate::Result<()>> {
    let client = Arc::clone(client);
    tokio::spawn(async move { client.close().await })
}

/// The peer passes the notification budget while the handler is inside a stretch it holds the
/// cancellation off for. The stretch is let finish, and the handler is told the connection
/// ended the moment it has.
#[tokio::test(start_paused = true)]
async fn an_overflow_waits_for_a_stretch_that_holds_its_cancellation_off() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(&link, HOLDING);
    overflow_behind_the_stretch(&link, &handler, &client).await;

    tokio::time::sleep(GRACE / 2).await;
    let waiting = (handler.progress(), handler.terminations().len());
    assert_eq!(
        waiting,
        ((1, 1, 0), 0),
        "expected the stretch still in progress and no termination yet: ((1, 1, 0), 0) | received {waiting:?}"
    );

    let opened = tokio::time::Instant::now();
    handler.inside.add_permits(1);
    eventually("the termination", handler.terminated.notified()).await;
    let told = (opened.elapsed(), handler.terminations());
    assert!(
        matches!(
            &told,
            (Duration::ZERO, told)
                if matches!(&told[..], [(PeerTermination::NotificationBackpressure { .. }, 1)])
        ),
        "expected one termination, the moment the stretch was committed: (0s, [(NotificationBackpressure, 1)]) | received {told:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// The wait is for the stretch and for nothing behind it. The notification queued behind the
/// call is dropped with the connection, not begun once the call returns.
#[tokio::test(start_paused = true)]
async fn what_is_queued_behind_a_held_stretch_is_not_begun_after_it() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(&link, HOLDING);
    overflow_behind_the_stretch(&link, &handler, &client).await;

    handler.inside.add_permits(8);
    eventually("the termination", handler.terminated.notified()).await;
    eventually(
        "the handler's task to stop",
        client.state.worker_stopped.cancelled(),
    )
    .await;
    let progress = handler.progress();
    assert_eq!(
        progress,
        (1, 1, 1),
        "expected the one call in progress finished and the queued one never begun: (1, 1, 1) | received {progress:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// The same overflow with a call that holds nothing: cancelled where it stands and the handler
/// told at once, as it always was.
#[tokio::test(start_paused = true)]
async fn an_overflow_cancels_a_call_that_holds_nothing_at_once() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(&link, NOT_HOLDING);
    inside_the_stretch(&link, &handler).await;
    let began = tokio::time::Instant::now();
    for _ in 0..3 {
        link.push_line(UPDATE);
    }

    eventually("the termination", handler.terminated.notified()).await;
    let observed = (began.elapsed(), handler.terminations(), handler.progress());
    assert!(
        matches!(
            &observed,
            (Duration::ZERO, told, (1, 0, 0))
                if matches!(&told[..], [(PeerTermination::NotificationBackpressure { .. }, 0)])
        ),
        "expected the handler told at once with its call cancelled: (0s, [(NotificationBackpressure, 0)], (1, 0, 0)) | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// A hold is a bound, not a promise. A stretch that does not finish within `shutdown_timeout`
/// is cancelled then, and the handler is told then: a host that stopped reading cannot hold
/// the end of the connection back.
#[tokio::test(start_paused = true)]
async fn a_stretch_that_outlasts_the_grace_is_cancelled_when_it_runs_out() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(&link, HOLDING);
    overflow_behind_the_stretch(&link, &handler, &client).await;
    let began = tokio::time::Instant::now();

    eventually("the termination", handler.terminated.notified()).await;
    let observed = (began.elapsed(), handler.terminations(), handler.progress());
    assert!(
        matches!(
            &observed,
            (waited, told, (1, 1, 0))
                if *waited == GRACE
                    && matches!(&told[..], [(PeerTermination::NotificationBackpressure { .. }, 0)])
        ),
        "expected the handler told when the grace ran out, the stretch cancelled: (200ms, [(NotificationBackpressure, 0)], (1, 1, 0)) | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// The wait is for the hold, not for the call. Once the stretch is committed and the hold
/// dropped, the rest of the call is cancelled like any other, though it never returns.
#[tokio::test(start_paused = true)]
async fn the_wait_ends_when_the_hold_is_dropped_not_when_the_call_returns() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(
        &link,
        Script {
            park_after: true,
            ..HOLDING
        },
    );
    overflow_behind_the_stretch(&link, &handler, &client).await;
    tokio::time::sleep(GRACE / 4).await;

    let opened = tokio::time::Instant::now();
    handler.inside.add_permits(1);
    eventually("the termination", handler.terminated.notified()).await;
    let told = (opened.elapsed(), handler.terminations());
    assert!(
        matches!(
            &told,
            (Duration::ZERO, told)
                if matches!(&told[..], [(PeerTermination::NotificationBackpressure { .. }, 1)])
        ),
        "expected the handler told when the hold was dropped, with the call still parked after it: (0s, [(NotificationBackpressure, 1)]) | received {told:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// A close waits for a held stretch too, and is over when the stretch is.
#[tokio::test(start_paused = true)]
async fn a_close_waits_for_a_stretch_that_holds_its_cancellation_off() {
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

/// A close does not wait for a call that holds nothing, parked or not.
#[tokio::test(start_paused = true)]
async fn a_close_does_not_wait_for_a_call_that_holds_nothing() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(&link, NOT_HOLDING);
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

/// The wait for a hold overlaps the wait for the answers still owed, so a stretch that never
/// finishes costs a close one `shutdown_timeout`, inside the two it may take.
#[tokio::test(start_paused = true)]
async fn a_close_waits_one_shutdown_timeout_for_a_stretch_that_never_finishes() {
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

/// A hold asked for once the cancellation has been decided is never granted: the call ends
/// there, before the stretch it wanted to finish, and not somewhere inside it.
#[tokio::test]
async fn a_hold_asked_for_after_the_cancellation_was_decided_is_never_granted() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(
        &link,
        Script {
            park_before_hold: true,
            ..HOLDING
        },
    );
    link.push_line(UPDATE);
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
        HoldingHandler::count(&handler.asked_for_hold) == 1
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

/// A future that is ready when polled, or not: one poll, no waiting.
fn poll_once<T>(future: &mut std::pin::Pin<Box<dyn Future<Output = T> + Send + '_>>) -> Option<T> {
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    match future.as_mut().poll(&mut context) {
        std::task::Poll::Ready(value) => Some(value),
        std::task::Poll::Pending => None,
    }
}

/// The two halves of the handshake, each placed inside the other by a hook.
///
/// A cancellation raises its flag and then counts the holds; a call counts its hold and then
/// reads the flag. Whichever lands inside the other, the outcome is one of two: the
/// cancellation saw the hold and waits for it, or the hold was not granted. Never a hold
/// granted that the cancellation did not see.
#[tokio::test]
async fn a_hold_and_a_cancellation_that_cross_never_both_go_ahead() {
    // The hold lands between the cancellation's flag and its count.
    let link = ScriptedLink::new();
    let (_handler, client) = connect(&link, NOT_HOLDING);
    let granted = Arc::new(StdMutex::new(None));
    {
        *client
            .state
            .after_cancelling_raised
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Box::new({
            let client = Arc::clone(&client);
            let granted = Arc::clone(&granted);
            move || {
                // As the handler's task would ask, from inside a notification call.
                let mut asking: std::pin::Pin<Box<dyn Future<Output = bool> + Send + '_>> =
                    Box::pin(
                        HANDLING_NOTIFICATION_FOR
                            .scope(NotificationCall::of(client.state.address()), async {
                                client.hold_cancellation().await.is_holding()
                            }),
                    );
                *granted.lock().unwrap_or_else(PoisonError::into_inner) =
                    Some(poll_once(&mut asking));
            }
        }));
        let seen = client.state.cancellation_held();
        let granted = granted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .expect("expected the hook to have run");
        assert!(
            seen || granted.is_none(),
            "expected a hold asked for inside the cancellation seen by it or not granted: (true, _) or (_, None) | received ({seen}, {granted:?})"
        );
        assert_eq!(
            granted, None,
            "expected a hold asked for after the flag was raised not granted: None | received {granted:?}"
        );
    }
    client.close().await.expect("expected a clean close");

    // The cancellation lands between the hold's count and its read of the flag.
    let link = ScriptedLink::new();
    let (_handler, client) = connect(&link, NOT_HOLDING);
    let seen = Arc::new(StdMutex::new(None));
    {
        let state = Arc::clone(&client.state);
        let seen = Arc::clone(&seen);
        *client
            .state
            .after_hold_counted
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(move || {
            // As a close on another task would decide it, not one made from inside the call.
            let held = HANDLING_NOTIFICATION_FOR
                .sync_scope(NotificationCall::of(0), || state.cancellation_held());
            *seen.lock().unwrap_or_else(PoisonError::into_inner) = Some(held);
        }));
    }
    let mut asking: std::pin::Pin<Box<dyn Future<Output = bool> + Send + '_>> = Box::pin(
        HANDLING_NOTIFICATION_FOR.scope(NotificationCall::of(client.state.address()), async {
            client.hold_cancellation().await.is_holding()
        }),
    );
    let granted = poll_once(&mut asking);
    let seen = seen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
        .expect("expected the hook to have run");
    assert!(
        seen || granted.is_none(),
        "expected a cancellation decided inside the asking to see the hold, or the hold not granted: (true, _) or (_, None) | received ({seen}, {granted:?})"
    );
    assert!(
        seen,
        "expected the cancellation to see a hold counted before it looked: true | received false"
    );
    drop(asking);
    let left = client.state.holds.out.load(Ordering::SeqCst);
    assert_eq!(
        left, 0,
        "expected the hold taken back once the flag was read: 0 holds | received {left}"
    );
    client.close().await.expect("expected a clean close");
}

/// A close arrives while the reader is waiting out a hold after an overflow. Both wake when the
/// stretch is committed. The reader still has the handler to tell, and the close waits for it:
/// `on_terminated` runs to its end, once.
#[tokio::test(start_paused = true)]
async fn a_close_during_the_grace_of_an_overflow_lets_the_handler_be_told_whole() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(
        &link,
        Script {
            park_termination: true,
            ..HOLDING
        },
    );
    overflow_behind_the_stretch(&link, &handler, &client).await;
    let closing = spawn_close(&client);
    tokio::time::sleep(GRACE / 4).await;

    handler.inside.add_permits(1);
    eventually("the termination to begin", handler.terminated.notified()).await;
    tokio::time::sleep(GRACE / 4).await;
    let waiting = (
        closing.is_finished(),
        HoldingHandler::count(&handler.terminations_returned),
    );
    assert_eq!(
        waiting,
        (false, 0),
        "expected the close waiting while the handler is being told: (false, 0) | received {waiting:?}"
    );

    handler.termination_gate.add_permits(1);
    eventually("the close", closing)
        .await
        .expect("expected the close task to finish")
        .expect("expected a clean close");
    let told = (
        handler.terminations().len(),
        HoldingHandler::count(&handler.terminations_returned),
        handler.progress(),
    );
    assert_eq!(
        told,
        (1, 1, (1, 1, 1)),
        "expected the handler told once, to the end, after its stretch: (1, 1, (1, 1, 1)) | received {told:?}"
    );
}

/// The same for the ends that drain: a close that arrives while the reader is draining after
/// the peer's exit does not take the reader down before the handler has been told.
#[tokio::test(start_paused = true)]
async fn a_close_during_the_drain_after_an_exit_lets_the_handler_be_told_whole() {
    for script in [NOT_HOLDING, HOLDING] {
        let link = ScriptedLink::new();
        let (handler, client) = connect(
            &link,
            Script {
                park_termination: true,
                ..script
            },
        );
        inside_the_stretch(&link, &handler).await;
        link.end();
        until("the exit to end the connection", || client.is_closed()).await;

        let closing = spawn_close(&client);
        if script.hold {
            handler.inside.add_permits(1);
        }
        eventually("the termination to begin", handler.terminated.notified()).await;
        tokio::time::sleep(GRACE / 4).await;
        let waiting = (
            closing.is_finished(),
            HoldingHandler::count(&handler.terminations_returned),
        );
        assert_eq!(
            waiting,
            (false, 0),
            "expected the close waiting while the handler is being told (hold: {}): (false, 0) | received {waiting:?}",
            script.hold
        );

        handler.termination_gate.add_permits(1);
        eventually("the close", closing)
            .await
            .expect("expected the close task to finish")
            .expect("expected a clean close");
        let told = (
            handler.terminations(),
            HoldingHandler::count(&handler.terminations_returned),
        );
        assert!(
            matches!(&told, (told, 1) if matches!(&told[..], [(PeerTermination::Exited, _)])),
            "expected the handler told of the exit once, to the end (hold: {}): ([(Exited, _)], 1) | received {told:?}",
            script.hold
        );
    }
}

/// That wait is inside the bound of a close. A handler that never returns from `on_terminated`
/// holds the close until twice `shutdown_timeout` from its start, and no longer.
#[tokio::test(start_paused = true)]
async fn a_close_waits_for_the_handler_to_be_told_only_within_its_own_bound() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(
        &link,
        Script {
            park_termination: true,
            ..NOT_HOLDING
        },
    );
    link.end();
    eventually("the termination to begin", handler.terminated.notified()).await;

    let began = tokio::time::Instant::now();
    eventually("the close", client.close())
        .await
        .expect("expected a clean close");
    let observed = (
        began.elapsed(),
        HoldingHandler::count(&handler.terminations_returned),
    );
    assert_eq!(
        observed,
        (GRACE * 2, 0),
        "expected the close over at its bound with the handler still being told: (400ms, 0) | received {observed:?}"
    );
}

/// On the common path nothing is waited for: a close of a connection that has not ended takes
/// the reader down at once, as it always did.
#[tokio::test(start_paused = true)]
async fn a_close_of_a_live_connection_does_not_wait_for_the_reader() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(&link, NOT_HOLDING);
    let began = tokio::time::Instant::now();
    eventually("the close", client.close())
        .await
        .expect("expected a clean close");
    let observed = (began.elapsed(), handler.terminations().len());
    assert_eq!(
        observed,
        (Duration::ZERO, 0),
        "expected the close over at once and the handler not told, since its owner closed it: (0s, 0) | received {observed:?}"
    );
}

/// This side's outbound budget is passed by a caller, on the caller's thread, and the reader
/// ends the connection when it gets to it. A close that gets there first does not swallow the
/// end: the handler is still told why the connection ended.
#[tokio::test]
async fn a_close_that_overtakes_an_outbound_overflow_still_lets_the_handler_be_told() {
    let gated = gated(options(), limits(usize::MAX, usize::MAX, 2));
    let _held = link_held(&gated).await;
    let _queued = gated
        .client
        .submit_notification("queued", json!({}))
        .expect("expected the second frame queued");
    let refused = gated.client.submit_notification("one-too-many", json!({}));
    assert!(
        refused.is_err(),
        "expected the frame past the budget refused: Err | received Ok"
    );
    // Nothing has been awaited since the budget was passed, so the reader has not run: the
    // close below raises its flag before the reader hears of the overflow.
    let _ = within("the close", gated.client.close()).await;

    let termination = one_termination(&gated.handler).await;
    // The reader's own branch for an overflow takes the note of it; found still there, it was
    // the close's flag the reader answered.
    let through_the_close = gated
        .client
        .state
        .outbound_overflow
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_some();
    assert!(
        through_the_close
            && matches!(
                termination,
                PeerTermination::OutboundBackpressure {
                    limit: 2,
                    received: 3,
                    ..
                }
            ),
        "expected the handler told the end on record by the reader the close stopped: (true, OutboundBackpressure {{ limit: 2, received: 3 }}) | received ({through_the_close}, {termination:?})"
    );
}

/// A stretch that panics lets go of its hold as it unwinds: the end of the connection is not
/// held for the rest of the grace.
#[tokio::test(start_paused = true)]
async fn a_stretch_that_panics_releases_its_hold() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(
        &link,
        Script {
            panic_inside: true,
            ..HOLDING
        },
    );
    overflow_behind_the_stretch(&link, &handler, &client).await;
    tokio::time::sleep(GRACE / 4).await;

    let opened = tokio::time::Instant::now();
    handler.inside.add_permits(1);
    eventually("the termination", handler.terminated.notified()).await;
    let observed = (
        opened.elapsed(),
        handler.terminations().len(),
        handler.progress(),
    );
    assert_eq!(
        observed,
        (Duration::ZERO, 1, (1, 1, 0)),
        "expected the handler told the moment the stretch panicked: (0s, 1, (1, 1, 0)) | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// Inside a stretch the connection has already ended. What the stretch asks of its client is
/// refused at once, by type, so it can finish.
#[tokio::test(start_paused = true)]
async fn calls_made_inside_a_held_stretch_after_the_end_are_refused_at_once() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(
        &link,
        Script {
            ask_inside: true,
            ..HOLDING
        },
    );
    overflow_behind_the_stretch(&link, &handler, &client).await;

    let opened = tokio::time::Instant::now();
    handler.inside.add_permits(1);
    eventually("the termination", handler.terminated.notified()).await;
    let outcomes = handler
        .outcomes
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let observed = (opened.elapsed(), handler.progress(), outcomes);
    let closed = format!("{:?}", Err::<(), _>(Error::Closed { subject: "link" }));
    assert!(
        matches!(&observed, (Duration::ZERO, (1, 1, 1), outcomes) if outcomes.len() == 2 && outcomes.iter().all(|outcome| *outcome == closed)),
        "expected both calls refused as closed with no wait and the stretch committed: (0s, (1, 1, 1), [{closed}, {closed}]) | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// A close given up by its caller while it waits for a hold asks for the call to be cancelled:
/// nobody is waiting for the stretch any more. The link is left unusable, as after any close
/// that did not reach it.
#[tokio::test(start_paused = true)]
async fn a_close_given_up_while_it_waits_for_a_hold_cancels_the_call() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(&link, HOLDING);
    inside_the_stretch(&link, &handler).await;

    {
        let mut closing = Box::pin(client.close());
        let abandoned = tokio::time::timeout(GRACE / 2, &mut closing).await;
        assert!(
            abandoned.is_err(),
            "expected the close still waiting on the hold at half the grace: pending | received {abandoned:?}"
        );
    }
    let began = tokio::time::Instant::now();
    eventually(
        "the handler's task to stop",
        client.state.worker_stopped.cancelled(),
    )
    .await;
    let observed = (
        began.elapsed(),
        handler.progress(),
        client.state.link_poisoned.load(Ordering::Acquire),
    );
    assert_eq!(
        observed,
        (Duration::ZERO, (1, 1, 0), true),
        "expected the call cancelled as the close was dropped and the link left unusable: (0s, (1, 1, 0), true) | received {observed:?}"
    );
}

/// A close made from inside the stretch cannot wait for the stretch it is made from.
#[tokio::test(start_paused = true)]
async fn a_close_made_from_inside_a_held_stretch_does_not_wait_for_it() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(
        &link,
        Script {
            close_inside: true,
            ..HOLDING
        },
    );
    inside_the_stretch(&link, &handler).await;
    let began = tokio::time::Instant::now();
    handler.inside.add_permits(1);

    eventually(
        "the handler's task to stop",
        client.state.worker_stopped.cancelled(),
    )
    .await;
    let waited = began.elapsed();
    assert_eq!(
        waited,
        Duration::ZERO,
        "expected the task stopped at once, not after the grace: 0s | received {waited:?}"
    );
}

/// Dropping the client cannot wait. With the reader waiting out a hold, the drop cancels the
/// call whatever it holds, and the handler is never told the connection ended: nothing is left
/// running to tell it.
#[tokio::test(start_paused = true)]
async fn dropping_the_client_cancels_a_held_stretch_and_the_handler_is_not_told() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(&link, HOLDING);
    overflow_behind_the_stretch(&link, &handler, &client).await;
    let began = tokio::time::Instant::now();

    drop(Arc::into_inner(client).expect("expected the test to own the client"));
    until("every task holding the handler to be gone", || {
        Arc::strong_count(&handler) == 1
    })
    .await;
    let observed = (
        began.elapsed(),
        handler.progress(),
        handler.terminations().len(),
    );
    assert_eq!(
        observed,
        (Duration::ZERO, (1, 1, 0), 0),
        "expected the stretch cancelled by the drop and no termination told: (0s, (1, 1, 0), 0) | received {observed:?}"
    );
}

/// A hold asked for anywhere but inside a notification call on the client's own handler task
/// holds nothing, and says so.
#[tokio::test(start_paused = true)]
async fn a_hold_asked_for_outside_a_notification_call_holds_nothing() {
    let link = ScriptedLink::new();
    let (_handler, client) = connect(&link, NOT_HOLDING);
    let hold = client.hold_cancellation().await;
    let observed = (
        hold.is_holding(),
        client.state.holds.out.load(Ordering::SeqCst),
    );
    assert_eq!(
        observed,
        (false, 0),
        "expected a hold from outside a notification call to hold nothing: (false, 0) | received {observed:?}"
    );

    let began = tokio::time::Instant::now();
    eventually("the close", client.close())
        .await
        .expect("expected a clean close");
    let waited = began.elapsed();
    assert_eq!(
        waited,
        Duration::ZERO,
        "expected the close not to wait for a hold that holds nothing: 0s | received {waited:?}"
    );
    drop(hold);
}

/// A close on one thread while the handler's task, on another, runs one held stretch after
/// another. Wherever the close lands, every stretch whose hold was granted is committed.
///
/// Where the close lands is up to the scheduler, so that some close lands mid-stream is a
/// matter of chance, however likely the pacing makes it. The property does not rest on this
/// test: `a_hold_and_a_cancellation_that_cross_never_both_go_ahead` places each side inside
/// the other with hooks, which are the only two ways the two can cross. This one is here to
/// run the real tasks against each other on real threads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_close_racing_the_handlers_task_never_cuts_a_held_stretch() {
    let mut mid_stream = 0;
    // At least 200 rounds, and on until one has landed mid-stream, which the pacing below makes
    // all but certain within the first few; bounded, so that a machine on which it never does
    // fails the test with a message and does not hang it.
    for round in 0..2000 {
        if round >= 200 && mid_stream > 0 {
            break;
        }
        let link = ScriptedLink::new();
        let mut roomy = ClientOptions::new("ACP agent");
        // Machine time on a machine that may be busy: the grace is not what is being timed.
        roomy.shutdown_timeout = Duration::from_secs(30);
        let (handler, client) = connect_with(
            &link,
            Script {
                quick: true,
                ..HOLDING
            },
            roomy,
            WireOptions::new(),
        );
        for _ in 0..32 {
            link.push_line(UPDATE);
        }
        let reached = round % 32;
        until("the handler to have begun that many calls", || {
            handler.progress().0 >= reached
        })
        .await;
        within("the close", client.close())
            .await
            .expect("expected a clean close");
        let (_, granted, committed) = handler.progress();
        assert_eq!(
            granted, committed,
            "expected every stretch whose hold was granted to be committed (round {round}): granted == committed | received ({granted}, {committed})"
        );
        mid_stream += usize::from(committed > 0 && committed < 32);
    }
    assert!(
        mid_stream > 0,
        "expected some close in 2000 to land with some but not all of the 32 stretches committed: > 0 | received {mid_stream}"
    );
}

/// A close given up before it has stopped anything, here while it waits its turn at the map of
/// calls, still asks the handler's task to stop. Whatever the call holds, nobody is waiting for
/// it any more.
#[tokio::test(start_paused = true)]
async fn a_close_given_up_before_it_stopped_anything_still_cancels_the_call() {
    for script in [NOT_HOLDING, HOLDING] {
        let link = ScriptedLink::new();
        let (handler, client) = connect(&link, script);
        inside_the_stretch(&link, &handler).await;

        let in_the_way = client.state.pending.lock().await;
        let abandoned = tokio::time::timeout(GRACE / 2, client.close()).await;
        assert!(
            abandoned.is_err(),
            "expected the close held up behind the map of calls: pending | received {abandoned:?}"
        );
        let began = tokio::time::Instant::now();
        eventually(
            "the handler's task to stop",
            client.state.worker_stopped.cancelled(),
        )
        .await;
        let observed = (began.elapsed(), handler.progress().2);
        assert_eq!(
            observed,
            (Duration::ZERO, 0),
            "expected the call cancelled as the close was given up (hold: {}): (0s, 0) | received {observed:?}",
            script.hold
        );
        drop(in_the_way);
    }
}

/// The shutdown flag is up for the whole of the wait after an overflow, not only after it: a
/// stretch waiting on something the flag releases gets on, and the answers still owed to the
/// peer are written while the stretch is waited for.
#[tokio::test(start_paused = true)]
async fn an_overflow_raises_the_shutdown_flag_before_it_waits_for_a_hold() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(&link, HOLDING);
    overflow_behind_the_stretch(&link, &handler, &client).await;
    tokio::time::sleep(GRACE / 2).await;

    let observed = (
        client.state.shutdown.is_cancelled(),
        handler.progress(),
        handler.terminations().len(),
    );
    assert_eq!(
        observed,
        (true, (1, 1, 0), 0),
        "expected the shutdown flag up while the stretch is still waited for: (true, (1, 1, 0), 0) | received {observed:?}"
    );
    handler.inside.add_permits(1);
    eventually("the termination", handler.terminated.notified()).await;
    client.close().await.expect("expected a clean close");
}

/// The handler is told the connection ended only once its task has stopped, also when a close
/// took the task's handle before the reader came to drain it.
#[tokio::test(start_paused = true)]
async fn the_handler_is_told_after_its_task_stopped_when_a_close_holds_the_task() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(&link, NOT_HOLDING);
    inside_the_stretch(&link, &handler).await;
    // What a close does first to stop the task, stopped short of stopping it.
    let taken = client
        .state
        .notifications
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
        .expect("expected the handler's task to be on record");
    link.end();
    until("the exit to end the connection", || client.is_closed()).await;
    tokio::time::sleep(GRACE * 2).await;
    let early = handler.terminations().len();
    assert_eq!(
        early, 0,
        "expected no termination while the handler's task is still inside its call: 0 | received {early}"
    );

    taken.abort();
    eventually("the termination", handler.terminated.notified()).await;
    let told = (handler.terminations().len(), taken.is_finished());
    assert_eq!(
        told,
        (1, true),
        "expected the handler told once its task had stopped: (1, true) | received {told:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// A hold asked for inside a stretch that is already held is granted even after the
/// cancellation was decided: refusing it would park the stretch the cancellation waits for.
#[tokio::test(start_paused = true)]
async fn a_hold_asked_for_under_another_is_granted_after_the_cancellation_was_decided() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(
        &link,
        Script {
            nested_hold: true,
            ..HOLDING
        },
    );
    overflow_behind_the_stretch(&link, &handler, &client).await;

    let opened = tokio::time::Instant::now();
    handler.inside.add_permits(1);
    eventually("the termination", handler.terminated.notified()).await;
    let observed = (opened.elapsed(), handler.progress(), handler.terminations());
    assert!(
        matches!(
            &observed,
            (Duration::ZERO, (1, 1, 1), told)
                if matches!(&told[..], [(PeerTermination::NotificationBackpressure { .. }, 1)])
        ),
        "expected the stretch committed through its second hold, with no wait: (0s, (1, 1, 1), [(NotificationBackpressure, 1)]) | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// Two closes at once both wait for the stretch, and both return when it is committed.
#[tokio::test(start_paused = true)]
async fn two_closes_at_once_both_wait_for_a_held_stretch() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(&link, HOLDING);
    inside_the_stretch(&link, &handler).await;

    let began = tokio::time::Instant::now();
    let first = spawn_close(&client);
    let second = spawn_close(&client);
    tokio::time::sleep(GRACE / 2).await;
    let waiting = (
        first.is_finished(),
        second.is_finished(),
        handler.progress(),
    );
    assert_eq!(
        waiting,
        (false, false, (1, 1, 0)),
        "expected both closes waiting on the stretch: (false, false, (1, 1, 0)) | received {waiting:?}"
    );
    handler.inside.add_permits(1);
    for closing in [first, second] {
        eventually("the close", closing)
            .await
            .expect("expected the close task to finish")
            .expect("expected a clean close");
    }
    let observed = (began.elapsed(), handler.progress());
    assert_eq!(
        observed,
        (GRACE / 2, (1, 1, 1)),
        "expected both closes over when the stretch was committed: (100ms, (1, 1, 1)) | received {observed:?}"
    );
}

/// A close given up while it waits for the handler to be told takes the reader down, as a
/// close always did: the reader is not left running with nothing able to stop it.
#[tokio::test(start_paused = true)]
async fn a_close_given_up_while_the_handler_is_told_takes_the_reader_down() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(
        &link,
        Script {
            park_termination: true,
            ..NOT_HOLDING
        },
    );
    link.end();
    eventually("the termination to begin", handler.terminated.notified()).await;
    let with_the_reader = Arc::strong_count(&handler);

    let abandoned = tokio::time::timeout(GRACE, client.close()).await;
    assert!(
        abandoned.is_err(),
        "expected the close still waiting for the handler to be told at one grace: pending | received {abandoned:?}"
    );
    until("the reader to let go of the handler", || {
        Arc::strong_count(&handler) < with_the_reader
    })
    .await;
    let returned = HoldingHandler::count(&handler.terminations_returned);
    assert_eq!(
        returned, 0,
        "expected the reader cancelled inside on_terminated: 0 returned | received {returned}"
    );
}

/// A handler that closes its client from `on_terminated` is on the reader's own task. That
/// close neither waits for the call it is made from nor cuts it: `on_terminated` returns.
#[tokio::test(start_paused = true)]
async fn a_close_made_from_on_terminated_lets_that_call_return() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(
        &link,
        Script {
            close_in_termination: true,
            ..NOT_HOLDING
        },
    );
    let began = tokio::time::Instant::now();
    link.end();
    eventually(
        "on_terminated to return",
        handler.termination_returned.notified(),
    )
    .await;
    let observed = (
        began.elapsed(),
        handler.terminations().len(),
        HoldingHandler::count(&handler.terminations_returned),
    );
    assert_eq!(
        observed,
        (Duration::ZERO, 1, 1),
        "expected on_terminated to return from its own close, with no wait: (0s, 1, 1) | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// A host's close is waiting for the handler to be told, and the handler closes its client
/// from `on_terminated`. The inner close does not queue up behind the outer one: it returns,
/// the call returns, and the outer close is over then, not at its bound.
#[tokio::test(start_paused = true)]
async fn a_close_from_on_terminated_does_not_stall_a_close_waiting_for_it() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(
        &link,
        Script {
            park_termination: true,
            close_in_termination: true,
            ..NOT_HOLDING
        },
    );
    link.end();
    eventually("the termination to begin", handler.terminated.notified()).await;
    let began = tokio::time::Instant::now();
    let outer = spawn_close(&client);
    tokio::time::sleep(GRACE / 4).await;
    assert!(
        !outer.is_finished(),
        "expected the outer close waiting for the handler to be told: pending | received finished"
    );

    handler.termination_gate.add_permits(1);
    eventually("the outer close", outer)
        .await
        .expect("expected the close task to finish")
        .expect("expected a clean close");
    let observed = (
        began.elapsed(),
        HoldingHandler::count(&handler.terminations_returned),
    );
    assert_eq!(
        observed,
        (GRACE / 4, 1),
        "expected the outer close over when on_terminated returned from its own close: (50ms, 1) | received {observed:?}"
    );
}

/// A real close against a real inbound overflow, with nothing held: the reader is telling the
/// handler when the close arrives, and the handler is told once, to the end.
#[tokio::test(start_paused = true)]
async fn a_close_after_an_overflow_with_nothing_held_lets_the_handler_be_told_whole() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(
        &link,
        Script {
            park_termination: true,
            ..NOT_HOLDING
        },
    );
    inside_the_stretch(&link, &handler).await;
    for _ in 0..3 {
        link.push_line(UPDATE);
    }
    eventually("the termination to begin", handler.terminated.notified()).await;
    let closing = spawn_close(&client);
    tokio::time::sleep(GRACE / 4).await;
    let waiting = (
        closing.is_finished(),
        HoldingHandler::count(&handler.terminations_returned),
    );
    assert_eq!(
        waiting,
        (false, 0),
        "expected the close waiting while the handler is being told: (false, 0) | received {waiting:?}"
    );
    handler.termination_gate.add_permits(1);
    eventually("the close", closing)
        .await
        .expect("expected the close task to finish")
        .expect("expected a clean close");
    let told = (
        handler.terminations().len(),
        HoldingHandler::count(&handler.terminations_returned),
        handler.progress(),
    );
    assert_eq!(
        told,
        (1, 1, (1, 0, 0)),
        "expected the handler told once, to the end, its call cancelled: (1, 1, (1, 0, 0)) | received {told:?}"
    );
}

/// A hold belongs to its call. One kept past the call stops counting when the call returns:
/// no end of the connection waits for it, dropping it later on another thread takes nothing
/// off the count, and it keeps nothing of the client alive.
#[tokio::test(start_paused = true)]
async fn a_hold_kept_past_its_call_holds_nothing_once_the_call_is_over() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(
        &link,
        Script {
            keep_first_hold: true,
            ..HOLDING
        },
    );
    link.push_line(UPDATE);
    until("the first call to return with its hold kept", || {
        handler.progress() == (1, 1, 0)
            && handler
                .kept
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len()
                == 1
            && client.state.holds.out.load(Ordering::SeqCst) == 0
    })
    .await;
    let kept = handler
        .kept
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .pop()
        .expect("expected the kept hold");
    assert!(
        !kept.is_holding(),
        "expected a hold whose call is over to say it holds nothing: false | received true"
    );

    let began = tokio::time::Instant::now();
    eventually("the close", client.close())
        .await
        .expect("expected a clean close");
    let waited = began.elapsed();
    assert_eq!(
        waited,
        Duration::ZERO,
        "expected the close not to wait for a hold kept past its call: 0s | received {waited:?}"
    );

    let holds = Arc::clone(&client.state.holds);
    let state = Arc::downgrade(&client.state);
    drop(Arc::into_inner(client).expect("expected the test to own the client"));
    until(
        "the client's state to be freed with the hold still kept",
        || state.strong_count() == 0,
    )
    .await;
    std::thread::spawn(move || drop(kept))
        .join()
        .expect("expected the hold dropped on another thread");
    let left = holds.out.load(Ordering::SeqCst);
    assert_eq!(
        left, 0,
        "expected a hold dropped after its call to take nothing off the count: 0 | received {left}"
    );
}

/// A hold that is forgotten, never dropped, is released with its call all the same: the end of
/// the connection does not wait out the grace for it.
#[tokio::test(start_paused = true)]
async fn a_forgotten_hold_is_released_when_its_call_is_over() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(
        &link,
        Script {
            forget_hold: true,
            ..HOLDING
        },
    );
    overflow_behind_the_stretch(&link, &handler, &client).await;

    let opened = tokio::time::Instant::now();
    handler.inside.add_permits(1);
    eventually("the termination", handler.terminated.notified()).await;
    let observed = (
        opened.elapsed(),
        handler.progress(),
        client.state.holds.out.load(Ordering::SeqCst),
    );
    assert_eq!(
        observed,
        (Duration::ZERO, (1, 1, 1), 0),
        "expected the handler told the moment the call returned, nothing left held: (0s, (1, 1, 1), 0) | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// An answer that was waiting its turn behind a held stretch is queued work like any other. It
/// is not delivered once the end has been noticed: its caller is failed, once, with what ended
/// the connection, when the stretch is over.
#[tokio::test(start_paused = true)]
async fn an_answer_queued_behind_a_held_stretch_is_failed_not_delivered() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(&link, HOLDING);
    let (written, reply) = client
        .submit_request(
            "session/prompt",
            json!({}),
            RequestOptions::new()
                .without_deadline()
                .after_earlier_notifications(),
        )
        .expect("expected the request queued")
        .into_parts();
    eventually("the request written", written)
        .await
        .expect("expected the request written");
    inside_the_stretch(&link, &handler).await;
    // The answer takes its place behind the call; then the peer passes the budget.
    link.push_line(r#"{"jsonrpc":"2.0","id":"1","result":"done"}"#);
    for _ in 0..3 {
        link.push_line(UPDATE);
    }
    until("the overflow to decide the cancellation", || {
        client.is_closed() && client.state.cancelling_handler.load(Ordering::SeqCst)
    })
    .await;

    let mut reply = Box::pin(reply);
    tokio::time::sleep(GRACE / 2).await;
    let early =
        std::future::poll_fn(|cx| std::task::Poll::Ready(reply.as_mut().poll(cx).is_ready())).await;
    assert!(
        !early,
        "expected the reply still waiting while the stretch is in progress: pending | received an outcome"
    );
    handler.inside.add_permits(1);
    let outcome = eventually("the reply", reply).await;
    let failed = match outcome {
        Err(failed) => failed,
        Ok(delivered) => panic!(
            "expected the queued answer failed, not delivered: Err(Ended(Peer(NotificationBackpressure))) | received Ok({delivered:?})"
        ),
    };
    assert!(
        matches!(
            failed.cause(),
            crate::jsonrpc::CallFailureCause::Ended(crate::jsonrpc::ConnectionEnd::Peer(
                PeerTermination::NotificationBackpressure { .. }
            ))
        ),
        "expected the reply failed by the overflow: Ended(Peer(NotificationBackpressure)) | received {:?}",
        failed.cause()
    );
    let progress = handler.progress();
    assert_eq!(
        progress,
        (1, 1, 1),
        "expected the stretch committed and nothing else begun: (1, 1, 1) | received {progress:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// The ends that drain give the queue one `shutdown_timeout`. A stretch still held when that
/// runs out is not cut there: it gets the same bounded wait as on the other ends, and the
/// handler is told when it is committed.
#[tokio::test(start_paused = true)]
async fn the_drain_deadline_waits_for_a_stretch_that_is_held() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(&link, HOLDING);
    inside_the_stretch(&link, &handler).await;
    let began = tokio::time::Instant::now();
    link.end();
    tokio::time::sleep(GRACE + GRACE / 2).await;
    let waiting = (handler.progress(), handler.terminations().len());
    assert_eq!(
        waiting,
        ((1, 1, 0), 0),
        "expected the stretch still in progress past the drain's deadline: ((1, 1, 0), 0) | received {waiting:?}"
    );

    handler.inside.add_permits(1);
    eventually("the termination", handler.terminated.notified()).await;
    let told = (began.elapsed(), handler.terminations());
    assert!(
        matches!(&told, (waited, told) if *waited == GRACE + GRACE / 2 && matches!(&told[..], [(PeerTermination::Exited, 1)])),
        "expected the handler told when the stretch was committed: (300ms, [(Exited, 1)]) | received {told:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// That wait is bounded like the others: a held stretch that never finishes is cut two
/// `shutdown_timeout`s after the end, and a call that holds nothing after one, as it always was.
#[tokio::test(start_paused = true)]
async fn the_drain_deadline_is_one_grace_for_a_call_and_two_for_a_held_stretch() {
    for (script, expected) in [(NOT_HOLDING, GRACE), (HOLDING, GRACE * 2)] {
        let link = ScriptedLink::new();
        let (handler, client) = connect(&link, script);
        inside_the_stretch(&link, &handler).await;
        let began = tokio::time::Instant::now();
        link.end();

        eventually("the termination", handler.terminated.notified()).await;
        let observed = (began.elapsed(), handler.progress().2);
        assert_eq!(
            observed,
            (expected, 0),
            "expected the call cut and the handler told at the bound (hold: {}): ({expected:?}, 0) | received {observed:?}",
            script.hold
        );
        client.close().await.expect("expected a clean close");
    }
}

/// Two closes from the host while the handler is being told: both wait for it. The second does
/// not return early for finding the reader already taken by the first.
#[tokio::test(start_paused = true)]
async fn two_closes_at_once_both_wait_for_the_handler_to_be_told() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(
        &link,
        Script {
            park_termination: true,
            ..NOT_HOLDING
        },
    );
    link.end();
    eventually("the termination to begin", handler.terminated.notified()).await;
    let began = tokio::time::Instant::now();
    let first = spawn_close(&client);
    let second = spawn_close(&client);
    tokio::time::sleep(GRACE / 4).await;
    let waiting = (first.is_finished(), second.is_finished());
    assert_eq!(
        waiting,
        (false, false),
        "expected both closes waiting while the handler is being told: (false, false) | received {waiting:?}"
    );

    handler.termination_gate.add_permits(1);
    for closing in [first, second] {
        eventually("the close", closing)
            .await
            .expect("expected the close task to finish")
            .expect("expected a clean close");
    }
    let observed = (
        began.elapsed(),
        HoldingHandler::count(&handler.terminations_returned),
    );
    assert_eq!(
        observed,
        (GRACE / 4, 1),
        "expected both closes over when the handler had been told: (50ms, 1) | received {observed:?}"
    );
}

/// The drain's deadline overlaps its wait for a held stretch with the one for the answers
/// still owed to the peer: a stretch and a question that both never finish cost the end of
/// the connection two graces, not three.
#[tokio::test(start_paused = true)]
async fn the_drain_deadline_waits_for_a_held_stretch_and_the_answers_owed_at_once() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(&link, HOLDING);
    inside_the_stretch(&link, &handler).await;
    let began = tokio::time::Instant::now();
    link.end();
    tokio::time::sleep(GRACE + GRACE / 2).await;
    let observed = (
        client.state.shutdown.is_cancelled(),
        handler.terminations().len(),
    );
    assert_eq!(
        observed,
        (true, 0),
        "expected the shutdown flag up while the stretch is waited for past the drain: (true, 0) | received {observed:?}"
    );
    eventually("the termination", handler.terminated.notified()).await;
    let waited = began.elapsed();
    assert_eq!(
        waited,
        GRACE * 2,
        "expected the handler told two graces after the end: 400ms | received {waited:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// A caller that puts its own timeout around a close has to allow more than the close's bound.
/// At exactly twice `shutdown_timeout` the close's last deadline and the caller's timer fall on
/// the same instant. The close then still has the reader to take down and join, which needs
/// one more turn of the scheduler, and the caller's timer is found expired before that turn
/// comes: the close is given up, not finished.
#[tokio::test(start_paused = true)]
async fn a_callers_timeout_at_exactly_the_closes_bound_gives_the_close_up() {
    for (allowed, finishes) in [
        (GRACE * 2, false),
        (GRACE * 2 + Duration::from_millis(1), true),
    ] {
        let link = ScriptedLink::new();
        let (handler, client) = connect(
            &link,
            Script {
                park_termination: true,
                ..NOT_HOLDING
            },
        );
        link.end();
        eventually("the termination to begin", handler.terminated.notified()).await;

        let began = tokio::time::Instant::now();
        let outcome = tokio::time::timeout(allowed, client.close()).await;
        let observed = (outcome.is_ok(), began.elapsed());
        assert_eq!(
            observed,
            (finishes, GRACE * 2),
            "expected a close allowed {allowed:?} to finish: ({finishes}, 400ms) | received {observed:?}"
        );
    }
}

/// Two holds in one call are let go at different moments, in either order. The stretch is
/// over, and the end of the connection goes ahead, only when the last of them is dropped.
#[tokio::test(start_paused = true)]
async fn nested_holds_dropped_apart_hold_until_the_last_is_dropped() {
    for nested_dropped_first in [false, true] {
        let link = ScriptedLink::new();
        let (handler, client) = connect(
            &link,
            Script {
                nested_hold: true,
                nested_dropped_first,
                park_after: true,
                ..HOLDING
            },
        );
        overflow_behind_the_stretch(&link, &handler, &client).await;
        // The first of the two is dropped here, and the call waits with the other still out.
        handler.inside.add_permits(1);
        tokio::time::sleep(GRACE / 4).await;
        let waiting = (
            handler.progress(),
            client.state.holds.out.load(Ordering::SeqCst),
            handler.terminations().len(),
        );
        assert_eq!(
            waiting,
            ((1, 1, 1), 1, 0),
            "expected one hold still out and the end still waiting (nested first: {nested_dropped_first}): ((1, 1, 1), 1, 0) | received {waiting:?}"
        );

        let opened = tokio::time::Instant::now();
        handler.after.add_permits(1);
        eventually("the termination", handler.terminated.notified()).await;
        let observed = (
            opened.elapsed(),
            client.state.holds.out.load(Ordering::SeqCst),
        );
        assert_eq!(
            observed,
            (Duration::ZERO, 0),
            "expected the handler told when the last hold was dropped (nested first: {nested_dropped_first}): (0s, 0) | received {observed:?}"
        );
        client.close().await.expect("expected a clean close");
    }
}

/// A hold handed to another thread and dropped there while its call is still running is
/// released there and then: the stretch is no longer held, and the end goes ahead.
#[tokio::test(start_paused = true)]
async fn a_hold_dropped_on_another_thread_during_its_call_releases_the_stretch() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(
        &link,
        Script {
            hold_elsewhere: true,
            ..HOLDING
        },
    );
    overflow_behind_the_stretch(&link, &handler, &client).await;
    tokio::time::sleep(GRACE / 4).await;
    let waiting = handler.terminations().len();
    assert_eq!(
        waiting, 0,
        "expected the end waiting for the hold kept on another task: 0 | received {waiting}"
    );

    let elsewhere = handler
        .kept
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .pop()
        .expect("expected the hold handed over");
    let opened = tokio::time::Instant::now();
    std::thread::spawn(move || drop(elsewhere))
        .join()
        .expect("expected the hold dropped on another thread");
    eventually("the termination", handler.terminated.notified()).await;
    let observed = (
        opened.elapsed(),
        handler.progress(),
        client.state.holds.out.load(Ordering::SeqCst),
    );
    assert_eq!(
        observed,
        (Duration::ZERO, (1, 1, 0), 0),
        "expected the end to go ahead as the hold was dropped elsewhere, the call cut: (0s, (1, 1, 0), 0) | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// A hold handed to another task and kept there past the end of its call is released when the
/// call returns, and dropping it afterwards takes nothing off the count.
#[tokio::test(start_paused = true)]
async fn a_hold_kept_on_another_task_is_released_when_its_call_returns() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(
        &link,
        Script {
            hold_elsewhere: true,
            ..HOLDING
        },
    );
    overflow_behind_the_stretch(&link, &handler, &client).await;

    let opened = tokio::time::Instant::now();
    handler.inside.add_permits(1);
    eventually("the termination", handler.terminated.notified()).await;
    let observed = (
        opened.elapsed(),
        handler.progress(),
        client.state.holds.out.load(Ordering::SeqCst),
    );
    assert_eq!(
        observed,
        (Duration::ZERO, (1, 1, 1), 0),
        "expected the handler told when the call returned, its hold still kept elsewhere: (0s, (1, 1, 1), 0) | received {observed:?}"
    );
    handler
        .kept
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
    let left = client.state.holds.out.load(Ordering::SeqCst);
    assert_eq!(
        left, 0,
        "expected the hold dropped after its call to take nothing off the count: 0 | received {left}"
    );
    client.close().await.expect("expected a clean close");
}

/// The drain's deadline has passed while one stretch was held. A hold the same call asks for
/// after that, for a second stretch, is refused: the call is cut there, before the stretch.
#[tokio::test(start_paused = true)]
async fn a_hold_asked_for_after_the_drain_deadline_is_refused() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(
        &link,
        Script {
            second_stretch: true,
            ..HOLDING
        },
    );
    inside_the_stretch(&link, &handler).await;
    let began = tokio::time::Instant::now();
    link.end();
    tokio::time::sleep(GRACE + GRACE / 2).await;

    handler.inside.add_permits(1);
    eventually("the termination", handler.terminated.notified()).await;
    let observed = (
        began.elapsed(),
        HoldingHandler::count(&handler.asked_for_hold),
        handler.progress(),
    );
    assert_eq!(
        observed,
        (GRACE + GRACE / 2, 2, (1, 1, 1)),
        "expected the second hold asked for and never granted, the handler told at once: (300ms, 2, (1, 1, 1)) | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// With nothing held, there is a moment between an end deciding to stop the handler's task and
/// the task being stopped. An ordered answer the task comes to in that moment is not
/// delivered: its caller is failed, once, with what ended the connection.
#[tokio::test(start_paused = true)]
async fn an_answer_reached_after_the_end_was_decided_is_failed_not_delivered() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(&link, NOT_HOLDING);
    let (written, reply) = client
        .submit_request(
            "session/prompt",
            json!({}),
            RequestOptions::new()
                .without_deadline()
                .after_earlier_notifications(),
        )
        .expect("expected the request queued")
        .into_parts();
    eventually("the request written", written)
        .await
        .expect("expected the request written");
    inside_the_stretch(&link, &handler).await;
    link.push_line(r#"{"jsonrpc":"2.0","id":"1","result":"done"}"#);
    until("the answer to take its place behind the call", || {
        client
            .state
            .pending
            .try_lock()
            .is_ok_and(|pending| pending.is_empty())
    })
    .await;

    // What a close does up to the moment before it stops the task, and no further.
    let _ = client.state.ended_with(
        crate::jsonrpc::ConnectionEnd::Closed,
        client.state.closed_error(),
    );
    client.state.mark_closed();
    let held = client.state.cancellation_held();
    assert!(
        !held,
        "expected nothing held when the end was decided: false | received true"
    );
    // The task gets on before it is stopped: its call returns and it comes to the answer.
    handler.inside.add_permits(1);
    let outcome = eventually("the reply", reply).await;
    let failed = match outcome {
        Err(failed) => failed,
        Ok(delivered) => panic!(
            "expected the queued answer failed, not delivered: Err(Ended(Closed)) | received Ok({delivered:?})"
        ),
    };
    assert!(
        matches!(
            failed.cause(),
            crate::jsonrpc::CallFailureCause::Ended(crate::jsonrpc::ConnectionEnd::Closed)
        ),
        "expected the reply failed by the close: Ended(Closed) | received {:?}",
        failed.cause()
    );
    client.close().await.expect("expected a clean close");
}

/// With nothing held, there is a moment between an end deciding to stop the handler's task and
/// the task being stopped. A call that returns in that moment is the last: what is queued
/// behind it, a notification or a question from the peer, is not begun.
#[tokio::test(start_paused = true)]
async fn work_reached_after_the_end_was_decided_is_not_begun() {
    const QUESTION: &str =
        r#"{"jsonrpc":"2.0","id":7,"method":"session/request_permission","params":{}}"#;
    for queued in [UPDATE, QUESTION] {
        let link = ScriptedLink::new();
        let (handler, client) = connect(&link, NOT_HOLDING);
        inside_the_stretch(&link, &handler).await;
        link.push_line(queued);
        until("the next frame to be queued behind the call", || {
            client.state.peer_bytes.available_permits()
                <= client.state.options.max_pending_bytes - UPDATE.len() - queued.len()
        })
        .await;

        // What a close does up to the moment before it stops the task, and no further.
        let _ = client.state.ended_with(
            crate::jsonrpc::ConnectionEnd::Closed,
            client.state.closed_error(),
        );
        client.state.mark_closed();
        let held = client.state.cancellation_held();
        assert!(
            !held,
            "expected nothing held when the end was decided: false | received true"
        );
        // The task gets on before it is stopped: its call returns, with more queued behind.
        handler.inside.add_permits(8);
        // A task that took up what was queued is left waiting for more, so its stopping is
        // given a bound and is part of what is asserted.
        let stopped = tokio::time::timeout(GRACE, client.state.worker_stopped.cancelled())
            .await
            .is_ok();
        let observed = (
            stopped,
            handler.progress(),
            HoldingHandler::count(&handler.asked),
        );
        assert_eq!(
            observed,
            (true, (1, 0, 1), 0),
            "expected the task stopped after the call in progress, nothing queued begun: (true, (1, 0, 1), 0) | received {observed:?}"
        );
        client.close().await.expect("expected a clean close");
    }
}

/// After an inbound overflow with nothing held, the handler is told the connection ended only
/// once its task has stopped, also when a close took the task's handle first. Asking a task
/// to stop does not stop it there and then: it is gone the next time the scheduler comes to
/// it, and the reader waits for that.
#[tokio::test(start_paused = true)]
async fn an_overflow_tells_the_handler_after_its_task_stopped_when_a_close_holds_the_task() {
    let link = ScriptedLink::new();
    let (handler, client) = connect(&link, NOT_HOLDING);
    inside_the_stretch(&link, &handler).await;
    // A close that has the task's handle and is waiting for the task to stop, as the reader
    // finds it: the handle is gone from the record, and the task has only been asked to stop.
    let taken = client
        .state
        .notifications
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
        .expect("expected the handler's task to be on record");
    for _ in 0..3 {
        link.push_line(UPDATE);
    }
    eventually("the termination", handler.terminated.notified()).await;
    let ended = HoldingHandler::count(&handler.ended_at_termination);
    assert_eq!(
        ended, 1,
        "expected the call in progress gone by the time the handler was told: 1 call ended | received {ended}"
    );
    taken.abort();
    client.close().await.expect("expected a clean close");
}

/// A call's end and a drop of its hold on another thread, at once, over and over. Whichever
/// gets there first, the hold is counted off once: the count is back at zero after every call
/// and never below it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_calls_end_racing_a_hold_dropped_on_another_thread_counts_it_off_once() {
    let link = ScriptedLink::new();
    let mut roomy = ClientOptions::new("ACP agent");
    roomy.shutdown_timeout = Duration::from_secs(30);
    let (handler, client) = connect_with(
        &link,
        Script {
            hold_elsewhere: true,
            quick: true,
            ..HOLDING
        },
        roomy,
        WireOptions::new(),
    );
    for round in 1..=500 {
        link.push_line(UPDATE);
        // The call hands its hold over and runs on to its end, three turns of the scheduler
        // away. This thread drops the hold at once, or a varying moment later, or after the
        // call has ended, so that each of the two gets there first in some rounds.
        let looking = std::time::Instant::now();
        let elsewhere = loop {
            let found = handler
                .kept
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop();
            if let Some(hold) = found {
                break hold;
            }
            assert!(
                looking.elapsed() < Duration::from_secs(5),
                "expected the call to hand its hold over within 5s (round {round}) | received nothing"
            );
            std::hint::spin_loop();
        };
        match round % 3 {
            0 => {}
            1 => {
                for _ in 0..(round * 37 % 2000) {
                    std::hint::spin_loop();
                }
            }
            _ => {
                until("the call to end before its hold is dropped", || {
                    HoldingHandler::count(&handler.ended) == round
                })
                .await;
            }
        }
        drop(elsewhere);
        until("the call to end", || {
            HoldingHandler::count(&handler.ended) == round
        })
        .await;
        // The call's record is let go a step after its future: the count settles at zero.
        until("the hold to be counted off", || {
            client.state.holds.out.load(Ordering::SeqCst) == 0
        })
        .await;
    }
    let holds = Arc::clone(&client.state.holds);
    within("the close", client.close())
        .await
        .expect("expected a clean close");
    // Every call is over and its record gone: a hold counted off twice anywhere shows here.
    let left = holds.out.load(Ordering::SeqCst);
    assert_eq!(
        left, 0,
        "expected every hold counted off exactly once over 500 calls: 0 | received {left}"
    );
}
