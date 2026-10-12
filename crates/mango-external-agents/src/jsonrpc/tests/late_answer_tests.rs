//! An answer the handler's task starts after a close took the set of answers is taken down by that
//! close, and the grace for the answers can end where the caller says.
//!
//! The handler's task and the answers are stopped together, so the task can start an answer in
//! the moment between its last look at the flag that says it is cancelled and the abort that
//! lands. That moment is forced here with a hook that runs right after the set is taken.

use super::outbox_tests::{eventually, options, until, within};
use super::*;
use crate::jsonrpc::ClientState;
use std::future::pending;
use std::sync::PoisonError;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Counts how many times it was dropped, which is what taking a task down does to what it holds.
struct DropCount(Arc<AtomicUsize>);

impl Drop for DropCount {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

/// Starts, in the set of answers, a task that never finishes and says when it is dropped.
fn start_an_answer_that_never_finishes(state: &ClientState, dropped: &Arc<AtomicUsize>) {
    let witness = DropCount(Arc::clone(dropped));
    state
        .in_flight
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .spawn(async move {
            let _witness = witness;
            pending::<()>().await;
        });
}

fn connected() -> Client {
    Client::connect(
        ScriptedLink::new().into_link(),
        RecordingHandler::arc(None),
        options(),
    )
}

/// An answer the handler's task starts into the set after the close took it is aborted by the
/// close, and not left running until the client is dropped. It fails without the second drain.
#[tokio::test(start_paused = true)]
async fn a_close_takes_down_an_answer_started_after_it_took_the_set() {
    let client = connected();
    let dropped = Arc::new(AtomicUsize::new(0));
    let late = Arc::clone(&dropped);
    let state = Arc::clone(&client.state);
    *client
        .state
        .after_in_flight_taken
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(move || {
        start_an_answer_that_never_finishes(&state, &late);
    }));

    let closed = eventually("the close", client.close()).await;
    until("the late answer to be taken down", || {
        dropped.load(Ordering::Acquire) == 1
    })
    .await;
    let left = client
        .state
        .in_flight
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .len();
    let observed = (closed.is_ok(), dropped.load(Ordering::Acquire), left);
    assert_eq!(
        observed,
        (true, 1, 0),
        "expected the close to end with Ok and the late answer taken down, none left: (true, 1, 0) | received {observed:?} (closed, dropped, left)"
    );
}

/// A grace that is already over aborts what is still being composed without waiting for it.
#[tokio::test(start_paused = true)]
async fn a_drain_whose_grace_is_over_aborts_the_answers_at_once() {
    let client = connected();
    let dropped = Arc::new(AtomicUsize::new(0));
    start_an_answer_that_never_finishes(&client.state, &dropped);

    let began = tokio::time::Instant::now();
    within("the drain", client.state.drain_in_flight_until(began)).await;
    until("the answer to be taken down", || {
        dropped.load(Ordering::Acquire) == 1
    })
    .await;
    assert_eq!(
        began.elapsed(),
        Duration::ZERO,
        "expected the drain to end at once, its grace being over | received {:?}",
        began.elapsed()
    );
}

/// A grace that has time left is spent on an answer that does not finish, up to where it ends and
/// no further, and an answer that does finish ends the wait at once.
#[tokio::test(start_paused = true)]
async fn a_drain_waits_for_its_grace_for_an_answer_that_never_finishes() {
    let client = connected();
    let dropped = Arc::new(AtomicUsize::new(0));
    start_an_answer_that_never_finishes(&client.state, &dropped);
    let grace = Duration::from_millis(300);

    let began = tokio::time::Instant::now();
    eventually(
        "the drain",
        client.state.drain_in_flight_until(began + grace),
    )
    .await;
    assert_eq!(
        began.elapsed(),
        grace,
        "expected the drain to wait out its grace {grace:?} | received {:?}",
        began.elapsed()
    );

    let nothing = tokio::time::Instant::now();
    eventually(
        "the drain of nothing",
        client.state.drain_in_flight_until(nothing + grace),
    )
    .await;
    assert_eq!(
        nothing.elapsed(),
        Duration::ZERO,
        "expected a drain with nothing to wait for to end at once | received {:?}",
        nothing.elapsed()
    );
}
