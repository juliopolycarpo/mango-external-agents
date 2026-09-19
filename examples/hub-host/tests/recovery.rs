//! What the host must do when it cannot tell whether its submission arrived.
//!
//! Every test here is a case where the naive host — the one that reads an error as "it did not
//! happen" — runs the operation twice, silently reports a success nobody produced, or replays work
//! it has no proof did not run.

mod common;

use std::sync::Arc;

use hub_host::testing::{
    FakeHubApi, FakeVendorSession, HubCallKind, ReconcileAnswer, ReserveAnswer, TurnAnswer,
};
use hub_host::{Commit, HubError, HubStatus, Reconciliation, Settled, Stop};
use mango_external_agents::{AttemptId, Error, TerminalStatus, TurnId, TurnRequest};

/// A Hub that recorded the submission and then lost the answer on the way back.
///
/// The naive host retries and the operation runs twice. This one reconciles, finds the Hub
/// already holds the outcome, and submits exactly once.
#[tokio::test(start_paused = true)]
async fn a_lost_acknowledgement_is_reconciled_rather_than_submitted_again() {
    let hub = Arc::new(
        FakeHubApi::new()
            .reserving([ReserveAnswer::AcceptThenDropAcknowledgement])
            .reconciling([ReconcileAnswer::Answer(Reconciliation::Answered(
                HubStatus::Committed {
                    terminal: TerminalStatus::Completed,
                },
            ))]),
    );
    let session = FakeVendorSession::new();
    let stop = Arc::new(Stop::new());
    let mut supervisor = common::supervisor(&session, &hub, &stop);

    let settled = supervisor
        .run(TurnRequest::new("turn-1", "ship it"))
        .await
        .expect("expected the operation to settle");

    assert_eq!(
        hub.count(HubCallKind::Reserve),
        1,
        "expected exactly one submission, received the call sequence {:?}",
        hub.sequence()
    );
    assert_eq!(
        settled,
        Settled::AlreadyCommitted {
            terminal: common::COMPLETED
        }
    );
}

/// Acceptance-unknown does not advance to a newer attempt on a hunch.
///
/// The Hub answering "never arrived" is what unlocks it, and the call sequence is the proof that
/// the reconciliation came first.
#[tokio::test(start_paused = true)]
async fn a_newer_attempt_waits_for_hub_proof_that_the_first_never_arrived() {
    let hub = Arc::new(
        FakeHubApi::new().reserving([ReserveAnswer::Fail(HubError::recoverable(
            "connection reset",
        ))]),
    );
    let session = FakeVendorSession::new();
    let stop = Arc::new(Stop::new());
    let mut supervisor = common::supervisor(&session, &hub, &stop);

    let settled = supervisor
        .run(TurnRequest::new("turn-1", "ship it"))
        .await
        .expect("expected the operation to settle");

    assert_eq!(
        settled,
        Settled::Committed {
            terminal: common::COMPLETED,
            commit: Commit::Recorded,
        }
    );
    assert_eq!(
        hub.sequence(),
        vec![
            HubCallKind::Reserve,
            HubCallKind::Reconcile,
            HubCallKind::Reserve,
            HubCallKind::Commit,
        ],
        "expected the reconciliation between the two submissions"
    );
    assert_eq!(
        hub.attempts(HubCallKind::Reserve),
        vec![AttemptId::new(1), AttemptId::new(2)],
        "expected the second submission to carry a strictly newer attempt"
    );
}

/// The vendor already did the work and the acknowledgement was lost on the way back.
///
/// The Hub holds the terminal, so it is consumed rather than produced again. The session's start
/// count is what says so: one, not two.
#[tokio::test(start_paused = true)]
async fn a_terminal_the_hub_already_holds_is_consumed_instead_of_run_again() {
    let hub = Arc::new(FakeHubApi::new().reconciling([ReconcileAnswer::Answer(
        Reconciliation::Answered(HubStatus::Committed {
            terminal: TerminalStatus::Completed,
        }),
    )]));
    let session = FakeVendorSession::new().answering([TurnAnswer::AcknowledgementLost]);
    let stop = Arc::new(Stop::new());
    let mut supervisor = common::supervisor(&session, &hub, &stop);

    let settled = supervisor
        .run(TurnRequest::new("turn-1", "ship it"))
        .await
        .expect("expected the operation to settle");

    assert_eq!(
        session.start_count(),
        1,
        "expected the vendor work to run once and then be consumed from the hub"
    );
    assert_eq!(
        settled,
        Settled::AlreadyCommitted {
            terminal: common::COMPLETED
        }
    );
    assert_eq!(
        hub.count(HubCallKind::Reserve),
        1,
        "expected exactly one submission, received the call sequence {:?}",
        hub.sequence()
    );
    assert_eq!(
        hub.count(HubCallKind::Commit),
        0,
        "expected no second commit of an outcome the hub already held"
    );
}

