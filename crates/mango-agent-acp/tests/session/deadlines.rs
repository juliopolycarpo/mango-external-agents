//! `mango_agent_acp::deadlines`, held to the harness it describes.
//!
//! Every test runs on a paused clock against the real session, connection and reaper. The agent
//! stops reading (its input never takes another byte) and its child ignores the interrupt, is slow
//! to be killed and slow to be reaped (see `stalling.rs`): each stage of the stop then runs to one
//! millisecond short of its limit, and the operation under test takes the sum of its stages,
//! which the test reads off the clock and compares with the figure `Deadlines` gives.
//!
//! The limits are 3 s of `kill_grace`, 7 s of `shutdown_timeout` and 101 s of `request_timeout`
//! (the rest far above any sum), so a figure that reads one limit for another lands elsewhere.
//!
//! What is driven to its worst case, and what is not:
//!
//! | Figure                      | Driven by                                                      |
//! | --------------------------- | -------------------------------------------------------------- |
//! | `process_stop`              | core's `stop_process_with_limits` against the stalling control |
//! | `teardown`                  | a dropped session mid-turn, to the turn's terminal             |
//! | `dropped_session_mid_turn`  | the same test                                                  |
//! | `cancel_to_terminal`        | a cancel the agent never reads                                 |
//! | `close`, `close_worst_case` | a close with `session/close` ignored; all but its last `st`    |
//! | `request_timeout_to_closed` | a held request; all but its last `st`                          |
//! | `exit_to_closed`            | in the crate (`client/commit_hold_tests.rs`); not as a whole   |
//!
//! The last `st` of `close`, `request_timeout_to_closed` and `exit_to_closed` waits for a turn
//! another task is settling after a cleanup that succeeded. No fake that can be built from outside
//! the crate holds that task there: the prompt task writes its terminal and frees the slot right
//! after the cleanup it joined. That wait is driven on its own, to its bound, in the crate's
//! `deadlines::tests`.

use super::stalling::{Reap, StallingLauncher, Stalls, assert_reaches};
use super::*;

use mango_agent_acp::deadlines::Deadlines;
use mango_external_agents::process::stop_process_with_limits;

fn secs(seconds: u64) -> Duration {
    Duration::from_secs(seconds)
}

/// The slack under a driven sum: the stalled stages each fall a millisecond short of their limit.
const SLACK: Duration = Duration::from_millis(10);

/// 3 s of grace, 7 s of shutdown, 101 s of request: every term of every figure is told apart, and
/// the limits no figure counts are far above the longest sum.
fn distinct_limits() -> Limits {
    Limits {
        kill_grace: secs(3),
        shutdown_timeout: secs(7),
        request_timeout: secs(101),
        idle_timeout: secs(5000),
        approval_timeout: secs(5000),
        cancel_settle_timeout: secs(6000),
        ..Limits::default()
    }
}

/// The same with the request deadline out of the way, for every path that is not a timeout.
fn distinct_limits_with_slow_requests() -> Limits {
    Limits {
        request_timeout: secs(4000),
        ..distinct_limits()
    }
}

fn host_over(launcher: &StallingLauncher, limits: Limits) -> HostContext {
    HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .limits(limits)
        .build()
        .expect("expected a host")
}

/// One open session whose child is stalled.
struct StalledSession {
    session: Arc<dyn Session>,
    stalls: Stalls,
    live: FakeLauncher,
    deadlines: Deadlines,
}

async fn open_stalled(agent: FakeProcess, limits: Limits, reap: Reap) -> StalledSession {
    let live = FakeLauncher::new();
    live.push(agent);
    let stalls = Stalls::against(&limits, reap);
    let launcher = StallingLauncher::over(live.clone(), stalls.clone());
    let session = AcpHarness::new(profile())
        .open_session(&host_over(&launcher, limits), OpenSession::new("deadlines"))
        .await
        .expect("expected a session");
    StalledSession {
        session: Arc::from(session),
        stalls,
        live,
        deadlines: Deadlines::from_limits(&limits),
    }
}

