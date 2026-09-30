//! Stopping has to be prompt and final, for all three of the reasons a host stops.
//!
//! Prompt, because a host that has to wait out a thirty-second backoff before it can shut down is
//! a host that kills the process instead. Final, because a retry timer that fires after the stop
//! must not put the operation back on the wire.

mod common;

use std::sync::Arc;
use std::time::Duration;

use hub_host::testing::{
    FailingCancelSession, FakeHubApi, FakeVendorSession, HubCallKind, TurnAnswer,
};
use hub_host::{Commit, HubApi, Settled, Stop, Supervisor};
use mango_external_agents::{
    CancelReason, Error, ExitStatus, ProcessControl, SystemClock, TurnId, TurnRequest,
};

/// More recoverable failures than any run will get through, so the supervisor is always backing
/// off when the stop lands.
const ENDLESS_FAILURES: usize = 1_000;

async fn stop_during_backoff_is_prompt_and_final(reason: CancelReason) {
    let hub = Arc::new(FakeHubApi::new().reserving_after_recoverable_failures(ENDLESS_FAILURES));
    let session = FakeVendorSession::new();
    let stop = Arc::new(Stop::new());
    let mut supervisor = common::supervisor(&session, &hub, &stop);
    let running = tokio::spawn({
        let request = TurnRequest::new("turn-1", "ship it");
        async move { supervisor.run(request).await }
    });

    common::until("the first submission", || {
        hub.count(HubCallKind::Reserve) >= 1
    })
    .await;
    stop.stop(reason);
    // Long past the backoff the supervisor was waiting out. A retry task that wakes here must
    // find the operation stopped rather than resume it.
    tokio::time::advance(common::MAX_DELAY * 8).await;

    let settled = running
        .await
        .expect("expected the run task to finish")
        .expect("expected the operation to settle");

    assert_eq!(settled, Settled::Stopped { reason });
    assert_eq!(
        hub.count(HubCallKind::Reserve),
        1,
        "expected no submission after the stop, received the call sequence {:?}",
        hub.sequence()
    );
}

#[tokio::test(start_paused = true)]
async fn an_explicit_abort_stops_the_operation_for_good() {
    stop_during_backoff_is_prompt_and_final(CancelReason::Requested).await;
}

#[tokio::test(start_paused = true)]
async fn revoked_consent_stops_the_operation_for_good() {
    stop_during_backoff_is_prompt_and_final(CancelReason::ConsentRevoked).await;
}

#[tokio::test(start_paused = true)]
async fn owner_shutdown_stops_the_operation_for_good() {
    stop_during_backoff_is_prompt_and_final(CancelReason::Shutdown).await;
}

/// A stop mid-transcript ends the vendor's own work rather than leaving it running.
#[tokio::test(start_paused = true)]
async fn a_stop_during_a_live_turn_cancels_the_vendor_with_the_host_s_reason() {
    let hub = Arc::new(FakeHubApi::new());
    let session = FakeVendorSession::new().by_default(TurnAnswer::CompleteWhenReleased);
    let stop = Arc::new(Stop::new());
    let mut supervisor = common::supervisor(&session, &hub, &stop);
    let running = tokio::spawn({
        let request = TurnRequest::new("turn-1", "ship it");
        async move { supervisor.run(request).await }
    });

    common::until("the turn to start", || session.start_count() == 1).await;
    stop.stop(CancelReason::ConsentRevoked);
    tokio::time::advance(Duration::from_millis(1)).await;

    let settled = running
        .await
        .expect("expected the run task to finish")
        .expect("expected the operation to settle");

    assert_eq!(
        settled,
        Settled::Stopped {
            reason: CancelReason::ConsentRevoked
        }
    );
    assert_eq!(
        session.cancels(),
        vec![CancelReason::ConsentRevoked],
        "expected the vendor's own work to be cancelled with the host's reason"
    );
    assert_eq!(
        hub.count(HubCallKind::Commit),
        0,
        "expected a stopped operation to commit nothing"
    );
}

