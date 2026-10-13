//! What `SessionSnapshot::resumability` says about a Claude conversation after each way a turn can
//! end, against a scripted `claude`.
//!
//! The host reads the value on the terminal event, after `cancel` or `close` returns, and once the
//! session is closed, and stores it beside the vendor handle, so a case reads it at those three
//! moments. Nothing here waits on a timer to settle: a test waits on the session's state subscription, on a
//! terminal event, or on a `close`, which joins the stop it follows.

mod support;

use std::sync::Arc;
use std::time::Duration;

use mango_agent_claude::ClaudeHarness;
use mango_external_agents::{
    CancelReason, CancelToken, CloseReason, Error, EventKind, ExitStatus, Harness, HostContext,
    Limits, LineLimits, OpenSession, ProcessLauncher, Resumability, ResumeMode, Session,
    TurnRequest, TurnStream, VendorError,
};
use support::{ControlTap, FakeClaudeCli, Run, host, host_under};

const NATIVE_ID: &str = "aaaaaaaa-1111-2222-3333-444444444444";
const OTHER_ID: &str = "bbbbbbbb-1111-2222-3333-444444444444";
const RESULT: &str = r#"{"type":"result","is_error":false}"#;
const ASSISTANT: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"working"}]}}"#;

fn init(id: &str) -> String {
    format!(r#"{{"type":"system","subtype":"init","session_id":"{id}"}}"#)
}

fn unfinished(lines: Vec<String>, code: i32) -> Run {
    Run::Transcript {
        lines,
        exit: ExitStatus {
            code: Some(code),
            signal: None,
        },
    }
}

async fn open_on(host: &HostContext) -> Box<dyn Session> {
    ClaudeHarness::new()
        .open_session(host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session")
}

async fn open(launcher: &Arc<FakeClaudeCli>) -> Box<dyn Session> {
    open_on(&host(Arc::clone(launcher))).await
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

/// Everything a turn produced up to its terminal, and what the snapshot said at that moment.
async fn drain_within(
    session: &dyn Session,
    turn: &mut TurnStream,
    bound: Duration,
) -> (Vec<EventKind>, Resumability) {
    let collected = tokio::time::timeout(bound, async {
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
    let events = collected.expect("expected the turn to end rather than hang");
    (events, session.snapshot().resumability)
}

async fn drain(session: &dyn Session, turn: &mut TurnStream) -> (Vec<EventKind>, Resumability) {
    drain_within(session, turn, Duration::from_secs(5)).await
}

/// Waits until the pump has folded this run's `system/init` into the session, which publishes the
/// vendor's handle. The verdict does not change at `system/init`, so the handle is the signal.
async fn wait_for_init(session: &dyn Session) {
    let mut changes = session.subscribe();
    let reached = tokio::time::timeout(Duration::from_secs(5), async {
        while changes.current().ids.native_session_id != NATIVE_ID {
            if changes.changed().await.is_none() {
                return;
            }
        }
    })
    .await;
    assert!(
        reached.is_ok() && changes.current().ids.native_session_id == NATIVE_ID,
        "expected the vendor handle: {NATIVE_ID} | received: {}",
        changes.current().ids.native_session_id
    );
}

fn assert_resumability(session: &dyn Session, expected: Resumability, after: &str) {
    let received = session.snapshot().resumability;
    assert_eq!(
        received, expected,
        "expected resumability after {after}: {expected} | received: {received}"
    );
}

fn assert_is(received: Resumability, expected: Resumability, moment: &str) {
    assert_eq!(
        received, expected,
        "expected resumability {moment}: {expected} | received: {received}"
    );
}

/// A terminal written before the stop finishes may say `Unknown` (the stop is pending). When the
/// stop ends in a kill it never says `Resumable`; after a graceful interrupt it may.
fn assert_terminal_while_stopping(received: Resumability, graceful: bool, moment: &str) {
    let allowed: &[Resumability] = if graceful {
        &[Resumability::Unknown, Resumability::Resumable]
    } else {
        &[Resumability::Unknown, Resumability::UnfinishedTurn]
    };
    assert!(
        allowed.contains(&received),
        "expected resumability {moment}: one of {allowed:?} | received: {received}"
    );
}

fn failure_of(events: &[EventKind]) -> VendorError {
    match events.last() {
        Some(EventKind::Error { error }) => error.clone(),
        other => panic!("expected a terminal error event | received: {other:?}"),
    }
}

#[tokio::test]
async fn a_session_that_has_not_run_a_turn_is_unknown() {
    let launcher = Arc::new(FakeClaudeCli::new());
    let session = open(&launcher).await;
    assert_resumability(session.as_ref(), Resumability::Unknown, "opening");
}

// ---- Endings the vendor finished ------------------------------------------------------------

#[tokio::test]
async fn a_completed_turn_is_resumable_at_the_terminal_and_after_close() {
    let launcher = Arc::new(
        FakeClaudeCli::new().with_turn(Run::replaying(&format!("{}\n{RESULT}", init(NATIVE_ID)))),
    );
    let session = open(&launcher).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "hello"))
        .await
        .expect("expected a turn");
    let (_, at_terminal) = drain(session.as_ref(), &mut turn).await;
    assert_is(at_terminal, Resumability::Resumable, "on the terminal");

    session
        .close(CloseReason::Requested)
        .await
        .expect("expected the close to land");
    assert_resumability(session.as_ref(), Resumability::Resumable, "close");
}

#[tokio::test]
async fn a_failed_result_the_vendor_finished_keeps_the_conversation_resumable() {
    let failed = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":1,"errors":["overloaded"]}"#;
    let launcher = Arc::new(
        FakeClaudeCli::new().with_turn(Run::replaying(&format!("{}\n{failed}", init(NATIVE_ID)))),
    );
    let session = open(&launcher).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "hello"))
        .await
        .expect("expected a turn");
    let (events, at_terminal) = drain(session.as_ref(), &mut turn).await;
    assert_eq!(
        failure_of(&events).code.as_str(),
        "claude-error_during_execution"
    );
    assert_is(at_terminal, Resumability::Resumable, "on the terminal");
}

// ---- Endings the vendor did not finish, with nobody asking for a stop -----------------------

/// A child that exits mid-turn leaves a turn unfinished, whatever its status and whatever the
/// host's control would answer to a later interrupt (this one says it delivered it).
#[tokio::test]
async fn a_child_that_dies_mid_turn_leaves_the_conversation_unfinished() {
    for code in [143, 1, 0] {
        let launcher = Arc::new(FakeClaudeCli::new().with_graceful_interrupt().with_turn(
            unfinished(vec![init(NATIVE_ID), ASSISTANT.to_owned()], code),
        ));
        let session = open(&launcher).await;
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "work"))
            .await
            .expect("expected a turn");
        let (events, at_terminal) = drain(session.as_ref(), &mut turn).await;
        assert!(
            matches!(events.last(), Some(EventKind::Error { .. })),
            "expected a terminal error for exit {code} | received: {events:?}"
        );
        assert_is(
            at_terminal,
            Resumability::UnfinishedTurn,
            &format!("on the terminal after exit {code}"),
        );

        session
            .close(CloseReason::Requested)
            .await
            .expect("expected the close to land");
        assert_is(
            session.snapshot().resumability,
            Resumability::UnfinishedTurn,
            &format!("after close, exit {code}"),
        );
    }
}

