//! What `SessionSnapshot::resumability` says about a Claude conversation after each way a turn can
//! end, against a scripted `claude`.
//!
//! The host reads the value after `cancel` returns (or after a failed first strict-resume turn)
//! and stores it beside the vendor handle, so every assertion here reads it at that moment.

mod support;

use std::sync::Arc;
use std::time::Duration;

use mango_agent_claude::ClaudeHarness;
use mango_external_agents::{
    CancelReason, EventKind, Harness, OpenSession, Resumability, ResumeMode, Session, TurnRequest,
    TurnStream,
};
use support::{FakeClaudeCli, Run, host};

const NATIVE_ID: &str = "aaaaaaaa-1111-2222-3333-444444444444";
const OTHER_ID: &str = "bbbbbbbb-1111-2222-3333-444444444444";
const RESULT: &str = r#"{"type":"result","is_error":false}"#;

fn init(id: &str) -> String {
    format!(r#"{{"type":"system","subtype":"init","session_id":"{id}"}}"#)
}

async fn open(launcher: &Arc<FakeClaudeCli>) -> Box<dyn Session> {
    let host = host(Arc::clone(launcher));
    ClaudeHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session")
}

async fn open_resuming(launcher: &Arc<FakeClaudeCli>, id: &str) -> Box<dyn Session> {
    let host = host(Arc::clone(launcher));
    ClaudeHarness::new()
        .open_session(
            &host,
            OpenSession::new("chat-1").resuming(id, ResumeMode::Strict),
        )
        .await
        .expect("expected a strict resume session")
}

async fn drain(turn: &mut TurnStream) -> Vec<EventKind> {
    let collected = tokio::time::timeout(Duration::from_secs(5), async {
        let mut events = Vec::new();
        while let Some(event) = turn.recv().await {
            let terminal = event.is_terminal();
            events.push(event.kind);
            if terminal {
                break;
            }
        }
        events
    })
    .await;
    collected.expect("expected the turn to end rather than hang")
}

/// Waits until the session publishes `wanted`, so a test never races the pump's `system/init`.
async fn wait_for(session: &dyn Session, wanted: Resumability) {
    let mut changes = session.subscribe();
    let reached = tokio::time::timeout(Duration::from_secs(5), async {
        while changes.current().resumability != wanted {
            if changes.changed().await.is_none() {
                return;
            }
        }
    })
    .await;
    assert!(
        reached.is_ok() && changes.current().resumability == wanted,
        "expected resumability: {wanted} | received: {}",
        changes.current().resumability
    );
}

fn assert_resumability(session: &dyn Session, expected: Resumability, after: &str) {
    let received = session.snapshot().resumability;
    assert_eq!(
        received, expected,
        "expected resumability after {after}: {expected} | received: {received}"
    );
}

#[tokio::test]
async fn a_session_that_has_not_run_a_turn_is_unknown() {
    let launcher = Arc::new(FakeClaudeCli::new());
    let session = open(&launcher).await;
    assert_resumability(session.as_ref(), Resumability::Unknown, "opening");
}

#[tokio::test]
async fn a_completed_turn_leaves_the_conversation_resumable() {
    let launcher = Arc::new(
        FakeClaudeCli::new().with_turn(Run::replaying(&format!("{}\n{RESULT}", init(NATIVE_ID)))),
    );
    let session = open(&launcher).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "hello"))
        .await
        .expect("expected a turn");
    drain(&mut turn).await;
    assert_resumability(
        session.as_ref(),
        Resumability::Resumable,
        "a completed turn",
    );
}

#[tokio::test]
async fn a_graceful_interrupt_leaves_the_conversation_resumable() {
    let launcher = Arc::new(
        FakeClaudeCli::new()
            .with_graceful_interrupt()
            .with_turn(Run::stalling([init(NATIVE_ID)])),
    );
    let session = open(&launcher).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "hold"))
        .await
        .expect("expected a turn");
    wait_for(session.as_ref(), Resumability::Resumable).await;

    session
        .cancel(CancelReason::Requested)
        .await
        .expect("expected a graceful interruption");
    drain(&mut turn).await;

    assert_eq!(launcher.graceful_interrupts(), 1, "expected a SIGINT stop");
    assert_resumability(
        session.as_ref(),
        Resumability::Resumable,
        "a graceful interrupt",
    );
}

#[tokio::test]
async fn a_forced_termination_taints_the_conversation() {
    let launcher = Arc::new(FakeClaudeCli::new().with_turn(Run::stalling([init(NATIVE_ID)])));
    let session = open(&launcher).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "hold"))
        .await
        .expect("expected a turn");
    wait_for(session.as_ref(), Resumability::Resumable).await;

    session
        .cancel(CancelReason::Requested)
        .await
        .expect("expected the cancel to land");
    drain(&mut turn).await;

    assert_eq!(launcher.kill_requests(), 1, "expected a forced stop");
    assert_resumability(
        session.as_ref(),
        Resumability::TaintedByKill,
        "a forced termination",
    );
}

/// The kill is the verdict even when the child died before it ever announced a conversation.
#[tokio::test]
async fn a_forced_termination_before_init_taints_the_conversation() {
    let launcher =
        Arc::new(FakeClaudeCli::new().with_turn(Run::stalling::<[String; 0], String>([])));
    let session = open(&launcher).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "hold"))
        .await
        .expect("expected a turn");

    session
        .cancel(CancelReason::Requested)
        .await
        .expect("expected the cancel to land");
    drain(&mut turn).await;

    assert_resumability(
        session.as_ref(),
        Resumability::TaintedByKill,
        "a forced termination before init",
    );
}

#[tokio::test]
async fn a_confirmed_strict_resume_is_resumable() {
    let launcher = Arc::new(
        FakeClaudeCli::new().with_turn(Run::replaying(&format!("{}\n{RESULT}", init(NATIVE_ID)))),
    );
    let session = open_resuming(&launcher, NATIVE_ID).await;
    assert_resumability(
        session.as_ref(),
        Resumability::Unknown,
        "opening a strict resume that no turn has confirmed",
    );

    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "continue"))
        .await
        .expect("expected the deferred resume to start");
    drain(&mut turn).await;

    assert_resumability(
        session.as_ref(),
        Resumability::Resumable,
        "a confirmed strict resume",
    );
}

#[tokio::test]
async fn a_strict_resume_the_vendor_did_not_confirm_is_unverified() {
    let failures = [
        ("a different init handle", Run::replaying(&init(OTHER_ID))),
        (
            "a result before init",
            Run::replaying(r#"{"type":"result","is_error":false}"#),
        ),
        ("an ended stream without init", Run::exiting(0)),
    ];
    for (label, run) in failures {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_graceful_interrupt()
                .with_turn(run),
        );
        let session = open_resuming(&launcher, NATIVE_ID).await;
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "continue"))
            .await
            .expect("expected the deferred resume to start");
        drain(&mut turn).await;

        assert_resumability(session.as_ref(), Resumability::Unverified, label);
    }
}