/// A stop that lands while the vendor is still acknowledging must not wait out the deadline.
///
/// The submission is the one window with neither a transcript to select on nor a backoff to
/// interrupt: the attempt deadline is the only other future in the race. A host that observes the
/// stop only when that deadline expires makes an abort, a revoked consent and an owner shutdown
/// all take the full deadline — which is exactly how long a host has to wait before it gives up
/// and kills the process instead.
#[tokio::test(start_paused = true)]
async fn a_stop_while_the_vendor_is_acknowledging_does_not_wait_out_the_deadline() {
    let hub = Arc::new(FakeHubApi::new());
    let session = FakeVendorSession::new().answering([TurnAnswer::AcknowledgeWhenReleased]);
    let stop = Arc::new(Stop::new());
    let mut supervisor = common::supervisor(&session, &hub, &stop);
    let running = tokio::spawn({
        let request = TurnRequest::new("turn-1", "ship it");
        async move { supervisor.run(request).await }
    });

    common::until("the submission to reach the vendor", || {
        hub.count(HubCallKind::Reserve) == 1
    })
    .await;
    let pulled_at = tokio::time::Instant::now();
    stop.stop(CancelReason::Requested);
    let settled = running
        .await
        .expect("expected the run task to finish")
        .expect("expected the operation to settle");
    let after_the_stop = pulled_at.elapsed();

    assert_eq!(
        settled,
        Settled::Stopped {
            reason: CancelReason::Requested
        }
    );
    assert_eq!(
        after_the_stop,
        Duration::ZERO,
        "expected the stop to end the submission at once rather than after the {:?} attempt deadline",
        common::ATTEMPT_DEADLINE
    );
    assert_eq!(
        session.start_count(),
        0,
        "expected a turn the vendor never acknowledged not to count as started"
    );
}

/// Aborting one operation must not take the session's remaining operations with it.
///
/// The supervisor keeps a record per logical turn and is built to drive more than one, so a single
/// signal for its whole life makes that unusable: the first abort settles every later `run` — for
/// any turn id, however unrelated — as stopped on the old reason, before it dispatches anything.
#[tokio::test(start_paused = true)]
async fn aborting_one_turn_leaves_the_session_able_to_run_the_next() {
    let hub = Arc::new(FakeHubApi::new());
    let session = FakeVendorSession::new().answering([TurnAnswer::CompleteWhenReleased]);
    let stop = Arc::new(Stop::new());
    let mut supervisor = common::supervisor(&session, &hub, &stop);
    let aborting = supervisor.abort_signal(&TurnId::new("turn-1"));
    let running = tokio::spawn(async move {
        let settled = supervisor.run(TurnRequest::new("turn-1", "ship it")).await;
        (supervisor, settled)
    });

    common::until("the first turn to start", || session.start_count() == 1).await;
    aborting.stop(CancelReason::Requested);
    let (mut supervisor, first) = running.await.expect("expected the run task to finish");
    let first = first.expect("expected the aborted operation to settle");
    let second = supervisor
        .run(TurnRequest::new("turn-2", "ship the next thing"))
        .await
        .expect("expected the second operation to settle");

    assert_eq!(
        first,
        Settled::Stopped {
            reason: CancelReason::Requested
        }
    );
    assert_eq!(
        second,
        Settled::Committed {
            terminal: common::COMPLETED,
            commit: Commit::Recorded,
        },
        "expected an unrelated turn to run after the abort"
    );
    assert_eq!(
        hub.count(HubCallKind::Reserve),
        2,
        "expected the second turn to be submitted, received the call sequence {:?}",
        hub.sequence()
    );
    assert_eq!(
        session.start_count(),
        2,
        "expected the second turn to reach the vendor"
    );
}

/// An abort is final for the turn id it named, the way a Hub refusal is.
///
/// One signal per logical turn id, kept for the supervisor's life, so running that id again does
/// not resume it. An operation the owner stopped is not one a retry loop gets to pick back up;
/// asking for the work after all means a new logical turn id, which says so honestly.
#[tokio::test(start_paused = true)]
async fn an_aborted_turn_id_is_not_resumed_by_running_it_again() {
    let hub = Arc::new(FakeHubApi::new());
    let session = FakeVendorSession::new();
    let stop = Arc::new(Stop::new());
    let mut supervisor = common::supervisor(&session, &hub, &stop);
    supervisor
        .abort_signal(&TurnId::new("turn-1"))
        .stop(CancelReason::Requested);

    let aborted = supervisor
        .run(TurnRequest::new("turn-1", "ship it"))
        .await
        .expect("expected the aborted operation to settle");
    let renamed = supervisor
        .run(TurnRequest::new("turn-2", "ship it"))
        .await
        .expect("expected the renamed operation to settle");

    assert_eq!(
        aborted,
        Settled::Stopped {
            reason: CancelReason::Requested
        }
    );
    assert_eq!(
        renamed,
        Settled::Committed {
            terminal: common::COMPLETED,
            commit: Commit::Recorded,
        },
        "expected a new logical turn id to be the way to ask for the work after all"
    );
    assert_eq!(
        hub.attempts(HubCallKind::Reserve).len(),
        1,
        "expected only the renamed turn to be submitted, received the call sequence {:?}",
        hub.sequence()
    );
    assert_eq!(
        session.start_count(),
        1,
        "expected the aborted turn never to reach the vendor"
    );
}