/// A fresh conversation whose child died before `system/init` was never written, so there is
/// nothing unfinished to resume and the verdict stays `Unknown`.
#[tokio::test]
async fn a_child_that_dies_before_init_leaves_nothing_to_taint() {
    let launcher = Arc::new(
        FakeClaudeCli::new()
            .with_graceful_interrupt()
            .with_turn(Run::exiting(1)),
    );
    let session = open(&launcher).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "work"))
        .await
        .expect("expected a turn");
    let (_, at_terminal) = drain(session.as_ref(), &mut turn).await;
    assert_is(at_terminal, Resumability::Unknown, "on the terminal");
}

/// The snapshot read on the terminal event is already the final value, on a runtime where the pump
/// and the reader run on different threads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_terminal_of_a_dead_child_never_precedes_its_verdict() {
    for round in 0..200 {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_graceful_interrupt()
                .with_turn(unfinished(vec![init(NATIVE_ID), ASSISTANT.to_owned()], 1)),
        );
        let session = open(&launcher).await;
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "work"))
            .await
            .expect("expected a turn");
        let (_, at_terminal) = drain(session.as_ref(), &mut turn).await;
        assert_is(
            at_terminal,
            Resumability::UnfinishedTurn,
            &format!("on the terminal, round {round}"),
        );
    }
}