/// Reads the stream to its terminal, however long that takes on the paused clock.
async fn until_terminal(turn: &mut TurnStream) -> Vec<EventKind> {
    let mut seen = Vec::new();
    let ended = tokio::time::timeout(secs(3600), async {
        while let Some(event) = turn.recv().await {
            let terminal = event.is_terminal();
            seen.push(event.kind);
            if terminal {
                return true;
            }
        }
        false
    })
    .await;
    assert!(
        matches!(ended, Ok(true)),
        "expected a terminal event within an hour of virtual time | received: {seen:?}"
    );
    seen
}

/// Waits for the published status to be `Closed`, however long that takes on the paused clock.
async fn until_closed(session: &Arc<dyn Session>) {
    let mut lifecycle = session.subscribe();
    let waited = tokio::time::timeout(secs(3600), async {
        while lifecycle.current().status != SessionStatus::Closed {
            if lifecycle.changed().await.is_none() {
                return;
            }
        }
    })
    .await;
    let status = lifecycle.current().status;
    assert!(
        waited.is_ok() && status == SessionStatus::Closed,
        "expected session status: Closed within an hour of virtual time | received: {status:?}"
    );
}

/// A running turn on an agent that streams its updates and never answers or honours a cancel.
async fn start_running_turn(stalled: &StalledSession) -> TurnStream {
    let mut turn = stalled
        .session
        .start_turn(TurnRequest::new("turn-1", "work"))
        .await
        .expect("expected a turn");
    let first = turn.recv().await;
    assert!(
        first.is_some(),
        "expected the turn's first event: Some | received: None"
    );
    turn
}

fn closing_agent() -> FakeProcess {
    FakeAcpAgent::new().closing_sessions().process()
}

fn never_finishing_agent() -> FakeProcess {
    FakeAcpAgent::new().never_finishing_turns().process()
}

/// The stop of the child is `2 kill_grace + 2 shutdown_timeout`, on the core function the reaper
/// calls and for any control, not the built-in launcher's smaller stop.
#[tokio::test(start_paused = true)]
async fn the_process_stop_takes_the_sum_core_enforces() {
    for (reap, name) in [
        (Reap::Never, "a reap that never ends"),
        (Reap::JustInTime, "a slow reap"),
    ] {
        let limits = distinct_limits();
        let live = FakeLauncher::new();
        live.push(FakeProcess::responding(|_| Vec::new()));
        let child = live
            .spawn(mango_external_agents::LaunchSpec {
                argv: vec![String::from("fake-acp")],
                cwd: std::env::temp_dir(),
                env: Default::default(),
                stdin: true,
                hide_window: true,
            })
            .await
            .expect("expected a fake child");
        let control = Stalls::against(&limits, reap).control_over(child.control);

        let began = tokio::time::Instant::now();
        let stopped =
            stop_process_with_limits(control.as_ref(), CancelReason::Requested, &limits).await;
        let elapsed = began.elapsed();

        let outcome_matches = match reap {
            Reap::Never => stopped.is_err(),
            Reap::JustInTime => stopped.is_ok(),
        };
        assert!(
            outcome_matches,
            "expected the stop under {name} to end as {}: | received: {stopped:?}",
            if reap == Reap::Never {
                "Err(Timeout)"
            } else {
                "Ok"
            }
        );
        assert_reaches(
            &format!("a stop under {name}"),
            elapsed,
            Deadlines::from_limits(&limits).process_stop(),
            SLACK,
        );
    }
}

/// A session dropped under a running turn: the connection's close runs out its `shutdown_timeout`
/// behind a write the agent never reads, then the interrupt, its exit, the kill and the reap run
/// out theirs. The held stream's terminal is written at `teardown`.
#[tokio::test(start_paused = true)]
async fn a_dropped_session_ends_its_turn_at_the_teardown_bound() {
    let stalled = open_stalled(
        never_finishing_agent(),
        distinct_limits_with_slow_requests(),
        Reap::Never,
    )
    .await;
    let mut turn = start_running_turn(&stalled).await;
    let StalledSession {
        session,
        stalls,
        deadlines,
        ..
    } = stalled;
    stalls.block_writes();

    let began = tokio::time::Instant::now();
    drop(session);
    until_terminal(&mut turn).await;
    let elapsed = began.elapsed();

    assert_reaches(
        "a dropped session's turn to reach its terminal (Deadlines::dropped_session_mid_turn)",
        elapsed,
        deadlines.dropped_session_mid_turn(),
        SLACK,
    );
    assert_eq!(
        deadlines.dropped_session_mid_turn(),
        deadlines.teardown(),
        "expected dropped_session_mid_turn == teardown | received: {:?} and {:?}",
        deadlines.dropped_session_mid_turn(),
        deadlines.teardown()
    );
}

