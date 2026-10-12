//! Values of [`Deadlines`](super::Deadlines), and the worst case of the close paths on a paused
//! clock.
//!
//! The close rows that include the removal of the MCP artifact are driven here, because only the
//! crate can hand a session a remover that stalls. The rest of the worst cases are driven from
//! `tests/deadlines.rs`.

use crate::test_support as support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use mango_external_agents::process::stop_process_with_limits;
use mango_external_agents::testing::FrozenClock;
use mango_external_agents::{
    CancelReason, CloseReason, Error, ExecutablePath, ExitStatus, HarnessIdentity,
    InterruptOutcome, Limits, ProcessControl, Result, Session, SessionId, SessionIds,
    SessionSnapshot, SessionState, SessionStatus, StopOutcome, TransportKind, TransportSelection,
    TurnRequest,
};
use support::{FakeClaudeCli, Run, SlowStop, host_under};
use tokio::time::Instant;

use super::{Deadlines, exit_grace, launch_wait, removal_bound};
use crate::mcp::{ConfigFile, DirectoryRemover};
use crate::session::ClaudeSession;

const NEVER: Duration = Duration::from_secs(60 * 60);
const MILLISECOND: Duration = Duration::from_millis(1);

fn secs(seconds: u64) -> Duration {
    Duration::from_secs(seconds)
}

/// A distinct value in every term, so a formula that swaps two of them fails. The idle timeout
/// sits above the ten-minute floor.
fn distinct() -> Limits {
    Limits {
        kill_grace: secs(3),
        shutdown_timeout: secs(7),
        idle_timeout: secs(15 * 60),
        ..Limits::default()
    }
}

