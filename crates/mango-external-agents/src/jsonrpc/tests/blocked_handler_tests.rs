//! A handler that blocks its thread cannot be cut by an abort, and a close must still end within
//! its bound, with a timeout that says which wait ran out.
//!
//! These run on a multi-thread runtime and the real clock: a handler that blocks its thread on a
//! `std` primitive stops a current-thread runtime altogether, and a paused clock never advances
//! past it. The primitive is released by the test, and by a guard on every path out of it, so a
//! failing run leaves no thread blocked and no runtime stuck at shutdown.

use super::abandoned_close_tests::ParkedTermination;
use super::outbox_tests::{options, until, within};
use super::*;
use crate::jsonrpc::{
    ABORT_SETTLE, ConnectionEnd, READER_SHUTDOWN, Responder, WireOptions,
    is_handler_shutdown_timeout, is_link_shutdown_timeout, is_reader_shutdown_timeout,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex as StdMutex, PoisonError};
use std::time::Instant;

/// How long a blocked handler waits for its release when nothing releases it: past any wait a test
/// makes, so a test that fails to release it stalls a thread and not the suite.
const BACKSTOP: Duration = Duration::from_secs(20);

/// Time the machine may take over what the bound allows, so that a loaded runner does not fail a
/// close that is on time. The bound itself is what the failure message names.
const LOAD_MARGIN: Duration = Duration::from_secs(2);

/// A latch a thread blocks on with the standard library, not the runtime's.
struct ThreadGate {
    open: StdMutex<bool>,
    changed: Condvar,
}

impl ThreadGate {
    fn arc() -> Arc<Self> {
        Arc::new(Self {
            open: StdMutex::new(false),
            changed: Condvar::new(),
        })
    }

    /// Blocks the calling thread until [`ThreadGate::release`], or for `BACKSTOP`.
    fn block(&self) {
        let open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = self
            .changed
            .wait_timeout_while(open, BACKSTOP, |open| !*open)
            .unwrap_or_else(PoisonError::into_inner);
    }

    fn release(&self) {
        *self.open.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.changed.notify_all();
    }
}

