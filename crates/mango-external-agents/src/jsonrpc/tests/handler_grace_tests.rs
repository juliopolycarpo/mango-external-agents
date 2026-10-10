//! What becomes of a notification call in progress when the connection is cut short: by the
//! peer passing a budget, or by a close.

use super::outbox_tests::{eventually, until, within};
use super::*;
use std::sync::atomic::AtomicUsize;
use std::sync::{Mutex as StdMutex, PoisonError};

/// A handler whose `on_notification` records that it began, waits at a gate, and records that
/// it returned: what a handler holding something across an await looks like from outside.
struct ParkingHandler {
    finishes: bool,
    begun: AtomicUsize,
    returned: AtomicUsize,
    /// Questions from the peer put to this handler.
    asked: AtomicUsize,
    gate: tokio::sync::Semaphore,
    /// How many calls had returned when the handler was told the connection ended.
    returned_at_termination: StdMutex<Vec<(PeerTermination, usize)>>,
    terminated: Notify,
}

impl ParkingHandler {
    fn arc(finishes: bool) -> Arc<Self> {
        Arc::new(Self {
            finishes,
            begun: AtomicUsize::new(0),
            returned: AtomicUsize::new(0),
            asked: AtomicUsize::new(0),
            gate: tokio::sync::Semaphore::new(0),
            returned_at_termination: StdMutex::new(Vec::new()),
            terminated: Notify::new(),
        })
    }

    fn calls(&self) -> (usize, usize) {
        (
            self.begun.load(Ordering::Acquire),
            self.returned.load(Ordering::Acquire),
        )
    }

    fn terminations(&self) -> Vec<(PeerTermination, usize)> {
        self.returned_at_termination
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait::async_trait]
impl PeerHandler for ParkingHandler {
    async fn on_notification(&self, _method: String, _params: Value) {
        self.begun.fetch_add(1, Ordering::AcqRel);
        self.gate
            .acquire()
            .await
            .expect("expected the gate to stay open for the whole test")
            .forget();
        self.returned.fetch_add(1, Ordering::AcqRel);
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
        let returned = self.returned.load(Ordering::Acquire);
        self.returned_at_termination
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((termination, returned));
        self.terminated.notify_one();
    }