/// Shutdown is the one that *is* session-wide, and stays that way.
///
/// The owner going away ends every operation this supervisor has left, which is the whole reason
/// the injected signal is not replaced by the per-turn one.
#[tokio::test(start_paused = true)]
async fn an_owner_shutdown_stops_every_turn_the_session_has_left() {
    let hub = Arc::new(FakeHubApi::new());
    let session = FakeVendorSession::new();
    let stop = Arc::new(Stop::new());
    let mut supervisor = common::supervisor(&session, &hub, &stop);
    stop.stop(CancelReason::Shutdown);

    let settled = supervisor
        .run(TurnRequest::new("turn-1", "ship it"))
        .await
        .expect("expected the operation to settle");

    assert_eq!(
        settled,
        Settled::Stopped {
            reason: CancelReason::Shutdown
        }
    );
    assert_eq!(
        hub.count(HubCallKind::Reserve),
        0,
        "expected a shut-down session to submit nothing, received the call sequence {:?}",
        hub.sequence()
    );
    assert_eq!(
        session.start_count(),
        0,
        "expected a shut-down session never to reach the vendor"
    );
}

/// The first reason wins, so a shutdown racing an abort cannot rewrite the audit trail.
#[test]
fn a_stop_keeps_the_reason_it_was_first_given() {
    let stop = Stop::new();
    stop.stop(CancelReason::ConsentRevoked);
    stop.stop(CancelReason::Shutdown);

    assert_eq!(
        stop.reason(),
        Some(CancelReason::ConsentRevoked),
        "expected the first reason to stand"
    );
}

/// A child the vendor could not reap, retained so a test can prove the host gets the same one back.
struct UnreapedControl;

#[async_trait::async_trait]
impl ProcessControl for UnreapedControl {
    fn pid(&self) -> Option<u32> {
        None
    }

    fn stderr_tail(&self) -> String {
        String::new()
    }

    async fn wait(&self) -> mango_external_agents::Result<ExitStatus> {
        Err(Error::Closed {
            subject: "unreaped test process",
        })
    }

    async fn kill(&self, _reason: CancelReason) -> mango_external_agents::Result<()> {
        Ok(())
    }
}

/// Runs one turn on `session`, stops it mid-turn, and returns the supervisor with the outcome.
async fn stop_a_live_turn_on(
    session: FailingCancelSession,
    stopping: &FakeVendorSession,
) -> (Supervisor, mango_external_agents::Result<Settled>) {
    let hub = Arc::new(FakeHubApi::new());
    let stop = Arc::new(Stop::new());
    let mut supervisor = Supervisor::new(
        Box::new(session),
        Arc::clone(&hub) as Arc<dyn HubApi>,
        common::policy(),
        Arc::clone(&stop),
        Arc::new(SystemClock),
    );
    let request = TurnRequest::new("turn-1", "ship it");
    let abort = supervisor.abort_signal(&TurnId::new("turn-1"));
    let stopper = tokio::spawn({
        let stopping = stopping.clone();
        async move {
            common::until("the turn to start", || stopping.start_count() == 1).await;
            abort.stop(CancelReason::Requested);
        }
    });
    let settled = supervisor.run(request).await;
    stopper.await.expect("expected the stopper task to finish");
    (supervisor, settled)
}

/// A stop the vendor could not confirm is not reported as a completed stop.
///
/// The host only learns the work may still be running from the answer to `run`, so a plain
/// `Stopped` would tell it there is nothing left to clean up.
#[tokio::test(start_paused = true)]
async fn a_cancel_that_times_out_is_not_reported_as_a_completed_stop() {
    let inner = FakeVendorSession::new().by_default(TurnAnswer::CompleteWhenReleased);
    let session = FailingCancelSession::timing_out(inner.clone());

    let (mut supervisor, settled) = stop_a_live_turn_on(session, &inner).await;

    assert!(
        matches!(settled, Err(Error::Timeout { .. })),
        "expected Err(Timeout) from the failed cancel, received {settled:?}"
    );
    // Running the turn again is how a host retries the stop. The first run dropped the stream, so
    // an answer of `Stopped` here would turn a stop nobody confirmed into one that looks confirmed.
    let again = supervisor.run(TurnRequest::new("turn-1", "ship it")).await;
    assert!(
        matches!(again, Err(Error::Timeout { .. })),
        "expected the rerun to retry the cancel and surface Err(Timeout) again, received {again:?}"
    );
    assert_eq!(
        inner.start_count(),
        1,
        "expected the stopped turn not to be dispatched again"
    );
}