/// A cancel the agent never reads: the harness waits `kill_grace` for the turn to end and then
/// tears the connection down, so the terminal is written at `cancel_to_terminal`.
#[tokio::test(start_paused = true)]
async fn a_cancel_the_agent_ignores_ends_its_turn_at_the_cancel_bound() {
    let stalled = open_stalled(
        never_finishing_agent(),
        distinct_limits_with_slow_requests(),
        Reap::Never,
    )
    .await;
    let mut turn = start_running_turn(&stalled).await;
    stalled.stalls.block_writes();

    stalled
        .session
        .cancel(CancelReason::Requested)
        .await
        .expect("expected cancel to return once session/cancel is queued");
    let began = tokio::time::Instant::now();
    until_terminal(&mut turn).await;
    let elapsed = began.elapsed();

    assert_reaches(
        "an ignored cancel to reach the turn's terminal (Deadlines::cancel_to_terminal)",
        elapsed,
        stalled.deadlines.cancel_to_terminal(),
        SLACK,
    );
}

/// `close` on an agent that advertised `session/close` and stopped reading: the request runs out its
/// `shutdown_timeout`, the connection's close runs out another, and the stop spends every stage.
/// The one stage left, the wait for a turn another task is settling, is not reachable here.
#[tokio::test(start_paused = true)]
async fn a_close_with_every_stage_stalled_takes_all_but_the_turn_wait() {
    let limits = distinct_limits_with_slow_requests();
    let stalled = open_stalled(closing_agent(), limits, Reap::JustInTime).await;
    stalled.stalls.block_writes();

    let began = tokio::time::Instant::now();
    let closed = stalled.session.close(CloseReason::Requested).await;
    let elapsed = began.elapsed();

    assert!(
        closed.is_ok(),
        "expected a close whose stop finished inside its bounds to be: Ok | received: {closed:?}"
    );
    let status = stalled.session.snapshot().status;
    assert_eq!(
        status,
        SessionStatus::Closed,
        "expected session status: Closed | received: {status:?}"
    );
    // `session/close`, the connection's close, then the stop; the turn wait is the last `st`.
    assert_reaches(
        "a close, less its wait for a turn (Deadlines::close less shutdown_timeout)",
        elapsed,
        stalled.deadlines.close() - limits.shutdown_timeout,
        SLACK,
    );
    assert_eq!(
        stalled.live.live_children(),
        0,
        "expected live children after close: 0 | received: {}",
        stalled.live.live_children()
    );
}

/// The same close under the default limits: 5 + 5 + 14 = 24 s of the 29 s the guide gives.
#[tokio::test(start_paused = true)]
async fn a_stalled_close_under_the_defaults_takes_24_of_the_29_seconds() {
    let limits = Limits::default();
    let stalled = open_stalled(closing_agent(), limits, Reap::JustInTime).await;
    stalled.stalls.block_writes();

    let began = tokio::time::Instant::now();
    stalled
        .session
        .close(CloseReason::Requested)
        .await
        .expect("expected the close to succeed");
    let elapsed = began.elapsed();

    assert_eq!(
        stalled.deadlines.close(),
        secs(29),
        "expected Deadlines::close at the defaults: 29 s | received: {:?}",
        stalled.deadlines.close()
    );
    assert_reaches("a stalled close at the defaults", elapsed, secs(24), SLACK);
}