    fn finishes_notification_in_progress(&self) -> bool {
        self.finishes
    }
}

const GRACE: Duration = Duration::from_millis(200);
const UPDATE: &str = r#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#;

/// A client that holds one notification for a handler that is busy with another.
fn narrow_client(link: &ScriptedLink, handler: &Arc<ParkingHandler>) -> Client {
    Client::connect(
        link.clone().into_link(),
        Arc::clone(handler) as Arc<dyn PeerHandler>,
        ClientOptions::new("ACP agent").with_max_pending_notifications(1),
    )
}

/// Parks the handler inside its first call and has the peer pass the notification budget
/// behind it.
async fn overflow_behind_a_parked_call(link: &ScriptedLink, handler: &ParkingHandler) {
    link.push_line(UPDATE);
    until("the handler to be inside its first call", || {
        handler.calls().0 == 1
    })
    .await;
    for _ in 0..3 {
        link.push_line(UPDATE);
    }
}

/// The peer passes the notification budget while the handler is inside a call. A handler that
/// asked to finish its calls is let return from that one before it is told the connection
/// ended, and nothing that was still queued is handed to it.
#[tokio::test(start_paused = true)]
async fn an_overflow_lets_the_call_in_progress_finish_for_a_handler_that_asked() {
    let link = ScriptedLink::new();
    let handler = ParkingHandler::arc(true);
    let client = narrow_client(&link, &handler);
    overflow_behind_a_parked_call(&link, &handler).await;
    until("the overflow to end the connection", || client.is_closed()).await;

    // Half the grace later the call is still in progress, and the handler has not been told.
    tokio::time::sleep(GRACE / 2).await;
    let waiting = (handler.calls(), handler.terminations().len());
    assert_eq!(
        waiting,
        ((1, 0), 0),
        "expected the call still in progress and no termination yet: ((1, 0), 0) | received {waiting:?}"
    );

    let opened = tokio::time::Instant::now();
    handler.gate.add_permits(1);
    eventually("the termination", handler.terminated.notified()).await;
    let told = (opened.elapsed(), handler.terminations());
    assert!(
        matches!(
            &told,
            (Duration::ZERO, told)
                if matches!(&told[..], [(PeerTermination::NotificationBackpressure { .. }, 1)])
        ),
        "expected one termination, the moment the call returned: (0s, [(NotificationBackpressure, 1)]) | received {told:?}"
    );
    let calls = handler.calls();
    assert_eq!(
        calls,
        (1, 1),
        "expected the one call finished and nothing queued behind it started: (1, 1) | received {calls:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// The same overflow for a handler that did not ask: the call is cancelled where it stands, as
/// it always was, and the handler is told at once.
#[tokio::test(start_paused = true)]
async fn an_overflow_cancels_the_call_in_progress_for_a_handler_that_did_not_ask() {
    let link = ScriptedLink::new();
    let handler = ParkingHandler::arc(false);
    let client = narrow_client(&link, &handler);
    let began = tokio::time::Instant::now();
    overflow_behind_a_parked_call(&link, &handler).await;

    eventually("the termination", handler.terminated.notified()).await;
    let observed = (began.elapsed(), handler.terminations(), handler.calls());
    assert!(
        matches!(
            &observed,
            (Duration::ZERO, told, (1, 0))
                if matches!(&told[..], [(PeerTermination::NotificationBackpressure { .. }, 0)])
        ),
        "expected the handler told at once with its call cancelled: (0s, [(NotificationBackpressure, 0)], (1, 0)) | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// The grace is a bound. A call that does not return within `shutdown_timeout` is cancelled
/// then, and the handler is told then: a host that stopped reading cannot hold the end back.
#[tokio::test(start_paused = true)]
async fn a_call_that_outlasts_the_grace_is_cancelled_when_it_runs_out() {
    let link = ScriptedLink::new();
    let handler = ParkingHandler::arc(true);
    let client = narrow_client(&link, &handler);
    overflow_behind_a_parked_call(&link, &handler).await;
    until("the overflow to end the connection", || client.is_closed()).await;
    let began = tokio::time::Instant::now();

    eventually("the termination", handler.terminated.notified()).await;
    let observed = (began.elapsed(), handler.terminations(), handler.calls());
    assert!(
        matches!(
            &observed,
            (waited, told, (1, 0))
                if *waited == GRACE
                    && matches!(&told[..], [(PeerTermination::NotificationBackpressure { .. }, 0)])
        ),
        "expected the handler told when the grace ran out, its call cancelled: (200ms, [(NotificationBackpressure, 0)], (1, 0)) | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// A close gives the call in progress the same grace, and returns once the call has.
#[tokio::test(start_paused = true)]
async fn a_close_lets_the_call_in_progress_finish_for_a_handler_that_asked() {
    let link = ScriptedLink::new();
    let handler = ParkingHandler::arc(true);
    let client = Arc::new(narrow_client(&link, &handler));
    link.push_line(UPDATE);
    until("the handler to be inside its first call", || {
        handler.calls().0 == 1
    })
    .await;
    // One more waits behind it, within the budget.
    link.push_line(UPDATE);

    let closing = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.close().await })
    };
    tokio::time::sleep(GRACE / 2).await;
    let waiting = (closing.is_finished(), handler.calls());
    assert_eq!(
        waiting,
        (false, (1, 0)),
        "expected the close waiting on the call in progress: (false, (1, 0)) | received {waiting:?}"
    );
    handler.gate.add_permits(1);
    eventually("the close", closing)
        .await
        .expect("expected the close task to finish")
        .expect("expected a clean close");
    let calls = handler.calls();
    assert_eq!(
        calls,
        (1, 1),
        "expected the one call finished and the queued one never started: (1, 1) | received {calls:?}"
    );
}

/// A close does not take longer for it than a close may: the wait for the call overlaps the
/// wait for the answers still owed, so a call that never returns costs one `shutdown_timeout`.
#[tokio::test(start_paused = true)]
async fn a_close_waits_one_shutdown_timeout_for_a_call_that_never_returns() {
    let link = ScriptedLink::new();
    let handler = ParkingHandler::arc(true);
    let client = narrow_client(&link, &handler);
    link.push_line(UPDATE);
    until("the handler to be inside its first call", || {
        handler.calls().0 == 1
    })
    .await;

    let began = tokio::time::Instant::now();
    client.close().await.expect("expected a clean close");
    let observed = (began.elapsed(), handler.calls());
    assert_eq!(
        observed,
        (GRACE, (1, 0)),
        "expected the close over when the grace ran out, the call cancelled: (200ms, (1, 0)) | received {observed:?}"
    );
}

/// With no call in progress there is nothing to wait for, asked or not.
#[tokio::test(start_paused = true)]
async fn a_close_with_no_call_in_progress_does_not_wait() {
    for finishes in [true, false] {
        let link = ScriptedLink::new();
        let handler = ParkingHandler::arc(finishes);
        let client = narrow_client(&link, &handler);
        let began = tokio::time::Instant::now();
        client.close().await.expect("expected a clean close");
        let waited = began.elapsed();
        assert_eq!(
            waited,
            Duration::ZERO,
            "expected an idle handler (finishes: {finishes}) closed at once: 0s | received {waited:?}"
        );
    }
}

/// A handler whose calls take a few turns of the scheduler, counting those begun and returned.
struct CountingHandler {
    begun: AtomicUsize,
    returned: AtomicUsize,
}

#[async_trait::async_trait]
impl PeerHandler for CountingHandler {
    async fn on_notification(&self, _method: String, _params: Value) {
        self.begun.fetch_add(1, Ordering::AcqRel);
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        self.returned.fetch_add(1, Ordering::AcqRel);
    }

    async fn on_request(
        &self,
        _method: String,
        _params: Value,
        _id: RequestId,
    ) -> ServerRequestOutcome {
        ServerRequestOutcome::Answer(Value::Null)
    }

    fn finishes_notification_in_progress(&self) -> bool {
        true
    }
}

/// A close on one thread while the handler's task, on another, moves from one call to the
/// next. Wherever the close lands, no call that began is cut off: the task is stopped between
/// calls or let finish the one it is in.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_close_racing_the_handlers_task_never_cuts_a_call_that_began() {
    let mut cut_short = 0;
    for round in 0..200 {
        let link = ScriptedLink::new();
        let handler = Arc::new(CountingHandler {
            begun: AtomicUsize::new(0),
            returned: AtomicUsize::new(0),
        });
        let mut roomy = ClientOptions::new("ACP agent");
        // Machine time on a machine that may be busy: the grace is not what is being timed.
        roomy.shutdown_timeout = Duration::from_secs(30);
        let client = Client::connect(
            link.clone().into_link(),
            Arc::clone(&handler) as Arc<dyn PeerHandler>,
            roomy,
        );
        for _ in 0..32 {
            link.push_line(UPDATE);
        }
        // A different point in the stream each round.
        for _ in 0..round % 16 {
            tokio::task::yield_now().await;
        }
        within("the close", client.close())
            .await
            .expect("expected a clean close");
        let calls = (
            handler.begun.load(Ordering::Acquire),
            handler.returned.load(Ordering::Acquire),
        );
        assert_eq!(
            calls.0, calls.1,
            "expected every call that began to have returned by the end of the close (round {round}): begun == returned | received {calls:?}"
        );
        cut_short += usize::from(calls.1 < 32);
    }
    assert!(
        cut_short > 0,
        "expected some close in 200 to land before the 32 notifications were all handled: > 0 | received {cut_short}"
    );
}

/// The close is over when the call is, not when the grace is: with nothing queued behind the
/// call, the handler's task has nothing else to wake it.
#[tokio::test(start_paused = true)]
async fn a_close_returns_as_soon_as_the_call_in_progress_does() {
    let link = ScriptedLink::new();
    let handler = ParkingHandler::arc(true);
    let client = Arc::new(narrow_client(&link, &handler));
    link.push_line(UPDATE);
    until("the handler to be inside its first call", || {
        handler.calls().0 == 1
    })
    .await;

    let began = tokio::time::Instant::now();
    let closing = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.close().await })
    };
    tokio::time::sleep(GRACE / 2).await;
    handler.gate.add_permits(1);
    eventually("the close", closing)
        .await
        .expect("expected the close task to finish")
        .expect("expected a clean close");
    let observed = (began.elapsed(), handler.calls());
    assert_eq!(
        observed,
        (GRACE / 2, (1, 1)),
        "expected the close over when the call returned: (100ms, (1, 1)) | received {observed:?}"
    );
}