/// Releases the gate when it goes, on whichever path the test leaves by.
struct ReleaseOnDrop(Arc<ThreadGate>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// A handler whose `on_terminated` blocks its thread, with no await, until the test releases it.
struct ThreadBlockedTermination {
    entered: AtomicUsize,
    returned: AtomicUsize,
    gate: Arc<ThreadGate>,
}

impl ThreadBlockedTermination {
    fn arc(gate: &Arc<ThreadGate>) -> Arc<Self> {
        Arc::new(Self {
            entered: AtomicUsize::new(0),
            returned: AtomicUsize::new(0),
            gate: Arc::clone(gate),
        })
    }
}

#[async_trait::async_trait]
impl PeerHandler for ThreadBlockedTermination {
    async fn on_notification(&self, _method: String, _params: Value) {}

    async fn on_terminated(&self, _termination: PeerTermination) {
        self.entered.fetch_add(1, Ordering::AcqRel);
        self.gate.block();
        self.returned.fetch_add(1, Ordering::AcqRel);
    }
}

/// The documented bound of a close: twice `shutdown_timeout`, and the allowance the reader gets to
/// observe the abort when that time is spent.
fn bound() -> Duration {
    options().shutdown_timeout * 2 + ABORT_SETTLE
}

/// The bound of a close behind a handler that blocks its thread on a live connection: the same,
/// and one more allowance, for the handler's own task.
fn bound_with_both_allowances() -> Duration {
    options().shutdown_timeout * 2 + ABORT_SETTLE * 2
}

/// A close on a connection the peer ended, with a handler blocked in `on_terminated`, returns
/// within its bound with the timeout that names the reader, and the client is then in a state
/// that survives the handler returning late: the end is still the peer's exit, a later close and a
/// drop are quiet, and the handler is told once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_close_returns_within_its_bound_when_the_handler_blocks_its_thread() {
    let gate = ThreadGate::arc();
    let _release = ReleaseOnDrop(Arc::clone(&gate));
    let handler = ThreadBlockedTermination::arc(&gate);
    let link = ScriptedLink::new();
    let client = Client::connect(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options(),
    );
    link.end();
    until("the handler to block inside on_terminated", || {
        handler.entered.load(Ordering::Acquire) == 1
    })
    .await;

    let bound = bound();
    let began = Instant::now();
    let closed = tokio::time::timeout(bound + LOAD_MARGIN, client.close()).await;
    let waited = began.elapsed();
    let Ok(closed) = closed else {
        panic!(
            "expected close to return within {bound:?} | received: still waiting after {waited:?}"
        );
    };
    let timed_out_on_the_reader = matches!(&closed, Err(error) if is_reader_shutdown_timeout(error))
        && matches!(&closed, Err(Error::Timeout { operation, .. }) if operation == READER_SHUTDOWN);
    assert!(
        timed_out_on_the_reader,
        "expected close to give up with a Timeout for {READER_SHUTDOWN:?} | received {closed:?} after {waited:?}"
    );
    assert!(
        waited >= options().shutdown_timeout * 2,
        "expected close to wait out its whole bound for the handler before giving up | received: gave up after {waited:?} of {:?}",
        options().shutdown_timeout * 2
    );

    // The reader is still alive on its blocked thread. The client says what it said before.
    let while_blocked = (
        client.is_closed(),
        client.ended(),
        handler.entered.load(Ordering::Acquire),
        handler.returned.load(Ordering::Acquire),
    );
    let expected = (
        true,
        Some(ConnectionEnd::Peer(PeerTermination::Exited)),
        1,
        0,
    );
    assert_eq!(
        while_blocked, expected,
        "expected the client closed, the peer's exit on record and the handler blocked once: {expected:?} | received {while_blocked:?}"
    );

    // A close made while the handler is still blocked has no reader left to wait for.
    let again = within(
        "a second close with the handler still blocked",
        client.close(),
    )
    .await;
    assert!(
        again.is_ok(),
        "expected a later close to find nothing to wait for: Ok(()) | received {again:?}"
    );

    // The handler returns late: nothing is reported twice and nothing panics.
    gate.release();
    until("the blocked handler to return", || {
        handler.returned.load(Ordering::Acquire) == 1
    })
    .await;
    let after = (
        client.ended(),
        handler.entered.load(Ordering::Acquire),
        handler.returned.load(Ordering::Acquire),
    );
    let expected = (Some(ConnectionEnd::Peer(PeerTermination::Exited)), 1, 1);
    assert_eq!(
        after, expected,
        "expected the end unchanged and the handler told once after it returned late: {expected:?} | received {after:?}"
    );
    let last = within("a close after the handler returned", client.close()).await;
    assert!(
        last.is_ok(),
        "expected a close after the late return to be quiet: Ok(()) | received {last:?}"
    );
    drop(client);
}

/// A sender whose `close` fails, as a link does that could not be closed.
struct FailingClose(Box<dyn crate::link::LinkSender>);

#[async_trait::async_trait]
impl crate::link::LinkSender for FailingClose {
    async fn send(&mut self, message: String) -> crate::Result<()> {
        self.0.send(message).await
    }

    async fn close(&mut self) -> crate::Result<()> {
        Err(Error::Link {
            peer: String::from("ACP agent"),
            message: String::from("close refused"),
        })
    }
}