/// The handle to the unreaped child reaches the host, so it can retry the kill or escalate.
#[tokio::test(start_paused = true)]
async fn a_cancel_that_needs_cleanup_hands_the_host_the_same_process_handle() {
    let control: Arc<dyn ProcessControl> = Arc::new(UnreapedControl);
    let inner = FakeVendorSession::new().by_default(TurnAnswer::CompleteWhenReleased);
    let session = FailingCancelSession::requiring_cleanup(inner.clone(), Arc::clone(&control));

    let (_supervisor, settled) = stop_a_live_turn_on(session, &inner).await;

    let error = settled.expect_err("expected the failed cancel to surface as an error");
    let received = error
        .cleanup_control()
        .unwrap_or_else(|| panic!("expected a cleanup handle on the error, received {error:?}"));
    assert!(
        Arc::ptr_eq(&received, &control),
        "expected the host to receive the very handle the session returned, received a different one"
    );
}

/// Retrying a stop is running the turn again, and it confirms the stop once the vendor can.
///
/// The first run's stream is gone, so the retry has nothing to drain: the cancel it sends is the
/// only thing that can turn the unconfirmed stop into a confirmed one.
#[tokio::test(start_paused = true)]
async fn running_a_turn_again_retries_the_cancel_and_confirms_the_stop() {
    let inner = FakeVendorSession::new().by_default(TurnAnswer::CompleteWhenReleased);
    let session = FailingCancelSession::timing_out(inner.clone()).recovering_after(1);

    let (mut supervisor, settled) = stop_a_live_turn_on(session, &inner).await;
    assert!(
        matches!(settled, Err(Error::Timeout { .. })),
        "expected Err(Timeout) from the failed cancel, received {settled:?}"
    );
    let again = supervisor
        .run(TurnRequest::new("turn-1", "ship it"))
        .await
        .expect("expected the retried stop to settle");

    assert_eq!(
        again,
        Settled::Stopped {
            reason: CancelReason::Requested
        },
        "expected Stopped once the retried cancel succeeded"
    );
    assert_eq!(
        inner.cancels(),
        vec![CancelReason::Requested],
        "expected the vendor to receive the stop exactly once, from the retry"
    );
    let third = supervisor
        .run(TurnRequest::new("turn-1", "ship it"))
        .await
        .expect("expected the confirmed stop to settle");
    assert_eq!(
        third,
        Settled::Stopped {
            reason: CancelReason::Requested
        }
    );
    assert_eq!(
        inner.cancels().len(),
        1,
        "expected a confirmed stop not to be cancelled a third time"
    );
    assert_eq!(
        inner.start_count(),
        1,
        "expected the stopped turn not to be dispatched again"
    );
}

/// The retry after a cleanup failure still hands the host the handle, not a bare `Stopped`.
///
/// The host that retries a stop is the host that needs the process handle to retry the kill, so
/// it has to come back on every attempt and not only the first.
#[tokio::test(start_paused = true)]
async fn a_rerun_after_a_cleanup_failure_hands_the_host_the_same_process_handle_again() {
    let control: Arc<dyn ProcessControl> = Arc::new(UnreapedControl);
    let inner = FakeVendorSession::new().by_default(TurnAnswer::CompleteWhenReleased);
    let session = FailingCancelSession::requiring_cleanup(inner.clone(), Arc::clone(&control));

    let (mut supervisor, first) = stop_a_live_turn_on(session, &inner).await;
    assert!(
        first.is_err(),
        "expected the first run to surface the failed cancel, received {first:?}"
    );
    let again = supervisor.run(TurnRequest::new("turn-1", "ship it")).await;

    let error = again.expect_err("expected the rerun to surface the still-failing cancel");
    let received = error.cleanup_control().unwrap_or_else(|| {
        panic!("expected a cleanup handle on the rerun's error, received {error:?}")
    });
    assert!(
        Arc::ptr_eq(&received, &control),
        "expected the rerun to hand back the very handle the session returned, received a different one"
    );
    assert_eq!(
        inner.start_count(),
        1,
        "expected the stopped turn not to be dispatched again"
    );
}
