//! The worst case of each path in the Deadlines section of `docs/harness-claude.md`, driven on a
//! paused clock through the fake `claude` and compared with `mango_agent_claude::deadlines`.
//!
//! Each test makes the child as slow as its bound allows. A call that outlasts its bound ends the
//! stop there, so the fake answers one millisecond inside each bound, and the elapsed time falls
//! short of the formula by that many milliseconds and no more. The assertions are a window: no
//! later than the function's value, and no earlier than the value less the stages the fake answers
//! early.
//!
//! The close rows that include the artifact removal cannot be reached from here, because the
//! removal can only be stalled from inside the crate; `src/deadlines/tests.rs` drives them.

mod support;

use std::sync::Arc;
use std::time::Duration;

use mango_agent_claude::ClaudeHarness;
use mango_agent_claude::deadlines::Deadlines;
use mango_external_agents::{
    CancelReason, Error, EventKind, Harness, Limits, OpenSession, Session, TurnRequest, TurnStream,
};
use support::{FakeClaudeCli, Run, SlowStop, host_under};
use tokio::time::Instant;

const NEVER: Duration = Duration::from_secs(60 * 60);
const MILLISECOND: Duration = Duration::from_millis(1);

/// Limits with a distinct value in every term, so a formula that swaps two of them fails.
fn limits() -> Limits {
    Limits {
        kill_grace: Duration::from_secs(3),
        shutdown_timeout: Duration::from_secs(7),
        idle_timeout: Duration::from_secs(15 * 60),
        ..Limits::default()
    }
}

fn stalled() -> Run {
    Run::stalling::<[String; 0], String>([])
}

/// A child that takes the interrupt and answers the kill inside their bounds, exits for neither,
/// and is reaped `shutdown_timeout` less a millisecond after the kill answered. The stop spends
/// every stage and still succeeds, which is what a stop that is not allowed to fail needs.
fn slowest_stop(limits: &Limits) -> (SlowStop, Duration) {
    (
        SlowStop {
            interrupt: Some(limits.kill_grace - MILLISECOND),
            kill: limits.shutdown_timeout - MILLISECOND,
            kill_ends_child: false,
        },
        limits.shutdown_timeout - MILLISECOND,
    )
}

fn slow_launcher(limits: &Limits) -> Arc<FakeClaudeCli> {
    let (stop, reap) = slowest_stop(limits);
    Arc::new(
        FakeClaudeCli::new()
            .with_turn(stalled())
            .with_slow_stop(stop)
            .with_slow_reap(reap),
    )
}

/// What the fake answers early: one millisecond in each of the four stages of a full stop.
const FULL_STOP_EARLY: Duration = Duration::from_millis(3);

async fn open_under(launcher: &Arc<FakeClaudeCli>, limits: Limits) -> Arc<dyn Session> {
    let host = host_under(launcher.clone(), limits);
    Arc::from(
        ClaudeHarness::new()
            .open_session(&host, OpenSession::new("chat-1"))
            .await
            .expect("expected a session"),
    )
}

async fn terminal_of(turn: &mut TurnStream) -> Vec<EventKind> {
    tokio::time::timeout(NEVER, async {
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
    .await
    .expect("expected the turn to reach a terminal event | received none within an hour")
}

/// Asserts `elapsed` is no later than `bound` and no earlier than `bound` less `early`.
fn assert_window(what: &str, elapsed: Duration, bound: Duration, early: Duration) {
    assert!(
        elapsed <= bound && elapsed >= bound - early,
        "expected {what} within [{:?}, {bound:?}] | received {elapsed:?}",
        bound - early
    );
}

/// `cancel` writes the terminal at once and returns when the whole stop has.
#[tokio::test(start_paused = true)]
async fn a_cancel_writes_its_terminal_at_once_and_returns_after_the_whole_stop() {
    let limits = limits();
    let deadlines = Deadlines::from_limits(&limits);
    let launcher = slow_launcher(&limits);
    let session = open_under(&launcher, limits).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "hold"))
        .await
        .expect("expected a turn");

    let began = Instant::now();
    let cancelling = tokio::spawn({
        let session = Arc::clone(&session);
        async move { session.cancel(CancelReason::Requested).await }
    });
    let events = terminal_of(&mut turn).await;
    assert!(
        events.contains(&EventKind::Cancelled {
            reason: CancelReason::Requested
        }),
        "expected the turn to end cancelled | received {events:?}"
    );
    assert_eq!(
        began.elapsed(),
        deadlines.cancel_to_terminal(),
        "expected the terminal at cancel_to_terminal | received {:?}",
        began.elapsed()
    );

    tokio::time::timeout(NEVER, cancelling)
        .await
        .expect("expected the cancel to return | received none within an hour")
        .expect("expected the cancel task to finish")
        .expect("expected a stop that succeeded at the end of its bound to return Ok");
    assert_window(
        "cancel to return (cancel_to_return)",
        began.elapsed(),
        deadlines.cancel_to_return(),
        FULL_STOP_EARLY,
    );
}