/// A close that could not close the link and then could not stop the reader behind a blocked
/// handler returns the link's own error, as the better answer, and not the reader's timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_close_that_failed_to_close_the_link_returns_that_failure_behind_a_blocked_handler() {
    let gate = ThreadGate::arc();
    let _release = ReleaseOnDrop(Arc::clone(&gate));
    let handler = ThreadBlockedTermination::arc(&gate);
    let link = ScriptedLink::new();
    let (sender, receiver) = link.clone().into_link().split();
    let client = Client::connect(
        crate::link::Link::new(Box::new(FailingClose(sender)), receiver),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options(),
    );
    link.end();
    until("the handler to block inside on_terminated", || {
        handler.entered.load(Ordering::Acquire) == 1
    })
    .await;

    let bound = bound();
    let began = Instant::now();
    let closed = tokio::time::timeout(bound + LOAD_MARGIN, client.close()).await;
    let waited = began.elapsed();
    let Ok(closed) = closed else {
        panic!(
            "expected close to return within {bound:?} | received: still waiting after {waited:?}"
        );
    };
    assert!(
        matches!(&closed, Err(Error::Link { message, .. }) if message == "close refused"),
        "expected the link's own failure, Error::Link \"close refused\" | received {closed:?} after {waited:?}"
    );
}

/// The options every client here runs under, with no time at all for a close to wait out.
fn spent() -> ClientOptions {
    let mut spent = options();
    spent.shutdown_timeout = Duration::ZERO;
    spent
}

/// With a `shutdown_timeout` of zero the close's bound is spent before it reaches the reader.
/// The first close still takes the reader and aborts it: a handler parked at an await is cut,
/// and the close completes with `Ok`.
#[tokio::test(start_paused = true)]
async fn a_close_with_no_time_left_still_takes_the_reader_down() {
    let handler = ParkedTermination::arc();
    let link = ScriptedLink::new();
    let client = Client::connect(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        spent(),
    );
    link.end();
    until("the handler to be told of the exit", || {
        handler.entered.load(Ordering::Acquire) == 1
    })
    .await;

    let closed = within("the close", client.close()).await;
    let observed = (
        closed.is_ok(),
        handler.cut.load(Ordering::Acquire),
        handler.returned.load(Ordering::Acquire),
    );
    assert_eq!(
        observed,
        (true, 1, 0),
        "expected a close with no time left to abort the handler's call and complete: (true, 1, 0) | received {observed:?} (closed, cut, returned) with {closed:?}"
    );
}

/// The same close behind a handler that blocks its thread gives up with the reader's timeout
/// once the allowance has passed, and does not wait for the handler.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_close_with_no_time_left_gives_up_behind_a_blocked_handler() {
    let gate = ThreadGate::arc();
    let _release = ReleaseOnDrop(Arc::clone(&gate));
    let handler = ThreadBlockedTermination::arc(&gate);
    let link = ScriptedLink::new();
    let client = Client::connect(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        spent(),
    );
    link.end();
    until("the handler to block inside on_terminated", || {
        handler.entered.load(Ordering::Acquire) == 1
    })
    .await;

    let bound = ABORT_SETTLE;
    let began = Instant::now();
    let closed = tokio::time::timeout(bound + LOAD_MARGIN, client.close()).await;
    let waited = began.elapsed();
    let Ok(closed) = closed else {
        panic!(
            "expected close to return within {bound:?} | received: still waiting after {waited:?}"
        );
    };
    assert!(
        matches!(&closed, Err(error) if is_reader_shutdown_timeout(error)),
        "expected the reader's timeout | received {closed:?} after {waited:?}"
    );
}

/// Which call of a handler blocks its thread.
#[derive(Clone, Copy, Debug)]
enum BlockedCall {
    Notification,
    RequestHandoff,
    Request,
    /// A notification and a question at once, so that a close has a task that cannot be stopped
    /// and an answer that cannot be written.
    NotificationAndRequest,
}

/// A handler that blocks its thread, with no await, in the one call it is told to, and answers
/// every other call as the default handler does.
struct ThreadBlockedCall {
    blocked: BlockedCall,
    entered: AtomicUsize,
    returned: AtomicUsize,
    terminated: AtomicUsize,
    gate: Arc<ThreadGate>,
}