/// A Hub with no reconciliation query cannot prove anything, so nothing may be replayed.
///
/// The uncertainty becomes the caller's, as its own outcome. It is neither a silent replay nor a
/// success nobody produced.
#[tokio::test(start_paused = true)]
async fn an_operation_a_hub_cannot_reconcile_surfaces_as_uncertain() {
    let hub = Arc::new(
        FakeHubApi::new()
            .reserving([ReserveAnswer::AcceptThenDropAcknowledgement])
            .reconciling([ReconcileAnswer::Answer(Reconciliation::Unsupported)]),
    );
    let session = FakeVendorSession::new();
    let stop = Arc::new(Stop::new());
    let mut supervisor = common::supervisor(&session, &hub, &stop);

    let settled = supervisor
        .run(TurnRequest::new("turn-1", "ship it"))
        .await
        .expect("expected the operation to settle");

    let Settled::Uncertain { operation } = settled else {
        panic!("expected an uncertain operation, received {settled:?}");
    };
    assert_eq!(operation.attempt, AttemptId::FIRST);
    assert_eq!(
        hub.count(HubCallKind::Reserve),
        1,
        "expected zero re-submissions, received the call sequence {:?}",
        hub.sequence()
    );
    assert_eq!(
        session.start_count(),
        0,
        "expected the vendor never to be asked after an unreconcilable submission"
    );
}

/// The library's own dispatch certainty is native proof, so no Hub query is needed.
///
/// A refusal carrying `Dispatch::NotSubmitted` says the request never left this host. That is the
/// one case where a newer attempt is safe without asking anybody.
#[tokio::test(start_paused = true)]
async fn a_failure_that_never_left_the_host_needs_no_hub_query_to_retry() {
    let hub = Arc::new(FakeHubApi::new());
    let session = FakeVendorSession::new().answering([TurnAnswer::NotSubmitted]);
    let stop = Arc::new(Stop::new());
    let mut supervisor = common::supervisor(&session, &hub, &stop);

    let settled = supervisor
        .run(TurnRequest::new("turn-1", "ship it"))
        .await
        .expect("expected the operation to settle");

    assert_eq!(
        settled,
        Settled::Committed {
            terminal: common::COMPLETED,
            commit: Commit::Recorded,
        }
    );
    assert_eq!(
        hub.sequence(),
        vec![
            HubCallKind::Reserve,
            HubCallKind::Reserve,
            HubCallKind::Commit,
        ],
        "expected no reconciliation query for a request that never left this host"
    );
    assert_eq!(
        session.start_count(),
        1,
        "expected the refused dispatch not to count as vendor work"
    );
}

/// A logical turn id is the host's own, and reusing one for different content is a host bug.
///
/// Refused before anything is dispatched, so the refusal cannot itself duplicate work.
#[tokio::test(start_paused = true)]
async fn reusing_a_logical_turn_id_with_different_content_is_refused() {
    let hub = Arc::new(FakeHubApi::new());
    let session = FakeVendorSession::new();
    let stop = Arc::new(Stop::new());
    let mut supervisor = common::supervisor(&session, &hub, &stop);

    supervisor
        .run(TurnRequest::new("turn-1", "ship it"))
        .await
        .expect("expected the first operation to settle");
    let error = supervisor
        .run(TurnRequest::new("turn-1", "ship something else"))
        .await
        .expect_err("expected a refusal for a reused logical turn id");

    assert!(
        matches!(error, Error::HostConfiguration { .. }),
        "expected a host-configuration refusal, received {error:?}"
    );
    assert_eq!(
        hub.count(HubCallKind::Reserve),
        1,
        "expected the refusal to dispatch nothing, received the call sequence {:?}",
        hub.sequence()
    );
    let record = supervisor
        .record(&TurnId::new("turn-1"))
        .expect("expected the refusal to keep the record that refused it");
    assert_eq!(
        record.terminal(),
        Some(&TerminalStatus::Completed),
        "expected the first operation's committed terminal to survive the refusal"
    );
}
