//! The fake Hub has to behave like a consistent Hub, or the tests built on it prove nothing.
//!
//! A scripted answer is what an earlier call would have left behind. If the fake forgets it as
//! soon as it is spent, a rerun sees an empty ledger and the "Hub" happily records a terminal it
//! had just said it already held, which no real Hub does.

use hub_host::testing::{CommitAnswer, FakeHubApi};
use hub_host::{Commit, HubApi, HubStatus, Reconciliation};
use mango_external_agents::{
    AttemptId, ErrorCode, OperationRef, SessionId, TerminalStatus, TurnId,
};

fn operation() -> OperationRef {
    OperationRef::new(
        SessionId::new("hub-chat-1"),
        TurnId::new("turn-1"),
        AttemptId::FIRST,
    )
}

fn failed() -> TerminalStatus {
    TerminalStatus::Failed {
        code: ErrorCode::from_static("hub-recorded-failure"),
    }
}

/// The scripted answer is one call's, but what it says the Hub holds stays true afterwards.
#[tokio::test]
async fn a_scripted_already_recorded_terminal_keeps_being_the_terminal_the_hub_holds() {
    let hub = FakeHubApi::new().committing([CommitAnswer::AlreadyRecorded { terminal: failed() }]);

    let first = hub
        .commit(&operation(), &TerminalStatus::Completed)
        .await
        .expect("expected the scripted commit to answer");
    let again = hub
        .commit(&operation(), &TerminalStatus::Completed)
        .await
        .expect("expected the unscripted commit to answer");

    assert_eq!(first, Commit::AlreadyRecorded { terminal: failed() });
    assert_eq!(
        again,
        Commit::AlreadyRecorded { terminal: failed() },
        "expected the hub to keep answering the terminal it already held, received {again:?}"
    );
}

/// Reconciliation reads the same ledger the commit wrote, so the two answers cannot disagree.
#[tokio::test]
async fn reconciling_after_a_scripted_already_recorded_commit_answers_that_terminal() {
    let hub = FakeHubApi::new().committing([CommitAnswer::AlreadyRecorded { terminal: failed() }]);
    hub.commit(&operation(), &TerminalStatus::Completed)
        .await
        .expect("expected the scripted commit to answer");

    let answer = hub
        .reconcile(&operation())
        .await
        .expect("expected reconciliation to answer");

    assert_eq!(
        answer,
        Reconciliation::Answered(HubStatus::Committed { terminal: failed() }),
        "expected the terminal the commit reported, received {answer:?}"
    );
}