impl ThreadBlockedCall {
    fn arc(blocked: BlockedCall, gate: &Arc<ThreadGate>) -> Arc<Self> {
        Arc::new(Self {
            blocked,
            entered: AtomicUsize::new(0),
            returned: AtomicUsize::new(0),
            terminated: AtomicUsize::new(0),
            gate: Arc::clone(gate),
        })
    }

    fn block_if(&self, call: BlockedCall) {
        let blocks = matches!(
            (self.blocked, call),
            (BlockedCall::Notification, BlockedCall::Notification)
                | (BlockedCall::RequestHandoff, BlockedCall::RequestHandoff)
                | (BlockedCall::Request, BlockedCall::Request)
                | (
                    BlockedCall::NotificationAndRequest,
                    BlockedCall::Notification | BlockedCall::Request
                )
        );
        if !blocks {
            return;
        }
        self.entered.fetch_add(1, Ordering::AcqRel);
        self.gate.block();
        self.returned.fetch_add(1, Ordering::AcqRel);
    }
}

#[async_trait::async_trait]
impl PeerHandler for ThreadBlockedCall {
    async fn on_notification(&self, _method: String, _params: Value) {
        self.block_if(BlockedCall::Notification);
    }

    async fn on_request(
        &self,
        _method: String,
        _params: Value,
        _id: RequestId,
    ) -> ServerRequestOutcome {
        self.block_if(BlockedCall::Request);
        ServerRequestOutcome::Answer(json!({}))
    }

    async fn on_request_handoff(&self, method: String, params: Value, responder: Responder) {
        self.block_if(BlockedCall::RequestHandoff);
        let outcome = self
            .on_request(method, params, responder.id().clone())
            .await;
        let _ = responder.answer_or_refuse(outcome);
    }

    async fn on_terminated(&self, _termination: PeerTermination) {
        self.terminated.fetch_add(1, Ordering::AcqRel);
    }
}

/// What a close that was made behind a handler blocked in `blocked` did, and how long it took.
struct ClosedBehind {
    link: ScriptedLink,
    client: Client,
    handler: Arc<ThreadBlockedCall>,
    gate: Arc<ThreadGate>,
    closed: crate::Result<()>,
    waited: Duration,
    _release: ReleaseOnDrop,
}

/// Connects a client whose handler blocks its thread in `blocked`, feeds it the frame that makes
/// the peer call it, waits for the handler to block, and closes the client while it is blocked.
/// The connection has not ended, unless `peer_exits`, in which case the peer exits first. Panics
/// with the bound it missed when the close does not return.
async fn close_behind_a_handler_blocked_in(blocked: BlockedCall, peer_exits: bool) -> ClosedBehind {
    let gate = ThreadGate::arc();
    let release = ReleaseOnDrop(Arc::clone(&gate));
    let handler = ThreadBlockedCall::arc(blocked, &gate);
    let link = ScriptedLink::new();
    let client = Client::connect_with(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options(),
        match blocked {
            // Every question goes to the handler's task on a client made with the option.
            BlockedCall::RequestHandoff => WireOptions::new().with_request_handoff(),
            BlockedCall::Notification | BlockedCall::Request => WireOptions::new(),
            BlockedCall::NotificationAndRequest => {
                unreachable!("a test of its own drives both")
            }
        },
    );
    let frame = match blocked {
        BlockedCall::Notification => r#"{"jsonrpc":"2.0","method":"note","params":{}}"#,
        BlockedCall::RequestHandoff | BlockedCall::Request => {
            r#"{"jsonrpc":"2.0","id":7,"method":"ask","params":{}}"#
        }
        BlockedCall::NotificationAndRequest => unreachable!("a test of its own drives both"),
    };
    link.push_line(frame);
    until("the handler to block inside the call", || {
        handler.entered.load(Ordering::Acquire) == 1
    })
    .await;
    if peer_exits {
        link.end();
        // The reader takes the handler's task to drain it once it has seen the exit.
        until("the reader to take the handler's task", || {
            client
                .state
                .notifications
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_none()
        })
        .await;
    }

    let bound = bound_with_both_allowances();
    let began = Instant::now();
    let closed = tokio::time::timeout(bound + LOAD_MARGIN, client.close()).await;
    let waited = began.elapsed();
    let Ok(closed) = closed else {
        panic!(
            "expected close to return within {bound:?} | received: still waiting after {waited:?}"
        );
    };
    ClosedBehind {
        link,
        client,
        handler,
        gate,
        closed,
        waited,
        _release: release,
    }
}