/// An answer that was waiting its turn behind the call in progress is queued work like any
/// other: it is not delivered once the stop has been asked for, and its caller is failed with
/// what ended the connection.
#[tokio::test(start_paused = true)]
async fn an_answer_queued_behind_the_call_in_progress_is_failed_not_delivered() {
    let link = ScriptedLink::new();
    let handler = ParkingHandler::arc(true);
    let client = narrow_client(&link, &handler);
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
    link.push_line(UPDATE);
    until("the handler to be inside its first call", || {
        handler.calls().0 == 1
    })
    .await;
    // The answer takes its place behind the call; then the peer passes the budget.
    link.push_line(r#"{"jsonrpc":"2.0","id":"1","result":"done"}"#);
    for _ in 0..3 {
        link.push_line(UPDATE);
    }
    until("the overflow to end the connection", || client.is_closed()).await;

    // Its answer was read and is in the queue, so it is failed with the queue, when the call
    // in progress returns, and not before.
    let mut reply = Box::pin(reply);
    tokio::time::sleep(GRACE / 2).await;
    let early =
        std::future::poll_fn(|cx| std::task::Poll::Ready(reply.as_mut().poll(cx).is_ready())).await;
    assert!(
        !early,
        "expected the reply still waiting while the call is in progress: pending | received an outcome"
    );
    handler.gate.add_permits(1);
    let outcome = eventually("the reply", reply).await;
    let failed = outcome.expect_err("expected the queued answer not delivered after the stop");
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
    let calls = handler.calls();
    assert_eq!(
        calls,
        (1, 1),
        "expected the call in progress finished and nothing else started: (1, 1) | received {calls:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// The narrowest place for a stop to land: the handler's task has taken a notification and has
/// not yet said it is in a call. The stop finds no call and goes to cancel the task; the task,
/// which says it is in a call before it looks for a stop, sees the stop and does not begin.
/// Begun all the same, the call would be cancelled at its first await.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_that_lands_as_a_notification_is_taken_keeps_its_call_from_beginning() {
    let link = ScriptedLink::new();
    let handler = Arc::new(CountingHandler {
        begun: AtomicUsize::new(0),
        returned: AtomicUsize::new(0),
    });
    let mut roomy = ClientOptions::new("ACP agent");
    roomy.shutdown_timeout = Duration::from_secs(30);
    let client = Client::connect(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        roomy,
    );
    let taken = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let found_idle = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let spin_until = |reached: Box<dyn Fn() -> bool + Send>, bound: Duration| {
        let began = std::time::Instant::now();
        while !reached() && began.elapsed() < bound {
            std::thread::yield_now();
        }
    };
    {
        // The task stands still, with the notification in hand, until the stop has looked.
        let taken = Arc::clone(&taken);
        let found_idle = Arc::clone(&found_idle);
        *client
            .state
            .before_call_claimed
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(move || {
            taken.store(true, Ordering::Release);
            spin_until(
                Box::new(move || found_idle.load(Ordering::Acquire)),
                Duration::from_secs(5),
            );
        }));
    }
    {
        // The stop, having found no call, stands still until the task has shown what it does
        // with the notification it holds: stop, or begin the call.
        let handler = Arc::clone(&handler);
        let found_idle = Arc::clone(&found_idle);
        let stopped = client.state.worker_stopped.clone();
        *client
            .state
            .after_no_call_found
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(move || {
            found_idle.store(true, Ordering::Release);
            spin_until(
                Box::new(move || {
                    stopped.is_cancelled() || handler.begun.load(Ordering::Acquire) > 0
                }),
                Duration::from_secs(5),
            );
        }));
    }
    link.push_line(UPDATE);
    until("the handler's task to take the notification", || {
        taken.load(Ordering::Acquire)
    })
    .await;

    within("the close", client.close())
        .await
        .expect("expected a clean close");
    let observed = (
        found_idle.load(Ordering::Acquire),
        handler.begun.load(Ordering::Acquire),
        handler.returned.load(Ordering::Acquire),
    );
    assert_eq!(
        observed,
        (true, 0, 0),
        "expected the stop to find no call and the call never to begin: (true, 0, 0) | received {observed:?}"
    );
}