/// The host's shutdown token stops the child before the pump writes the terminal.
#[tokio::test(start_paused = true)]
async fn a_host_shutdown_writes_the_terminal_after_the_whole_stop() {
    let limits = limits();
    let deadlines = Deadlines::from_limits(&limits);
    let launcher = slow_launcher(&limits);
    let host = host_under(launcher.clone(), limits);
    let session = ClaudeHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "hold"))
        .await
        .expect("expected a turn");

    let began = Instant::now();
    host.cancel().cancel();
    let events = terminal_of(&mut turn).await;
    assert!(
        events.contains(&EventKind::Cancelled {
            reason: CancelReason::Shutdown
        }),
        "expected the turn to end cancelled by the shutdown | received {events:?}"
    );
    assert_window(
        "host shutdown to terminal (host_shutdown_to_terminal)",
        began.elapsed(),
        deadlines.host_shutdown_to_terminal(),
        FULL_STOP_EARLY,
    );
}

/// A session dropped with a child installed reaps it by the end of the stop.
#[tokio::test(start_paused = true)]
async fn a_dropped_session_reaps_its_child_after_the_whole_stop() {
    let limits = limits();
    let deadlines = Deadlines::from_limits(&limits);
    let launcher = slow_launcher(&limits);
    let session = open_under(&launcher, limits).await;
    let _turn = session
        .start_turn(TurnRequest::new("turn-1", "hold"))
        .await
        .expect("expected a turn");

    let began = Instant::now();
    drop(session);
    tokio::time::timeout(NEVER, launcher.turn_reaped())
        .await
        .expect("expected the dropped session's child to be reaped | received none within an hour");
    assert_window(
        "dropped session to reaped (dropped_session_to_reaped)",
        began.elapsed(),
        deadlines.dropped_session_to_reaped(),
        FULL_STOP_EARLY,
    );
}

/// A child that goes quiet and stays alive ends its turn at the idle timer, floored, plus one
/// kill grace for an exit status that never comes. The timer is above the ten-minute floor here,
/// so a harness that read the floor instead of the host's value ends it early.
#[tokio::test(start_paused = true)]
async fn a_silent_child_ends_its_turn_at_the_idle_timer_plus_a_kill_grace() {
    let limits = limits();
    let deadlines = Deadlines::from_limits(&limits);
    let launcher = Arc::new(FakeClaudeCli::new().with_turn(stalled()));
    let session = open_under(&launcher, limits).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "hold"))
        .await
        .expect("expected a turn");
    // The prompt was written and stdin closed; the idle timer starts there.
    launcher.turn_reading().await;

    let began = Instant::now();
    let events = terminal_of(&mut turn).await;
    assert!(
        matches!(events.last(), Some(EventKind::Error { error }) if error.code.as_str() == "claude-stream-broken"),
        "expected a claude-stream-broken terminal | received {events:?}"
    );
    assert_eq!(
        began.elapsed(),
        deadlines.silent_stream_to_terminal(),
        "expected the terminal at silent_stream_to_terminal | received {:?}",
        began.elapsed()
    );
    assert_eq!(
        deadlines.silent_stream_to_terminal(),
        Duration::from_secs(15 * 60) + limits.kill_grace,
        "expected an idle_timeout above the floor to be the one read"
    );
    launcher.end_turns_for_host_cleanup();
}