impl ClosedBehind {
    /// What the client says while the handler's call is still blocked: closed, the close on
    /// record as its end, the call entered once and not returned, and nobody told of an end.
    fn assert_closed_with_the_call_still_blocked(&self, ended: Option<ConnectionEnd>) {
        let observed = (
            self.client.is_closed(),
            self.client.ended(),
            self.handler.entered.load(Ordering::Acquire),
            self.handler.returned.load(Ordering::Acquire),
            self.handler.terminated.load(Ordering::Acquire),
        );
        let expected = (true, ended, 1, 0, 0);
        assert_eq!(
            observed, expected,
            "expected the client closed, its end on record, the call blocked once and no end told: {expected:?} | received {observed:?}"
        );
    }

    /// Lets the blocked call return, and checks that nothing was reported or broken by that: the
    /// end is as it was, nobody is told of one, and a later close and a drop are quiet.
    async fn assert_quiet_when_the_call_returns_late(self, ended: Option<ConnectionEnd>) {
        let again = within("a close with the call still blocked", self.client.close()).await;
        assert!(
            again.is_ok(),
            "expected a later close to find nothing to wait for: Ok(()) | received {again:?}"
        );
        self.gate.release();
        until("the blocked call to return", || {
            self.handler.returned.load(Ordering::Acquire) == 1
        })
        .await;
        let after = (
            self.client.ended(),
            self.handler.entered.load(Ordering::Acquire),
            self.handler.terminated.load(Ordering::Acquire),
        );
        let expected = (ended, 1, 0);
        assert_eq!(
            after, expected,
            "expected the end unchanged and no end told after the call returned late: {expected:?} | received {after:?}"
        );
        let last = within("a close after the call returned", self.client.close()).await;
        assert!(
            last.is_ok(),
            "expected a close after the late return to be quiet: Ok(()) | received {last:?}"
        );
        drop(self.client);
        drop(self.link);
    }
}

/// A handler that blocks its thread in `on_notification` on a connection that has not ended does
/// not hold the close for as long as it blocks: the close waits the first grace for the call, as
/// it would for a slow one, closes the link, and returns the handler's timeout within its bound.
/// The task stays on its blocked thread; when the call returns late nothing is reported.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_close_returns_within_its_bound_when_on_notification_blocks_its_thread() {
    let behind = close_behind_a_handler_blocked_in(BlockedCall::Notification, false).await;
    let closed = &behind.closed;
    assert!(
        matches!(closed, Err(error) if is_handler_shutdown_timeout(error) && !is_reader_shutdown_timeout(error) && !is_link_shutdown_timeout(error)),
        "expected the handler's timeout | received {closed:?} after {:?}",
        behind.waited
    );
    assert!(
        behind.waited >= options().shutdown_timeout,
        "expected close to give the call one shutdown_timeout ({:?}) first | received: gave up after {:?}",
        options().shutdown_timeout,
        behind.waited
    );
    behind.assert_closed_with_the_call_still_blocked(Some(ConnectionEnd::Closed));
    behind
        .assert_quiet_when_the_call_returns_late(Some(ConnectionEnd::Closed))
        .await;
}

/// The same for `on_request_handoff`, which the handler's task awaits as it does a notification.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_close_returns_within_its_bound_when_on_request_handoff_blocks_its_thread() {
    let behind = close_behind_a_handler_blocked_in(BlockedCall::RequestHandoff, false).await;
    let closed = &behind.closed;
    assert!(
        matches!(closed, Err(error) if is_handler_shutdown_timeout(error)),
        "expected the handler's timeout | received {closed:?} after {:?}",
        behind.waited
    );
    behind.assert_closed_with_the_call_still_blocked(Some(ConnectionEnd::Closed));
    behind
        .assert_quiet_when_the_call_returns_late(Some(ConnectionEnd::Closed))
        .await;
}