/// Every figure of one `Deadlines`, named, in the order the guide lists them.
fn figures(deadlines: &Deadlines) -> [(&'static str, Duration); 15] {
    [
        ("process_stop", deadlines.process_stop()),
        ("teardown", deadlines.teardown()),
        ("cancel_to_terminal", deadlines.cancel_to_terminal()),
        ("cancel_to_return", deadlines.cancel_to_return()),
        ("close_no_turn", deadlines.close_no_turn()),
        ("close_with_turn", deadlines.close_with_turn()),
        ("close_pending_launch", deadlines.close_pending_launch()),
        (
            "close_after_pending_launch",
            deadlines.close_after_pending_launch(),
        ),
        ("close", deadlines.close()),
        ("close_worst_case", deadlines.close_worst_case()),
        ("exit_to_terminal", deadlines.exit_to_terminal()),
        ("exit_to_reaped", deadlines.exit_to_reaped()),
        (
            "silent_stream_to_terminal",
            deadlines.silent_stream_to_terminal(),
        ),
        (
            "host_shutdown_to_terminal",
            deadlines.host_shutdown_to_terminal(),
        ),
        (
            "dropped_session_to_reaped",
            deadlines.dropped_session_to_reaped(),
        ),
    ]
}

fn assert_figures(label: &str, received: [(&'static str, Duration); 15], expected: [u64; 15]) {
    for ((name, got), expected) in received.into_iter().zip(expected) {
        assert_eq!(
            got,
            secs(expected),
            "expected Deadlines::{name} = {expected} s {label} | received {got:?}"
        );
    }
}

// ----- values -----

#[test]
fn every_figure_at_the_defaults_is_the_one_the_guide_prints() {
    assert_figures(
        "at the defaults",
        figures(&Deadlines::from_limits(&Limits::default())),
        [14, 14, 0, 14, 5, 19, 24, 14, 24, 24, 2, 16, 602, 14, 14],
    );
}

#[test]
fn every_figure_follows_its_own_limit_when_the_limits_are_all_different() {
    // kill_grace 3 s, shutdown_timeout 7 s, idle_timeout 900 s: S = 2 * 3 + 2 * 7 = 20 s.
    assert_figures(
        "for kill_grace 3 s, shutdown_timeout 7 s, idle_timeout 900 s",
        figures(&Deadlines::from_limits(&distinct())),
        [20, 20, 0, 20, 7, 27, 34, 20, 34, 34, 3, 23, 903, 20, 20],
    );
}

#[test]
fn an_idle_timeout_below_the_floor_is_read_as_the_floor() {
    let limits = Limits {
        idle_timeout: secs(30),
        ..distinct()
    };
    assert_eq!(
        Deadlines::from_limits(&limits).silent_stream_to_terminal(),
        crate::pinned::STREAM_IDLE_TIMEOUT + secs(3),
        "expected max(idle_timeout, floor) + kill_grace | received a figure that ignored the floor"
    );
}

#[test]
fn the_shared_expressions_read_the_limit_the_session_waits_on() {
    let limits = distinct();
    assert_eq!(
        (
            launch_wait(&limits),
            removal_bound(&limits),
            exit_grace(&limits)
        ),
        (secs(7), secs(7), secs(3)),
        "expected (launch_wait, removal_bound, exit_grace) = (shutdown_timeout, shutdown_timeout, kill_grace) | received different limits"
    );
}

#[test]
fn a_limit_of_duration_max_saturates_every_figure_instead_of_panicking() {
    let each = [
        Limits {
            kill_grace: Duration::MAX,
            ..Limits::default()
        },
        Limits {
            shutdown_timeout: Duration::MAX,
            ..Limits::default()
        },
        Limits {
            idle_timeout: Duration::MAX,
            ..Limits::default()
        },
    ];
    for limits in each {
        for (name, figure) in figures(&Deadlines::from_limits(&limits)) {
            assert!(
                name == "cancel_to_terminal" || figure >= secs(2),
                "expected {name} not to shrink below its defaults when one limit is Duration::MAX | received {figure:?}"
            );
        }
    }
    let all = Limits {
        kill_grace: Duration::MAX,
        shutdown_timeout: Duration::MAX,
        idle_timeout: Duration::MAX,
        ..Limits::default()
    };
    for (name, figure) in figures(&Deadlines::from_limits(&all)) {
        let expected = if name == "cancel_to_terminal" {
            Duration::ZERO
        } else {
            Duration::MAX
        };
        assert_eq!(
            figure, expected,
            "expected Deadlines::{name} to saturate over Duration::MAX limits | received {figure:?}"
        );
    }
}

// ----- the stop, against the core -----

/// How the final wait of a stalling control ends.
#[derive(Clone, Copy)]
enum FinalWait {
    /// The child never exits, so the reap times out.
    NeverExits,
    /// The child exits this long after the kill answered.
    ExitsAfter(Duration),
}

/// A control that spends all of its bounds.
///
/// The interrupt is delivered just under `kill_grace`, no exit follows it, the kill answers just
/// under `shutdown_timeout`, and the reap ends just under `shutdown_timeout` or not at all.
struct StallingControl {
    interrupt_after: Duration,
    kill_after: Duration,
    final_wait: FinalWait,
    killed: AtomicBool,
}

#[async_trait::async_trait]
impl ProcessControl for StallingControl {
    fn pid(&self) -> Option<u32> {
        None
    }

    fn stderr_tail(&self) -> String {
        String::new()
    }

    async fn wait(&self) -> Result<ExitStatus> {
        if !self.killed.load(Ordering::Acquire) {
            return std::future::pending().await;
        }
        match self.final_wait {
            FinalWait::NeverExits => std::future::pending().await,
            FinalWait::ExitsAfter(after) => {
                tokio::time::sleep(after).await;
                Ok(ExitStatus::default())
            }
        }
    }

    async fn interrupt(&self, _reason: CancelReason) -> Result<InterruptOutcome> {
        tokio::time::sleep(self.interrupt_after).await;
        Ok(InterruptOutcome::Delivered)
    }

    async fn kill(&self, _reason: CancelReason) -> Result<()> {
        tokio::time::sleep(self.kill_after).await;
        self.killed.store(true, Ordering::Release);
        Ok(())
    }
}

fn spending_everything(limits: &Limits, final_wait: FinalWait) -> StallingControl {
    StallingControl {
        interrupt_after: limits.kill_grace - MILLISECOND,
        kill_after: limits.shutdown_timeout - MILLISECOND,
        final_wait,
        killed: AtomicBool::new(false),
    }
}

fn assert_window(what: &str, elapsed: Duration, bound: Duration, early: Duration) {
    assert!(
        elapsed <= bound && elapsed >= bound - early,
        "expected {what} within [{:?}, {bound:?}] | received {elapsed:?}",
        bound - early
    );
}

/// The core's stop formula is what `process_stop` repeats. This runs the core against a control
/// that spends every stage and compares the two, so a change in the core fails here.
#[tokio::test(start_paused = true)]
async fn the_core_stop_that_spends_every_stage_takes_what_process_stop_says() {
    for limits in [Limits::default(), distinct()] {
        let bound = Deadlines::from_limits(&limits).process_stop();
        let control = spending_everything(&limits, FinalWait::NeverExits);
        let began = Instant::now();
        let error = stop_process_with_limits(&control, CancelReason::Requested, &limits)
            .await
            .expect_err("expected a reap that never answers to time out");
        assert!(
            matches!(&error, Error::Timeout { operation, after }
                if operation == "process reaping" && *after == limits.shutdown_timeout),
            "expected Error::Timeout naming process reaping | received {error:?}"
        );
        // The interrupt and the kill answer one millisecond early; the two waits run out exactly.
        assert_window(
            "a stop that spends every stage (process_stop)",
            began.elapsed(),
            bound,
            MILLISECOND * 2,
        );
    }
}

#[tokio::test(start_paused = true)]
async fn the_core_stop_that_succeeds_at_the_end_of_its_bound_takes_what_process_stop_says() {
    for limits in [Limits::default(), distinct()] {
        let bound = Deadlines::from_limits(&limits).process_stop();
        let control = spending_everything(
            &limits,
            FinalWait::ExitsAfter(limits.shutdown_timeout - MILLISECOND),
        );
        let began = Instant::now();
        let outcome = stop_process_with_limits(&control, CancelReason::Requested, &limits)
            .await
            .expect("expected a reap inside its bound to succeed");
        assert_eq!(
            outcome,
            StopOutcome::Terminated,
            "expected Terminated for a child that ignored the interrupt | received {outcome:?}"
        );
        assert_window(
            "a stop that succeeds in its last stage (process_stop)",
            began.elapsed(),
            bound,
            MILLISECOND * 3,
        );
    }
}

// ----- the close paths, with the removal stalled -----

/// A host scratch mount that takes the removal and does not answer it until it is released.
///
/// It gives up by itself after ten real seconds, so a close with no bound of its own fails a test
/// instead of hanging it.
struct StalledRemover {
    arrived: Arc<tokio::sync::Notify>,
    released: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl DirectoryRemover for StalledRemover {
    fn remove(&self, directory: &std::path::Path) -> std::io::Result<()> {
        self.arrived.notify_one();
        let _ = self
            .released
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .recv_timeout(Duration::from_secs(10));
        std::fs::remove_dir_all(directory)
    }
}

/// A session over the fake `claude` whose artifact removal stalls.
struct StalledSession {
    session: Arc<ClaudeSession>,
    launcher: Arc<FakeClaudeCli>,
    arrived: Arc<tokio::sync::Notify>,
    release: std::sync::mpsc::Sender<()>,
    scratch: std::path::PathBuf,
}

impl Drop for StalledSession {
    fn drop(&mut self) {
        let _ = self.release.send(());
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

async fn stalled_session(launcher: FakeClaudeCli, limits: Limits) -> StalledSession {
    let scratch = std::env::temp_dir().join(format!("mea-deadlines-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&scratch).expect("expected a scratch root");
    let arrived = Arc::new(tokio::sync::Notify::new());
    let (release, released) = std::sync::mpsc::channel::<()>();
    let servers = vec![mango_external_agents::McpServer {
        name: String::from("docs"),
        transport: mango_external_agents::McpTransport::Stdio {
            command: String::from("docs-mcp"),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
        },
    }];
    let file = ConfigFile::write(&servers, &scratch)
        .await
        .expect("expected a written artifact")
        .expect("expected servers to produce one")
        .with_remover(StalledRemover {
            arrived: Arc::clone(&arrived),
            released: Mutex::new(released),
        });
    let launcher = Arc::new(launcher);
    let host = host_under(launcher.clone(), limits);
    let snapshot = SessionSnapshot::opening(
        SessionIds {
            session_id: SessionId::new("chat-1"),
            native_session_id: String::from("00000000-0000-4000-8000-000000000000"),
        },
        HarnessIdentity::claude(),
        TransportSelection::new(None, TransportKind::Stdio),
        host.now(),
    );
    let state = SessionState::new(Arc::new(FrozenClock::default()), snapshot);
    let session = Arc::new(ClaudeSession::new(
        host,
        ExecutablePath::default(),
        state,
        crate::permissions::ModeAvailability::default(),
        None,
        false,
        Some(file),
    ));
    StalledSession {
        session,
        launcher,
        arrived,
        release,
        scratch,
    }
}

/// What a close came to, read off the paused clock.
struct ClosedAfter {
    result: Result<()>,
    /// How long into the close the removal reached the host's mount, when it did.
    removal_began: Option<Duration>,
    total: Duration,
}

/// Waits for a close, letting a removal that reached the host's mount stall for its whole bound.
///
/// Time does not run on its own while the removal holds the blocking pool, so the stall is
/// advanced by hand.
async fn finish_close(
    closing: tokio::task::JoinHandle<Result<()>>,
    held: &StalledSession,
    began: Instant,
    removal: Duration,
) -> ClosedAfter {
    tokio::pin!(closing);
    let mut removal_began = None;
    let result = tokio::time::timeout(NEVER, async {
        loop {
            tokio::select! {
                result = &mut closing => return result.expect("expected the close task to finish"),
                () = held.arrived.notified() => {
                    removal_began = Some(began.elapsed());
                    tokio::time::advance(removal).await;
                }
            }
        }
    })
    .await
    .expect("expected the close to return | received none within an hour");
    ClosedAfter {
        result,
        removal_began,
        total: began.elapsed(),
    }
}

fn closing(held: &StalledSession) -> tokio::task::JoinHandle<Result<()>> {
    let session = Arc::clone(&held.session);
    tokio::spawn(async move { session.close(CloseReason::Requested).await })
}

/// A child the stop spends every stage on and then loses, with the last stage just under its
/// bound: the stop succeeds `process_stop` less three milliseconds after it began.
fn slowest_succeeding_stop(limits: &Limits) -> FakeClaudeCli {
    FakeClaudeCli::new()
        .with_turn(Run::stalling::<[String; 0], String>([]))
        .with_slow_stop(SlowStop {
            interrupt: Some(limits.kill_grace - MILLISECOND),
            kill: limits.shutdown_timeout - MILLISECOND,
            kill_ends_child: false,
        })
        .with_slow_reap(limits.shutdown_timeout - MILLISECOND)
}

#[tokio::test(start_paused = true)]
async fn a_close_with_no_turn_spends_the_removal_and_nothing_else() {
    let limits = distinct();
    let deadlines = Deadlines::from_limits(&limits);
    let held = stalled_session(FakeClaudeCli::new(), limits).await;

    let began = Instant::now();
    let closed = finish_close(closing(&held), &held, began, limits.shutdown_timeout).await;
    let error = closed
        .result
        .expect_err("expected a removal the mount never answers to time out");
    assert!(
        matches!(error.cause(), Error::Timeout { operation, after }
            if operation == crate::mcp::REMOVAL_OPERATION && *after == limits.shutdown_timeout),
        "expected Error::Timeout naming the MCP configuration removal | received {error:?}"
    );
    assert_eq!(
        closed.removal_began,
        Some(Duration::ZERO),
        "expected the removal to begin at once with no turn to stop | received {:?}",
        closed.removal_began
    );
    assert_eq!(
        closed.total,
        deadlines.close_no_turn(),
        "expected the close to take close_no_turn | received {:?}",
        closed.total
    );
}

#[tokio::test(start_paused = true)]
async fn a_close_with_a_turn_active_spends_the_whole_stop_and_then_the_removal() {
    let limits = distinct();
    let deadlines = Deadlines::from_limits(&limits);
    let held = stalled_session(slowest_succeeding_stop(&limits), limits).await;
    let _turn = held
        .session
        .start_turn(TurnRequest::new("turn-1", "hold"))
        .await
        .expect("expected a turn");

    let began = Instant::now();
    let closed = finish_close(closing(&held), &held, began, limits.shutdown_timeout).await;
    let error = closed
        .result
        .expect_err("expected the stalled removal to end the close with a timeout");
    assert!(
        matches!(error.cause(), Error::Timeout { operation, after }
            if operation == crate::mcp::REMOVAL_OPERATION && *after == limits.shutdown_timeout),
        "expected Error::Timeout naming the MCP configuration removal after a stop that succeeded | received {error:?}"
    );
    let removal_began = closed.removal_began.expect(
        "expected the removal to begin after a stop that succeeded | received a close that never removed",
    );
    assert_window(
        "the stop before the removal (process_stop)",
        removal_began,
        deadlines.process_stop(),
        MILLISECOND * 3,
    );
    assert_window(
        "a close with a turn active (close_with_turn)",
        closed.total,
        deadlines.close_with_turn(),
        MILLISECOND * 3,
    );
    assert_eq!(
        held.session.snapshot().status,
        SessionStatus::Closed,
        "expected a Closed session | received {:?}",
        held.session.snapshot().status
    );
}

/// The launch returns a child one millisecond before the close stops waiting for it. The stop
/// then runs a whole bound, so the close takes the shutdown stage it waited, plus the stop.
///
/// Whether the removal adds a third stage is a race, and this test does not decide it: the start
/// that owns the launch holds a lease on the artifact until its own stop returns, and a close that
/// finds the lease still held has nothing to remove. The assertion is therefore a window from the
/// two stages that must elapse to the figure that adds the third.
#[tokio::test(start_paused = true)]
async fn a_close_over_a_launch_that_arrives_just_in_time_spends_the_wait_and_the_whole_stop() {
    let limits = distinct();
    let deadlines = Deadlines::from_limits(&limits);
    let held = stalled_session(slowest_succeeding_stop(&limits), limits).await;
    let gate = held.launcher.gate_turn_spawns();
    let starting = tokio::spawn({
        let session = Arc::clone(&held.session);
        async move {
            session
                .start_turn(TurnRequest::new("turn-1", "hold"))
                .await
                .map(drop)
        }
    });
    gate.wait_for_spawn().await;

    let began = Instant::now();
    let closing = closing(&held);
    // Lets the close task begin its wait before the clock moves.
    tokio::task::yield_now().await;
    tokio::time::advance(limits.shutdown_timeout - MILLISECOND).await;
    gate.release();

    let closed = finish_close(closing, &held, began, limits.shutdown_timeout).await;
    // Ok when the start's lease left nothing to remove; the removal's own timeout when the lease
    // was already gone and the stalled removal ran its whole stage. Both are the same close.
    assert!(
        matches!(&closed.result, Ok(()))
            || matches!(&closed.result, Err(Error::Timeout { operation, after })
                if operation == crate::mcp::REMOVAL_OPERATION && *after == limits.shutdown_timeout),
        "expected a close over a child that was stopped to return Ok or the removal's Timeout | received {:?}",
        closed.result
    );
    let started = starting.await.expect("expected the start task to finish");
    assert!(
        matches!(&started, Err(error) if matches!(error.cause(), Error::Closed { .. })),
        "expected the start that lost the race to answer Error::Closed | received {started:?}"
    );

    // The wait for the launcher, then the stop, less what the fake answers early.
    let two_stages =
        limits.shutdown_timeout - MILLISECOND + deadlines.process_stop() - MILLISECOND * 3;
    assert_window(
        "the close over a launch that arrived in time (close_pending_launch)",
        closed.total,
        deadlines.close_pending_launch(),
        deadlines.close_pending_launch() - two_stages,
    );
}

/// A start future that the host keeps and never polls again starts no stop: the close takes the
/// short branch and returns at the shutdown stage with the pending-launch timeout, the session
/// `Closing`, and the late child alive until the future is polled.
#[tokio::test(start_paused = true)]
async fn a_close_over_a_launch_nobody_drives_gives_up_at_the_shutdown_stage() {
    let limits = distinct();
    let deadlines = Deadlines::from_limits(&limits);
    let held = stalled_session(
        FakeClaudeCli::new().with_turn(Run::stalling::<[String; 0], String>([])),
        limits,
    )
    .await;
    let gate = held.launcher.gate_turn_spawns();
    let mut start = held.session.start_turn(TurnRequest::new("turn-1", "hold"));
    // One poll takes the reservation and hands the launcher its own task; nothing polls it again.
    assert!(
        tokio::time::timeout(Duration::ZERO, &mut start)
            .await
            .is_err(),
        "expected the start to be parked on the launcher after its first poll"
    );
    gate.wait_for_spawn().await;

    let began = Instant::now();
    let closing = closing(&held);
    // Lets the close task begin its wait before the clock moves.
    tokio::task::yield_now().await;
    tokio::time::advance(limits.shutdown_timeout - MILLISECOND).await;
    gate.release();

    let error = tokio::time::timeout(NEVER, closing)
        .await
        .expect("expected the close to return | received none within an hour")
        .expect("expected the close task to finish")
        .expect_err("expected a close whose launch nobody drove to time out");
    assert!(
        matches!(error.cause(), Error::Timeout { operation, after }
            if operation == "a pending Claude launch" && *after == limits.shutdown_timeout),
        "expected Error::Timeout naming a pending Claude launch | received {error:?}"
    );
    assert_eq!(
        began.elapsed(),
        deadlines.close_no_turn(),
        "expected the short branch to take one shutdown stage | received {:?}",
        began.elapsed()
    );
    assert_eq!(
        (
            held.session.snapshot().status,
            held.launcher.a_child_is_running()
        ),
        (SessionStatus::Closing, true),
        "expected a Closing session and a late child nobody stopped | received ({:?}, {})",
        held.session.snapshot().status,
        held.launcher.a_child_is_running()
    );

    // Polling the start is what stops the child.
    let started = tokio::time::timeout(NEVER, start)
        .await
        .expect("expected the start to finish once polled | received none within an hour");
    assert!(
        started.is_err(),
        "expected the start that lost the race to refuse | received a stream"
    );
    assert!(
        held.launcher.all_children_ended(),
        "expected the late child to be stopped once the start was polled | received a running child"
    );
}
