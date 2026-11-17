//! How long a teardown nobody asked for may take, and what it may leave behind.

use super::budgets::{StallingLauncher, stalling_host};
use super::*;

/// How many tasks are alive on the runtime this test runs on.
fn alive_tasks() -> usize {
    tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks()
}

/// The longest a connection's teardown may take under `limits`: the close of the connection, an
/// interrupt and the wait it is given, then the kill and the reap.
fn teardown_bound(limits: &Limits) -> Duration {
    limits.shutdown_timeout + 2 * limits.kill_grace + 2 * limits.shutdown_timeout
}

/// A host that keeps a turn's stream and drops the session's last handle has nobody left to
/// close it. The drop is the close: the turn ends as a shutdown on the stream the host kept, the
/// child is reaped inside the teardown bound, and every task the session started is gone.
///
/// The agent here has stopped reading, with the prompt stuck half-way into its stdin, so the
/// teardown cannot end the agent by closing its input and has to go the whole way to the kill.
#[tokio::test(start_paused = true)]
async fn a_session_dropped_mid_turn_is_torn_down_within_its_bound_and_leaves_no_task() {
    let before = alive_tasks();
    let inner = FakeLauncher::new();
    inner.push(FakeAcpAgent::new().process());
    let launcher = StallingLauncher::new(inner.clone(), "\"session/new\"");
    let limits = Limits::default();
    let session = AcpHarness::new(profile())
        .open_session(
            &stalling_host(&launcher, limits),
            OpenSession::new("chat-1"),
        )
        .await
        .expect("expected a session");
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "never read"))
        .await
        .expect("expected a turn");
    let state = session.state().clone();
    let mut lifecycle = state.subscribe();

    let begun = tokio::time::Instant::now();
    drop(session);

    assert_events(
        "the turn the host kept",
        &drain_within(&mut turn, teardown_bound(&limits)).await,
        &["TurnStarted", "Cancelled(Shutdown)", "Completed"],
    );
    let status = status_once_settled(&mut lifecycle).await;
    let waited = begun.elapsed();
    assert_status("after the drop", status, SessionStatus::Closed);
    assert_no_live_children("after the drop", &inner);
    assert!(
        waited <= teardown_bound(&limits),
        "expected the teardown within {:?} | received: {waited:?}",
        teardown_bound(&limits)
    );

    drop(turn);
    drop(lifecycle);
    eventually("every task the session started to end", &inner, || {
        (alive_tasks() == before).then_some(())
    })
    .await;
    let received = alive_tasks();
    assert_eq!(
        received, before,
        "expected alive tasks after the drop: {before} | received: {received}"
    );
}