/// The idle timeout is a floor of ten minutes, so this runs on a paused clock.
#[tokio::test(start_paused = true)]
async fn a_stream_that_goes_silent_leaves_the_conversation_unfinished() {
    let launcher = Arc::new(
        FakeClaudeCli::new()
            .with_graceful_interrupt()
            .with_turn(Run::stalling([init(NATIVE_ID), ASSISTANT.to_owned()])),
    );
    let session = open(&launcher).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "work"))
        .await
        .expect("expected a turn");
    let (events, at_terminal) = drain_within(
        session.as_ref(),
        &mut turn,
        Duration::from_secs(60 * 60 * 3),
    )
    .await;
    assert!(
        matches!(events.last(), Some(EventKind::Error { .. })),
        "expected a terminal error for the idle timeout | received: {events:?}"
    );
    assert_is(at_terminal, Resumability::UnfinishedTurn, "on the terminal");

    session
        .close(CloseReason::Requested)
        .await
        .expect("expected the close to land");
    assert_resumability(session.as_ref(), Resumability::UnfinishedTurn, "close");
}

#[tokio::test]
async fn a_stream_the_harness_cannot_read_leaves_the_conversation_unfinished() {
    let oversized = "x".repeat(8192);
    let launcher = Arc::new(
        FakeClaudeCli::new()
            .with_graceful_interrupt()
            .with_turn(Run::stalling([init(NATIVE_ID), oversized])),
    );
    let narrow = Limits {
        line: LineLimits {
            max_line_bytes: 4096,
            max_buffered_bytes: 8192,
        },
        ..Limits::default()
    };
    let session = open_on(&host_under(Arc::clone(&launcher) as _, narrow)).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "work"))
        .await
        .expect("expected a turn");
    let (events, at_terminal) = drain(session.as_ref(), &mut turn).await;
    assert!(
        matches!(events.last(), Some(EventKind::Error { .. })),
        "expected a terminal error for the unreadable line | received: {events:?}"
    );
    assert_is(at_terminal, Resumability::UnfinishedTurn, "on the terminal");
}

// ---- Endings someone asked for ---------------------------------------------------------------

fn stalled(graceful: bool) -> Arc<FakeClaudeCli> {
    let launcher = FakeClaudeCli::new().with_turn(Run::stalling([init(NATIVE_ID)]));
    Arc::new(if graceful {
        launcher.with_graceful_interrupt()
    } else {
        launcher
    })
}

fn after_stop(graceful: bool) -> Resumability {
    if graceful {
        Resumability::Resumable
    } else {
        Resumability::UnfinishedTurn
    }
}

/// The stop took the path the case names, so a verdict is not read off a stop that never ran.
fn assert_stopped_by(launcher: &FakeClaudeCli, graceful: bool) {
    let (interrupts, kills) = (launcher.graceful_interrupts(), launcher.kill_requests());
    let (wanted_interrupts, wanted_kills) = if graceful { (1, 0) } else { (0, 1) };
    assert!(
        interrupts == wanted_interrupts && kills >= wanted_kills,
        "expected graceful interrupts: {wanted_interrupts}, kills: {wanted_kills}+ | received: {interrupts}, {kills}"
    );
}