/// `on_request` runs on a task of its own that a close lets write its refusal for one grace and
/// then aborts without waiting for it: the close ends within its bound, with `Ok`, whatever the
/// call does. The task stays on its blocked thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_close_returns_within_its_bound_when_on_request_blocks_its_thread() {
    let behind = close_behind_a_handler_blocked_in(BlockedCall::Request, false).await;
    let closed = &behind.closed;
    assert!(
        closed.is_ok(),
        "expected the close to end with Ok(()) having aborted the call without waiting | received {closed:?} after {:?}",
        behind.waited
    );
    behind.assert_closed_with_the_call_still_blocked(Some(ConnectionEnd::Closed));
    behind
        .assert_quiet_when_the_call_returns_late(Some(ConnectionEnd::Closed))
        .await;
}

/// On a connection the peer ended, the reader is the one waiting for the handler's task, parked at
/// an await, so the close takes the reader down within its bound and the call stays blocked: the
/// handler is told of no end, and the close ends with `Ok`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_close_returns_within_its_bound_when_on_notification_blocks_after_the_peer_exited() {
    let behind = close_behind_a_handler_blocked_in(BlockedCall::Notification, true).await;
    let closed = &behind.closed;
    assert!(
        closed.is_ok(),
        "expected the close to end with Ok(()) having taken the parked reader down | received {closed:?} after {:?}",
        behind.waited
    );
    let ended = Some(ConnectionEnd::Peer(PeerTermination::Exited));
    behind.assert_closed_with_the_call_still_blocked(ended.clone());
    behind.assert_quiet_when_the_call_returns_late(ended).await;
}

/// A handler task that cannot be stopped and an answer that cannot be written take the same
/// first grace, not one each: the close waits for the two together, and the link's grace is then
/// all it has left to spend. One after the other they would take twice `shutdown_timeout` before
/// the link was reached.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blocked_task_and_a_blocked_answer_share_the_first_grace() {
    let gate = ThreadGate::arc();
    let _release = ReleaseOnDrop(Arc::clone(&gate));
    let handler = ThreadBlockedCall::arc(BlockedCall::NotificationAndRequest, &gate);
    let link = ScriptedLink::new();
    let mut slow = options();
    slow.shutdown_timeout = Duration::from_millis(400);
    let grace = slow.shutdown_timeout;
    let client = Client::connect(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        slow,
    );
    // The question first, and the notification once its answer is blocked: the handler's task
    // starts the answers, so a question queued behind a notification that blocks it never runs.
    link.push_line(r#"{"jsonrpc":"2.0","id":7,"method":"ask","params":{}}"#);
    until("the answer to block", || {
        handler.entered.load(Ordering::Acquire) == 1
    })
    .await;
    link.push_line(r#"{"jsonrpc":"2.0","method":"note","params":{}}"#);
    until("both calls to block", || {
        handler.entered.load(Ordering::Acquire) == 2
    })
    .await;

    let began = Instant::now();
    let closed = tokio::time::timeout(grace * 2 + LOAD_MARGIN, client.close()).await;
    let waited = began.elapsed();
    let Ok(closed) = closed else {
        panic!(
            "expected close to return within {:?} | received: still waiting after {waited:?}",
            grace * 2
        );
    };
    assert!(
        matches!(&closed, Err(error) if is_handler_shutdown_timeout(error)),
        "expected the handler's timeout | received {closed:?} after {waited:?}"
    );
    assert!(
        waited >= grace && waited < grace * 2,
        "expected the task and the answer to share one grace, {grace:?} to {:?} | received {waited:?}",
        grace * 2
    );
}