/// A close that joins the teardown the connection-loss watcher already began is the evidence that
/// no path takes a close past `close`, measured from its call: the join waits for a teardown that
/// started no later than `shutdown_timeout` after the call.
#[tokio::test(start_paused = true)]
async fn a_close_joining_the_watchers_teardown_ends_within_the_close_bound() {
    let gone = CancelToken::new();
    let limits = distinct_limits_with_slow_requests();
    let stalled = open_stalled(
        FakeAcpAgent::new()
            .closing_sessions()
            .process()
            .ending_stdout_when(gone.clone()),
        limits,
        Reap::JustInTime,
    )
    .await;
    stalled.stalls.block_writes();

    gone.cancel();
    let began = tokio::time::Instant::now();
    let closed = stalled.session.close(CloseReason::Requested).await;
    let elapsed = began.elapsed();

    assert!(
        closed.is_ok(),
        "expected a close joining the watcher's teardown to be: Ok | received: {closed:?}"
    );
    // The watcher's teardown began at the call, so the close ends one teardown later.
    assert_reaches(
        "a close joining a teardown begun at the call (Deadlines::teardown)",
        elapsed,
        stalled.deadlines.teardown(),
        SLACK,
    );
    assert!(
        elapsed <= stalled.deadlines.close_worst_case(),
        "expected a joined close within {:?} (close_worst_case) | received: {elapsed:?}",
        stalled.deadlines.close_worst_case()
    );
}

/// The agent exits part-way into a close: the watcher begins the teardown later than the call, and
/// the close, which would have begun it itself at `shutdown_timeout`, still ends within `close`.
#[tokio::test(start_paused = true)]
async fn an_exit_part_way_into_a_close_does_not_take_it_past_the_close_bound() {
    for exit_after in [
        secs(0),
        secs(3),
        secs(6),
        // One millisecond before the close would have given up on `session/close` itself.
        secs(7) - Duration::from_millis(1),
    ] {
        let gone = CancelToken::new();
        let limits = distinct_limits_with_slow_requests();
        let stalled = open_stalled(
            FakeAcpAgent::new()
                .closing_sessions()
                .process()
                .ending_stdout_when(gone.clone()),
            limits,
            Reap::JustInTime,
        )
        .await;
        stalled.stalls.block_writes();

        let began = tokio::time::Instant::now();
        let closing = tokio::spawn({
            let session = Arc::clone(&stalled.session);
            async move { session.close(CloseReason::Requested).await }
        });
        tokio::time::sleep(exit_after).await;
        gone.cancel();
        let closed = closing.await.expect("expected the close task to join");
        let elapsed = began.elapsed();

        assert!(
            closed.is_ok(),
            "expected a close with the agent exiting after {exit_after:?} to be: Ok | received: {closed:?}"
        );
        assert!(
            elapsed <= stalled.deadlines.close_worst_case(),
            "expected a close with the agent exiting after {exit_after:?} within {:?} (close_worst_case) | received: {elapsed:?}",
            stalled.deadlines.close_worst_case()
        );
        // The teardown began when the exit was noticed, and the close joined it.
        assert_reaches(
            &format!("a close with the agent exiting after {exit_after:?}"),
            elapsed,
            exit_after + stalled.deadlines.teardown(),
            SLACK,
        );
    }
}

/// A request the agent never answers: `request_timeout` after it was queued the connection is torn
/// down, and the session is `Closed` once the teardown is over. The wait for a turn to settle that
/// the figure ends with is not reachable here, as for `close`.
#[tokio::test(start_paused = true)]
async fn a_request_the_agent_never_answers_closes_the_session_at_the_request_bound() {
    let limits = distinct_limits();
    let agent = HeldRequestAgent::new("session/set_config_option");
    let stalled = open_stalled(agent.process(), limits, Reap::JustInTime).await;
    let configuring = tokio::spawn({
        let session = Arc::clone(&stalled.session);
        async move {
            session
                .configure(
                    ConfigurationPatch::new()
                        .model(ConfigurationChange::Set(String::from("large"))),
                )
                .await
        }
    });
    agent.wait_until_entered().await;
    stalled.stalls.block_writes();

    let began = tokio::time::Instant::now();
    until_closed(&stalled.session).await;
    let elapsed = began.elapsed();
    let configured = configuring
        .await
        .expect("expected the configure task to join");

    assert!(
        matches!(
            configured.as_ref().map_err(Error::cause),
            Err(Error::Timeout { .. })
        ),
        "expected the unanswered request to fail: Err(Timeout) | received: {configured:?}"
    );
    // The request was queued as `configure` began, which is the same virtual instant the held
    // agent saw it: the clock does not move until every task is idle.
    assert_reaches(
        "the session to be Closed after an unanswered request, less its turn wait \
         (Deadlines::request_timeout_to_closed less shutdown_timeout)",
        elapsed,
        stalled.deadlines.request_timeout_to_closed() - limits.shutdown_timeout,
        SLACK,
    );
}
