//! Stopping has to be prompt and final, for all three of the reasons a host stops.
//!
//! Prompt, because a host that has to wait out a thirty-second backoff before it can shut down is
//! a host that kills the process instead. Final, because a retry timer that fires after the stop
//! must not put the operation back on the wire.

mod common;

use std::sync::Arc;
use std::time::Duration;

use hub_host::testing::{FakeHubApi, FakeVendorSession, HubCallKind, TurnAnswer};
use hub_host::{Settled, Stop};
use mango_external_agents::{CancelReason, TurnRequest};

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
