//! A handler that blocks its thread cannot be cut by an abort, and a close must still end within
//! its bound, with a timeout that says which wait ran out.
//!
//! These run on a multi-thread runtime and the real clock: a handler that blocks its thread on a
//! `std` primitive stops a current-thread runtime altogether, and a paused clock never advances
//! past it. The primitive is released by the test, and by a guard on every path out of it, so a
//! failing run leaves no thread blocked and no runtime stuck at shutdown.

use super::outbox_tests::{options, until, within};
use super::*;
use crate::jsonrpc::{
    ConnectionEnd, READER_ABORT_SETTLE, READER_SHUTDOWN, is_reader_shutdown_timeout,
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
    options().shutdown_timeout * 2 + READER_ABORT_SETTLE
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