/// A child that exited with its stdout still held open is read on for one kill grace.
#[tokio::test(start_paused = true)]
async fn a_child_that_exited_ends_its_turn_one_kill_grace_later() {
    let limits = limits();
    let deadlines = Deadlines::from_limits(&limits);
    let launcher = Arc::new(FakeClaudeCli::new().with_turn(stalled()));
    let session = open_under(&launcher, limits).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "hold"))
        .await
        .expect("expected a turn");
    launcher.turn_reading().await;

    let began = Instant::now();
    launcher.exit_turn_holding_its_output(1);
    let events = terminal_of(&mut turn).await;
    assert!(
        matches!(events.last(), Some(EventKind::Error { .. })),
        "expected an error terminal for a child that left without a result | received {events:?}"
    );
    assert_eq!(
        began.elapsed(),
        deadlines.exit_to_terminal(),
        "expected the terminal at exit_to_terminal | received {:?}",
        began.elapsed()
    );
    launcher.end_turns_for_host_cleanup();
}

/// A stream that ends under a child that keeps running: one kill grace for an exit status, the
/// terminal, then the whole stop. Reaped at `exit_to_reaped`.
///
/// The child that exited cannot take the whole stop: its `wait` answers at once, so the interrupt
/// stage ends early. A closed stream under a live child is the shape that spends every stage.
#[tokio::test(start_paused = true)]
async fn a_stream_that_ended_under_a_live_child_reaps_it_after_a_grace_and_a_whole_stop() {
    let limits = limits();
    let deadlines = Deadlines::from_limits(&limits);
    let launcher = slow_launcher(&limits);
    let session = open_under(&launcher, limits).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "hold"))
        .await
        .expect("expected a turn");
    launcher.turn_reading().await;

    let began = Instant::now();
    launcher.close_turn_output();
    let events = terminal_of(&mut turn).await;
    assert!(
        matches!(events.last(), Some(EventKind::Error { .. })),
        "expected an error terminal for a stream that ended without a result | received {events:?}"
    );
    assert_eq!(
        began.elapsed(),
        deadlines.exit_to_terminal(),
        "expected the terminal at exit_to_terminal | received {:?}",
        began.elapsed()
    );

    tokio::time::timeout(NEVER, launcher.turn_reaped())
        .await
        .expect("expected the child to be reaped | received none within an hour");
    assert_window(
        "end of stream to reaped (exit_to_reaped)",
        began.elapsed(),
        deadlines.exit_to_reaped(),
        FULL_STOP_EARLY,
    );
    drop(session);
}

/// A stop that cannot succeed still ends at the same bound, with the stop's own error.
#[tokio::test(start_paused = true)]
async fn a_stop_the_child_ignores_fails_at_the_process_stop_bound() {
    let limits = limits();
    let deadlines = Deadlines::from_limits(&limits);
    let (stop, _) = slowest_stop(&limits);
    let launcher = Arc::new(
        FakeClaudeCli::new()
            .with_turn(stalled())
            .with_slow_stop(stop),
    );
    let session = open_under(&launcher, limits).await;
    let _turn = session
        .start_turn(TurnRequest::new("turn-1", "hold"))
        .await
        .expect("expected a turn");

    let began = Instant::now();
    let error = tokio::time::timeout(NEVER, session.cancel(CancelReason::Requested))
        .await
        .expect("expected the cancel to return | received none within an hour")
        .expect_err("expected a stop the child ignored to fail");
    assert!(
        matches!(error.cause(), Error::Timeout { operation, .. } if operation == "process reaping"),
        "expected Error::Timeout naming process reaping | received {error:?}"
    );
    assert_window(
        "a failed stop (process_stop)",
        began.elapsed(),
        deadlines.process_stop(),
        FULL_STOP_EARLY - MILLISECOND,
    );
    launcher.end_turns_for_host_cleanup();
}