#[tokio::test]
async fn cancel_records_how_it_stopped_the_child() {
    for graceful in [true, false] {
        let launcher = stalled(graceful);
        let session = open(&launcher).await;
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected a turn");
        wait_for_init(session.as_ref()).await;

        session
            .cancel(CancelReason::Requested)
            .await
            .expect("expected the cancel to land");
        let (_, at_terminal) = drain(session.as_ref(), &mut turn).await;

        assert_stopped_by(&launcher, graceful);
        // The terminal is written before the stop finishes: it may read `Unknown`, never a
        // `Resumable` the stop is about to withdraw. `cancel` has returned, so it is final here.
        assert_is(at_terminal, after_stop(graceful), "after cancel returned");
        session
            .close(CloseReason::Requested)
            .await
            .expect("expected the close to land");
        assert_resumability(session.as_ref(), after_stop(graceful), "close");
    }
}

/// Read on the terminal while the stop is still running, the value is never a `Resumable` that the
/// kill then withdraws. Different threads read and stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_terminal_written_before_a_kill_finishes_never_reads_resumable() {
    for round in 0..200 {
        let launcher = stalled(false);
        let session: Arc<dyn Session> = Arc::from(open(&launcher).await);
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected a turn");
        wait_for_init(session.as_ref()).await;

        let stopper = {
            let session = Arc::clone(&session);
            tokio::spawn(async move { session.cancel(CancelReason::Requested).await })
        };
        let (_, at_terminal) = drain(session.as_ref(), &mut turn).await;
        assert_terminal_while_stopping(
            at_terminal,
            false,
            &format!("on the terminal, round {round}"),
        );
        stopper
            .await
            .expect("expected the cancel task to finish")
            .expect("expected the cancel to land");
        assert_resumability(
            session.as_ref(),
            Resumability::UnfinishedTurn,
            &format!("cancel returned, round {round}"),
        );
    }
}

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
    drain(session.as_ref(), &mut turn).await;

    assert_resumability(
        session.as_ref(),
        Resumability::UnfinishedTurn,
        "a forced termination before init",
    );
}

#[tokio::test]
async fn close_records_how_it_stopped_the_child() {
    for graceful in [false, true] {
        let launcher = stalled(graceful);
        let session = open(&launcher).await;
        let _turn = session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected a turn");
        wait_for_init(session.as_ref()).await;

        session
            .close(CloseReason::Requested)
            .await
            .expect("expected the close to land");

        assert_stopped_by(&launcher, graceful);
        assert_resumability(session.as_ref(), after_stop(graceful), "close");
    }
}

#[tokio::test]
async fn a_cancelled_host_records_how_it_stopped_the_child() {
    for graceful in [false, true] {
        let launcher = stalled(graceful);
        let token = CancelToken::new();
        let host = HostContext::builder()
            .launcher(Arc::clone(&launcher) as Arc<dyn ProcessLauncher>)
            .cwd(std::env::temp_dir())
            .scratch(std::env::temp_dir())
            .client_info("mea-tests", "0.0.0")
            .cancel(token.clone())
            .build()
            .expect("expected a host");
        let session = open_on(&host).await;
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected a turn");
        wait_for_init(session.as_ref()).await;

        token.cancel();
        let (_, at_terminal) = drain(session.as_ref(), &mut turn).await;
        assert_terminal_while_stopping(at_terminal, graceful, "on the terminal");
        // `close` joins the stop the pump started, so it is the barrier that follows it.
        session
            .close(CloseReason::Requested)
            .await
            .expect("expected the close to land");

        assert_stopped_by(&launcher, graceful);
        assert_resumability(session.as_ref(), after_stop(graceful), "close");
    }
}

#[tokio::test]
async fn a_dropped_stream_records_how_it_stopped_the_child() {
    for graceful in [false, true] {
        let launcher = stalled(graceful);
        let session = open(&launcher).await;
        let turn = session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected a turn");
        wait_for_init(session.as_ref()).await;

        drop(turn);
        session
            .close(CloseReason::Requested)
            .await
            .expect("expected the close to land");

        assert_stopped_by(&launcher, graceful);
        assert_resumability(session.as_ref(), after_stop(graceful), "close");
    }
}