/// The peer passes the byte budget with one frame, while the only other one is still in the
/// handler: nothing is queued behind the call. The handler is told the moment the call returns,
/// not when the grace runs out, though nothing else arrives to wake its task.
#[tokio::test(start_paused = true)]
async fn an_overflow_with_nothing_queued_behind_the_call_ends_when_the_call_returns() {
    let link = ScriptedLink::new();
    let handler = ParkingHandler::arc(true);
    let client = Client::connect(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        ClientOptions {
            max_pending_bytes: UPDATE.len(),
            ..ClientOptions::new("ACP agent")
        },
    );
    link.push_line(UPDATE);
    until("the handler to be inside its first call", || {
        handler.calls().0 == 1
    })
    .await;
    // The frame in the handler still holds its bytes, so this one does not fit.
    link.push_line(UPDATE);
    until("the overflow to end the connection", || client.is_closed()).await;

    tokio::time::sleep(GRACE / 4).await;
    let opened = tokio::time::Instant::now();
    handler.gate.add_permits(1);
    eventually("the termination", handler.terminated.notified()).await;
    let told = (opened.elapsed(), handler.terminations(), handler.calls());
    assert!(
        matches!(
            &told,
            (Duration::ZERO, told, (1, 1))
                if matches!(&told[..], [(PeerTermination::NotificationByteBackpressure { .. }, 1)])
        ),
        "expected the handler told the moment its call returned: (0s, [(NotificationByteBackpressure, 1)], (1, 1)) | received {told:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// A close and the reader can both be stopping the handler's task at once. Each waits for the
/// call in progress: neither goes on because the other got to the task first.
#[tokio::test(start_paused = true)]
async fn two_stops_at_once_both_wait_for_the_call_in_progress() {
    let link = ScriptedLink::new();
    let handler = ParkingHandler::arc(true);
    let client = narrow_client(&link, &handler);
    link.push_line(UPDATE);
    until("the handler to be inside its first call", || {
        handler.calls().0 == 1
    })
    .await;

    let found = client.state.notification_call_to_wait_for();
    assert!(
        found,
        "expected the stop to find the call in progress: true | received false"
    );
    let mut first = Box::pin(client.state.wait_for_notification_call());
    let mut second = Box::pin(client.state.wait_for_notification_call());
    for (name, stop) in [("first", &mut first), ("second", &mut second)] {
        let finished =
            std::future::poll_fn(|cx| std::task::Poll::Ready(stop.as_mut().poll(cx).is_ready()))
                .await;
        assert!(
            !finished,
            "expected the {name} stop waiting on the call in progress: pending | received finished"
        );
    }
    handler.gate.add_permits(1);
    for (name, stop) in [("first", first), ("second", second)] {
        eventually("a stop", stop).await;
        let calls = handler.calls();
        assert_eq!(
            calls,
            (1, 1),
            "expected the {name} stop over only once the call had returned: (1, 1) | received {calls:?}"
        );
    }
    client.close().await.expect("expected a clean close");
}

/// The shutdown flag goes up before the wait, so a call that is itself waiting for the
/// connection to end can return, and the handler is told then.
#[tokio::test(start_paused = true)]
async fn an_overflow_raises_the_shutdown_flag_before_it_waits_for_the_call() {
    let link = ScriptedLink::new();
    let handler = ParkingHandler::arc(true);
    let client = narrow_client(&link, &handler);
    overflow_behind_a_parked_call(&link, &handler).await;
    until("the overflow to end the connection", || client.is_closed()).await;

    let raised = client.state.shutdown.is_cancelled();
    let waiting = (handler.calls(), handler.terminations().len());
    assert!(
        raised && waiting == ((1, 0), 0),
        "expected the shutdown flag up while the call is still waited for: (true, ((1, 0), 0)) | received ({raised}, {waiting:?})"
    );
    handler.gate.add_permits(1);
    eventually("the termination", handler.terminated.notified()).await;
    client.close().await.expect("expected a clean close");
}

/// Dropping the client cannot wait. With the reader waiting out the grace for a call in
/// progress, the drop cancels the call and everything else at once: nothing is left holding
/// the handler.
#[tokio::test(start_paused = true)]
async fn dropping_the_client_during_the_grace_cancels_the_call_at_once() {
    let link = ScriptedLink::new();
    let handler = ParkingHandler::arc(true);
    let client = narrow_client(&link, &handler);
    overflow_behind_a_parked_call(&link, &handler).await;
    until("the overflow to end the connection", || client.is_closed()).await;
    let began = tokio::time::Instant::now();

    drop(client);
    until("every task holding the handler to be gone", || {
        Arc::strong_count(&handler) == 1
    })
    .await;
    let observed = (began.elapsed(), handler.calls());
    assert_eq!(
        observed,
        (Duration::ZERO, (1, 0)),
        "expected the call cancelled by the drop with no wait: (0s, (1, 0)) | received {observed:?}"
    );
}

/// A close given up by its caller during the grace does not leave the handler's task running a
/// call nobody waits for any more: the task is stopped as the close is dropped.
#[tokio::test(start_paused = true)]
async fn a_close_dropped_during_the_grace_stops_the_handlers_task() {
    let link = ScriptedLink::new();
    let handler = ParkingHandler::arc(true);
    let client = narrow_client(&link, &handler);
    link.push_line(UPDATE);
    until("the handler to be inside its first call", || {
        handler.calls().0 == 1
    })
    .await;

    {
        let mut closing = Box::pin(client.close());
        let abandoned = tokio::time::timeout(GRACE / 2, &mut closing).await;
        assert!(
            abandoned.is_err(),
            "expected the close still waiting on the call at half the grace: pending | received {abandoned:?}"
        );
    }
    let began = tokio::time::Instant::now();
    eventually(
        "the handler's task to stop",
        client.state.worker_stopped.cancelled(),
    )
    .await;
    let observed = (began.elapsed(), handler.calls());
    assert_eq!(
        observed,
        (Duration::ZERO, (1, 0)),
        "expected the task stopped as the close was dropped, its call cancelled: (0s, (1, 0)) | received {observed:?}"
    );
}

/// A handler that closes its own client from inside a notification call, and asked for its
/// calls to finish. The close cannot wait for the call it is being made from.
struct SelfClosingHandler {
    client: std::sync::OnceLock<std::sync::Weak<Client>>,
}

#[async_trait::async_trait]
impl PeerHandler for SelfClosingHandler {
    async fn on_notification(&self, _method: String, _params: Value) {
        let client = self.client.get().and_then(std::sync::Weak::upgrade);
        if let Some(client) = client {
            let _ = client.close().await;
        }
    }

    async fn on_request(
        &self,
        _method: String,
        _params: Value,
        _id: RequestId,
    ) -> ServerRequestOutcome {
        ServerRequestOutcome::Answer(Value::Null)
    }

    fn finishes_notification_in_progress(&self) -> bool {
        true
    }
}

#[tokio::test(start_paused = true)]
async fn a_close_made_from_inside_the_call_does_not_wait_for_that_call() {
    let link = ScriptedLink::new();
    let handler = Arc::new(SelfClosingHandler {
        client: std::sync::OnceLock::new(),
    });
    let client = Arc::new(Client::connect(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        ClientOptions::new("ACP agent"),
    ));
    let _ = handler.client.set(Arc::downgrade(&client));
    let began = tokio::time::Instant::now();
    link.push_line(UPDATE);

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

/// Once the stop has been asked for, the handler's task takes up nothing more, whatever kind of
/// work comes next: a question from the peer read just then is not put to the handler.
#[tokio::test]
async fn a_question_read_after_the_stop_was_asked_for_is_not_put_to_the_handler() {
    let link = ScriptedLink::new();
    let handler = ParkingHandler::arc(true);
    let client = narrow_client(&link, &handler);
    // What a close and an overflow do first, before they wait for anything.
    client.state.ask_handler_task_to_stop();
    link.push_line(r#"{"jsonrpc":"2.0","id":7,"method":"session/request_permission"}"#);

    within(
        "the handler's task to stop at the question",
        client.state.worker_stopped.cancelled(),
    )
    .await;
    let asked = handler.asked.load(Ordering::Acquire);
    assert_eq!(
        asked, 0,
        "expected the question not put to the handler after the stop: 0 | received {asked}"
    );
    client.close().await.expect("expected a clean close");
}
