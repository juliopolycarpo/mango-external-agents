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

/// What `claude -p --resume <unknown uuid> --output-format stream-json --verbose` printed on
/// stdout with an empty `CLAUDE_CONFIG_DIR` (Claude Code 2.1.296, signed out): this one record,
/// before any `system/init`, then exit 1. Its stderr was the same sentence as `errors[0]`.
const MISSING_CONVERSATION_RESULT: &str = r#"{"type":"result","subtype":"error_during_execution","duration_ms":0,"duration_api_ms":0,"is_error":true,"num_turns":0,"stop_reason":null,"session_id":"3f2c1a90-1b2c-4d3e-8f4a-5b6c7d8e9f01","total_cost_usd":0,"usage":{"output_tokens_details":{"thinking_tokens":0},"input_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":0,"server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},"service_tier":"standard","cache_creation":{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":0},"inference_geo":"","iterations":[],"speed":"standard","fallback_credit":null},"modelUsage":{},"permission_denials":[],"uuid":"cda4c431-888f-4a71-8740-1f03574505ac","errors":["No conversation found with session ID: 3f2c1a90-1b2c-4d3e-8f4a-5b6c7d8e9f01"],"result_index":0}"#;
const MISSING_ID: &str = "3f2c1a90-1b2c-4d3e-8f4a-5b6c7d8e9f01";

fn failure_of(events: &[EventKind]) -> mango_external_agents::VendorError {
    match events.last() {
        Some(EventKind::Error { error }) => error.clone(),
        other => panic!("expected a terminal error event | received: {other:?}"),
    }
}

/// The CLI ended a strict resume with an errored `result` before it announced the session. That
/// shape — not the vendor's wording — is what the harness reads, so a host can tell a conversation
/// the CLI would not start from a transport failure (`claude-stream-broken`).
#[tokio::test]
async fn a_strict_resume_the_cli_ends_before_init_fails_with_its_own_code() {
    let launcher = Arc::new(
        FakeClaudeCli::new()
            .with_graceful_interrupt()
            .with_turn(Run::replaying(MISSING_CONVERSATION_RESULT)),
    );
    let session = open_resuming(&launcher, MISSING_ID).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "continue"))
        .await
        .expect("expected the deferred resume to start");
    let error = failure_of(&drain(&mut turn).await);

    assert_eq!(
        error.code.as_str(),
        "claude-resume-failed",
        "expected error code: claude-resume-failed | received: {}",
        error.code
    );
    assert!(!error.retryable, "expected a non-retryable failure");
    assert_resumability(
        session.as_ref(),
        Resumability::Unverified,
        "a resume the CLI ended before init",
    );
}

/// The code must not depend on the sentence. The same structure with other words, or with none,
/// is the same failure.
#[tokio::test]
async fn the_resume_failure_code_does_not_depend_on_the_vendors_wording() {
    let wordings = [
        r#""errors":["Failed to resume session: transcript unreadable"]"#,
        r#""errors":[]"#,
        r#""result":"something else entirely""#,
    ];
    for wording in wordings {
        let record = format!(
            r#"{{"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":0,{wording}}}"#
        );
        let launcher = Arc::new(FakeClaudeCli::new().with_turn(Run::replaying(&record)));
        let session = open_resuming(&launcher, MISSING_ID).await;
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "continue"))
            .await
            .expect("expected the deferred resume to start");
        let error = failure_of(&drain(&mut turn).await);
        assert_eq!(
            error.code.as_str(),
            "claude-resume-failed",
            "expected error code for {wording}: claude-resume-failed | received: {}",
            error.code
        );
    }
}

/// A failure after `system/init` is a failure of the turn, not of the resume: the conversation was
/// found, so the code stays the result's own and the conversation stays resumable.
#[tokio::test]
async fn an_errored_result_after_a_confirmed_resume_keeps_the_results_own_code() {
    let failed = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":1,"errors":["overloaded"]}"#;
    let launcher = Arc::new(
        FakeClaudeCli::new().with_turn(Run::replaying(&format!("{}\n{failed}", init(NATIVE_ID)))),
    );
    let session = open_resuming(&launcher, NATIVE_ID).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "continue"))
        .await
        .expect("expected the deferred resume to start");
    let error = failure_of(&drain(&mut turn).await);

    assert_eq!(
        error.code.as_str(),
        "claude-error_during_execution",
        "expected error code: claude-error_during_execution | received: {}",
        error.code
    );
    assert_resumability(
        session.as_ref(),
        Resumability::Resumable,
        "a failed turn on a confirmed resume",
    );
}

/// A fresh session has no resume to fail, so an errored result with no init stays what it was.
#[tokio::test]
async fn an_errored_result_on_a_fresh_session_is_not_a_resume_failure() {
    let launcher =
        Arc::new(FakeClaudeCli::new().with_turn(Run::replaying(MISSING_CONVERSATION_RESULT)));
    let session = open(&launcher).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "hello"))
        .await
        .expect("expected a turn");
    let error = failure_of(&drain(&mut turn).await);

    assert_eq!(
        error.code.as_str(),
        "claude-error_during_execution",
        "expected error code: claude-error_during_execution | received: {}",
        error.code
    );
}