/// A host that drops the session handle and the stream without closing leaves the verdict where
/// the last stop put it. The subscription outlives the handle and ends with the session's state.
#[tokio::test]
async fn a_dropped_session_records_how_the_child_was_stopped() {
    for graceful in [false, true] {
        let launcher = stalled(graceful);
        let session = open(&launcher).await;
        let turn = session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected a turn");
        wait_for_init(session.as_ref()).await;
        let mut changes = session.subscribe();

        drop(turn);
        drop(session);
        let ended = tokio::time::timeout(Duration::from_secs(5), async {
            while changes.changed().await.is_some() {}
        })
        .await;
        assert!(
            ended.is_ok(),
            "expected the session state to end after the drop | received: still open"
        );

        assert_stopped_by(&launcher, graceful);
        assert_is(
            changes.current().resumability,
            after_stop(graceful),
            "after the session was dropped",
        );
    }
}

// ---- A stop that does not complete -----------------------------------------------------------

/// A refused kill leaves a child that may still be mid-turn: the least resumable state there is.
#[tokio::test]
async fn a_stop_that_fails_leaves_the_conversation_unfinished() {
    let inner = Arc::new(FakeClaudeCli::new().with_turn(Run::stalling([init(NATIVE_ID)])));
    let tap = Arc::new(ControlTap::refusing_kill(Arc::clone(&inner)));
    let session = open_on(&host_under(tap, Limits::default())).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "hold"))
        .await
        .expect("expected a turn");
    wait_for_init(session.as_ref()).await;

    let cancelled = session.cancel(CancelReason::Requested).await;
    let error = cancelled.expect_err("expected the refused kill to fail the cancel");
    assert!(
        error.cleanup_control().is_some(),
        "expected a CleanupRequired carrying the host's control | received: {error:?}"
    );
    assert_resumability(
        session.as_ref(),
        Resumability::UnfinishedTurn,
        "a cancel whose stop failed",
    );
    assert!(inner.a_child_is_running(), "expected the child to remain");

    let (_, at_terminal) = drain(session.as_ref(), &mut turn).await;
    assert_is(at_terminal, Resumability::UnfinishedTurn, "on the terminal");
    let _ = session.close(CloseReason::Requested).await;
    assert_resumability(
        session.as_ref(),
        Resumability::UnfinishedTurn,
        "a close after a failed stop",
    );
    inner.end_turns_for_host_cleanup();
}

/// A launcher that fails over a child it could not end leaves that child possibly running.
#[tokio::test]
async fn a_launch_that_fails_over_a_child_leaves_the_conversation_unfinished() {
    let inner = Arc::new(FakeClaudeCli::new().with_turn(Run::stalling([init(NATIVE_ID)])));
    let tap = Arc::new(ControlTap::failing_launch_over_a_child(Arc::clone(&inner)));
    let session = open_on(&host_under(tap, Limits::default())).await;

    let error = session
        .start_turn(TurnRequest::new("turn-1", "hold"))
        .await
        .expect_err("expected the launch to fail");
    assert!(
        error.cleanup_control().is_some(),
        "expected a CleanupRequired carrying the child's control | received: {error:?}"
    );
    assert_resumability(
        session.as_ref(),
        Resumability::UnfinishedTurn,
        "a failed launch over a child",
    );
    inner.end_turns_for_host_cleanup();
}

/// A turn refused before any launch ran nothing, so it changes nothing.
#[tokio::test]
async fn a_turn_refused_before_launch_changes_nothing() {
    let launcher = Arc::new(
        FakeClaudeCli::new().with_turn(Run::replaying(&format!("{}\n{RESULT}", init(NATIVE_ID)))),
    );
    let session = open(&launcher).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "hello"))
        .await
        .expect("expected a turn");
    drain(session.as_ref(), &mut turn).await;
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected the close to land");

    let refused = session
        .start_turn(TurnRequest::new("turn-2", "too late"))
        .await
        .expect_err("expected a closed session to refuse a turn");
    assert!(
        matches!(refused.cause(), Error::Closed { .. }),
        "expected a closed refusal | received: {refused:?}"
    );
    assert_resumability(
        session.as_ref(),
        Resumability::Resumable,
        "a turn refused before launch",
    );
}

// ---- A running turn, a finished turn whose reap fails, and a later turn -----------------------

/// A host that persists what it reads and then crashes mid-turn has last stored the value of a
/// running turn, so a running turn must not read `Resumable`: not at admission, not after
/// `system/init`, and not on a session that was `Resumable` after its previous turn.
#[tokio::test]
async fn a_running_turn_does_not_read_resumable() {
    let launcher = Arc::new(
        FakeClaudeCli::new()
            .with_graceful_interrupt()
            .with_turn(Run::replaying(&format!("{}\n{RESULT}", init(NATIVE_ID))))
            .with_turn(Run::stalling([init(NATIVE_ID)])),
    );
    let session = open(&launcher).await;
    let mut first = session
        .start_turn(TurnRequest::new("turn-1", "hello"))
        .await
        .expect("expected a turn");
    let (_, finished) = drain(session.as_ref(), &mut first).await;
    assert_is(finished, Resumability::Resumable, "on the first terminal");

    let mut second = session
        .start_turn(TurnRequest::new("turn-2", "again"))
        .await
        .expect("expected a second turn");
    assert_resumability(
        session.as_ref(),
        Resumability::Unknown,
        "the admission of a second turn",
    );
    // The same handle is announced again, so the second `system/init` publishes nothing; the
    // stalled child cannot finish, so what is read from here on is a running turn.
    assert_resumability(
        session.as_ref(),
        Resumability::Unknown,
        "a running second turn",
    );

    session
        .cancel(CancelReason::Requested)
        .await
        .expect("expected a graceful interruption");
    drain(session.as_ref(), &mut second).await;
    assert_resumability(
        session.as_ref(),
        Resumability::Resumable,
        "a graceful interrupt of the second turn",
    );
}

#[tokio::test]
async fn a_fresh_turn_reads_unknown_after_system_init() {
    let launcher = stalled(true);
    let session = open(&launcher).await;
    let _turn = session
        .start_turn(TurnRequest::new("turn-1", "hold"))
        .await
        .expect("expected a turn");
    wait_for_init(session.as_ref()).await;
    assert_resumability(
        session.as_ref(),
        Resumability::Unknown,
        "system/init of a running first turn",
    );
}

/// The turn finished, so the conversation is resumable; only the process is owed cleanup, and
/// that is the error `close` carries.
#[tokio::test]
async fn a_finished_turn_whose_child_cannot_be_reaped_stays_resumable() {
    let inner = Arc::new(
        FakeClaudeCli::new().with_turn(Run::stalling([init(NATIVE_ID), RESULT.to_owned()])),
    );
    let tap = Arc::new(ControlTap::refusing_kill(Arc::clone(&inner)));
    let session = open_on(&host_under(tap, Limits::default())).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "hello"))
        .await
        .expect("expected a turn");
    let (events, at_terminal) = drain(session.as_ref(), &mut turn).await;
    assert!(
        matches!(events.last(), Some(EventKind::Completed)),
        "expected the vendor's finished turn to complete | received: {events:?}"
    );
    assert_is(at_terminal, Resumability::Resumable, "on the terminal");

    let closed = session.close(CloseReason::Requested).await;
    let error = closed.expect_err("expected the refused kill to fail the close");
    assert!(
        error.cleanup_control().is_some(),
        "expected a CleanupRequired carrying the host's control | received: {error:?}"
    );
    assert_resumability(
        session.as_ref(),
        Resumability::Resumable,
        "a close whose reap of a finished turn failed",
    );
    inner.end_turns_for_host_cleanup();
}

/// After a child died mid-turn the session stays usable, and a later turn the vendor finishes
/// leaves the conversation's last turn finished.
#[tokio::test]
async fn a_later_finished_turn_clears_an_unfinished_one() {
    let launcher = Arc::new(
        FakeClaudeCli::new()
            .with_graceful_interrupt()
            .with_turn(unfinished(vec![init(NATIVE_ID), ASSISTANT.to_owned()], 1))
            .with_turn(Run::replaying(&format!("{}\n{RESULT}", init(NATIVE_ID)))),
    );
    let session = open(&launcher).await;
    let mut first = session
        .start_turn(TurnRequest::new("turn-1", "work"))
        .await
        .expect("expected a turn");
    let (_, dead) = drain(session.as_ref(), &mut first).await;
    assert_is(dead, Resumability::UnfinishedTurn, "on the first terminal");

    let mut second = session
        .start_turn(TurnRequest::new("turn-2", "again"))
        .await
        .expect("expected the session to stay usable after a child died");
    let (_, finished) = drain(session.as_ref(), &mut second).await;
    assert_is(
        finished,
        Resumability::Resumable,
        "on the terminal of the later turn",
    );
}

/// A child that had already exited when the interrupt was asked for did not end because of it,
/// whatever the host's control answers about delivering one to a dead process (this fake says
/// `Delivered`, as a control that signals a recycled or lingering pid can). The vendor's `result`
/// never came, so the turn is unfinished.
#[tokio::test]
async fn an_interrupt_after_the_child_already_exited_does_not_read_resumable() {
    for code in [143, 1, 0] {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_graceful_interrupt()
                .with_turn(Run::stalling([init(NATIVE_ID), ASSISTANT.to_owned()])),
        );
        let session = open(&launcher).await;
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "work"))
            .await
            .expect("expected a turn");
        wait_for_init(session.as_ref()).await;
        launcher.exit_turn_holding_its_output(code);

        session
            .cancel(CancelReason::Requested)
            .await
            .expect("expected the cancel to land");
        drain(session.as_ref(), &mut turn).await;
        assert_resumability(
            session.as_ref(),
            Resumability::UnfinishedTurn,
            &format!("cancel returned after the child exited {code}"),
        );

        session
            .close(CloseReason::Requested)
            .await
            .expect("expected the close to land");
        assert_resumability(
            session.as_ref(),
            Resumability::UnfinishedTurn,
            &format!("after close, child exited {code}"),
        );
    }
}

/// The verdict is conservative, the session is not refused: a later turn the vendor finishes
/// settles it, as after a child that died on its own.
#[tokio::test]
async fn an_interrupt_after_the_child_exited_leaves_the_session_usable() {
    let launcher = Arc::new(
        FakeClaudeCli::new()
            .with_graceful_interrupt()
            .with_turn(Run::stalling([init(NATIVE_ID), ASSISTANT.to_owned()]))
            .with_turn(Run::replaying(&format!("{}\n{RESULT}", init(NATIVE_ID)))),
    );
    let session = open(&launcher).await;
    let mut first = session
        .start_turn(TurnRequest::new("turn-1", "work"))
        .await
        .expect("expected a turn");
    wait_for_init(session.as_ref()).await;
    launcher.exit_turn_holding_its_output(143);
    session
        .cancel(CancelReason::Requested)
        .await
        .expect("expected the cancel to land");
    drain(session.as_ref(), &mut first).await;
    assert_resumability(
        session.as_ref(),
        Resumability::UnfinishedTurn,
        "cancel returned after the child exited",
    );

    let mut second = session
        .start_turn(TurnRequest::new("turn-2", "again"))
        .await
        .expect("expected a session that stays usable after an interrupt that ended nothing");
    let (_, finished) = drain(session.as_ref(), &mut second).await;
    assert_is(finished, Resumability::Resumable, "on the later terminal");
}

// ---- Strict resume ---------------------------------------------------------------------------

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
    drain(session.as_ref(), &mut turn).await;

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
        let (_, at_terminal) = drain(session.as_ref(), &mut turn).await;

        assert_is(at_terminal, Resumability::Unverified, label);
        assert_resumability(session.as_ref(), Resumability::Unverified, label);
    }
}

// ---- A strict resume the CLI ends before it starts -------------------------------------------

/// What `claude -p --resume <unknown uuid> --output-format stream-json --verbose` printed on
/// stdout with an empty `CLAUDE_CONFIG_DIR` (Claude Code 2.1.296, signed out): this one record,
/// before any `system/init`, then exit 1. Its stderr was the same sentence as `errors[0]`.
const MISSING_CONVERSATION_RESULT: &str = r#"{"type":"result","subtype":"error_during_execution","duration_ms":0,"duration_api_ms":0,"is_error":true,"num_turns":0,"stop_reason":null,"session_id":"3f2c1a90-1b2c-4d3e-8f4a-5b6c7d8e9f01","total_cost_usd":0,"usage":{"output_tokens_details":{"thinking_tokens":0},"input_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":0,"server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},"service_tier":"standard","cache_creation":{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":0},"inference_geo":"","iterations":[],"speed":"standard","fallback_credit":null},"modelUsage":{},"permission_denials":[],"uuid":"cda4c431-888f-4a71-8740-1f03574505ac","errors":["No conversation found with session ID: 3f2c1a90-1b2c-4d3e-8f4a-5b6c7d8e9f01"],"result_index":0}"#;
const MISSING_ID: &str = "3f2c1a90-1b2c-4d3e-8f4a-5b6c7d8e9f01";

/// The code and the message are the 0.7.0 ones. A host matching `claude-stream-broken` for a
/// refused resume keeps matching; the new facts are the snapshot's `Unverified` and the
/// result's subtype as the failure's `vendor_code`.
#[tokio::test]
async fn a_strict_resume_the_cli_ends_before_init_keeps_its_code_and_adds_facts() {
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
    let (events, at_terminal) = drain(session.as_ref(), &mut turn).await;
    let error = failure_of(&events);

    assert_eq!(
        error.code.as_str(),
        "claude-stream-broken",
        "expected error code: claude-stream-broken | received: {}",
        error.code
    );
    assert_eq!(
        error.message,
        Error::Protocol {
            expected: String::from(
                "a system/init record confirming the requested Claude resume handle before conversation content or result"
            ),
            received: String::from("a result record before resume confirmation"),
        }
        .to_string(),
        "expected the 0.7.0 message"
    );
    assert_eq!(
        error.vendor_code.as_deref(),
        Some("error_during_execution"),
        "expected vendor_code: error_during_execution | received: {:?}",
        error.vendor_code
    );
    assert!(!error.retryable, "expected a non-retryable failure");
    assert_is(at_terminal, Resumability::Unverified, "on the terminal");
}

/// A vendor result that is not a failure, or content before init, carries no vendor code: only an
/// errored `result` says the CLI ended the run.
#[tokio::test]
async fn a_strict_resume_refused_for_another_reason_carries_no_vendor_code() {
    let wordings = [
        (
            "a succeeded result",
            r#"{"type":"result","subtype":"success","is_error":false}"#,
        ),
        ("an assistant record", ASSISTANT),
    ];
    for (label, record) in wordings {
        let launcher = Arc::new(FakeClaudeCli::new().with_turn(Run::replaying(record)));
        let session = open_resuming(&launcher, MISSING_ID).await;
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "continue"))
            .await
            .expect("expected the deferred resume to start");
        let (events, _) = drain(session.as_ref(), &mut turn).await;
        let error = failure_of(&events);
        assert_eq!(
            error.code.as_str(),
            "claude-stream-broken",
            "expected error code for {label}: claude-stream-broken | received: {}",
            error.code
        );
        assert_eq!(
            error.vendor_code, None,
            "expected no vendor_code for {label} | received: {:?}",
            error.vendor_code
        );
    }
}

/// The subtype is read from the record's structure, never from the vendor's wording.
#[tokio::test]
async fn the_vendor_code_does_not_depend_on_the_vendors_wording() {
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
        let (events, _) = drain(session.as_ref(), &mut turn).await;
        let error = failure_of(&events);
        assert_eq!(
            error.vendor_code.as_deref(),
            Some("error_during_execution"),
            "expected vendor_code for {wording}: error_during_execution | received: {:?}",
            error.vendor_code
        );
    }
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
    let (events, _) = drain(session.as_ref(), &mut turn).await;
    let error = failure_of(&events);

    assert_eq!(
        error.code.as_str(),
        "claude-error_during_execution",
        "expected error code: claude-error_during_execution | received: {}",
        error.code
    );
}
