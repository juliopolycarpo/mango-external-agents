//! Dropping the last handle of an ACP session that still has a turn running: the host keeps the
//! stream, so the session's own teardown has to end the turn and the child. Each test asserts the
//! terminal on the held stream, the published status and the live child count.

use super::host_cancel::{
    ChatteringAgent, ReasonRecordingLauncher, assert_children_reaped, assert_shutdown_terminal,
    cancellable_host, events_to_terminal,
};
use super::*;

/// Waits on a clone of the session state, because the handle itself is gone by then.
async fn assert_state_settles_closed(state: &mango_external_agents::SessionState) {
    let mut lifecycle = state.subscribe();
    let status = status_once_settled(&mut lifecycle).await;
    assert_eq!(
        status,
        SessionStatus::Closed,
        "expected session status: Closed | received: {status:?}"
    );
}

/// Opens a session on a silent agent and starts a turn the agent will never answer.
async fn open_running_turn(
    launcher: &FakeLauncher,
    agent: FakeProcess,
) -> (Box<dyn Session>, TurnStream) {
    launcher.push(agent);
    let session = AcpHarness::new(profile())
        .open_session(
            &cancellable_host(launcher, &CancelToken::new()),
            OpenSession::new("drop").with_configuration(permissive()),
        )
        .await
        .expect("expected a session");
    let turn = session
        .start_turn(TurnRequest::new("turn-1", "work"))
        .await
        .expect("expected a turn");
    (session, turn)
}

fn silent_agent() -> FakeProcess {
    FakeAcpAgent::new()
        .with_updates(Vec::new())
        .staying_silent()
        .process()
}

/// The host keeps the stream and drops the handle: idle expiry is 600 s here, so a terminal inside
/// the bound came from the drop.
#[tokio::test]
async fn dropping_the_session_with_a_turn_running_ends_the_turn_and_reaps_the_child() {
    let launcher = FakeLauncher::new();
    let (session, mut turn) = open_running_turn(&launcher, silent_agent()).await;
    let state = session.state().clone();

    drop(session);

    assert_shutdown_terminal(&events_to_terminal(&mut turn).await);
    assert_state_settles_closed(&state).await;
    assert_children_reaped(&launcher).await;
}

/// An agent that keeps updating never goes idle, so only the drop can end its turn.
#[tokio::test]
async fn dropping_the_session_ends_a_turn_whose_agent_keeps_updating() {
    let agent = ChatteringAgent::new();
    let launcher = FakeLauncher::new();
    let (session, mut turn) = open_running_turn(&launcher, agent.process()).await;
    let state = session.state().clone();
    let chatter = agent.start_chattering();
    tokio::time::sleep(Duration::from_millis(50)).await;

    drop(session);

    let events = events_to_terminal(&mut turn).await;
    agent.hang_up();
    let _ = chatter.await;
    assert_shutdown_terminal(&events);
    assert_state_settles_closed(&state).await;
    assert_children_reaped(&launcher).await;
}

/// The last handle can go from a thread with no runtime; the connection's own runtime runs the
/// shutdown.
#[tokio::test]
async fn dropping_the_session_off_the_runtime_ends_the_turn_and_reaps_the_child() {
    let launcher = FakeLauncher::new();
    let (session, mut turn) = open_running_turn(&launcher, silent_agent()).await;
    let state = session.state().clone();

    std::thread::spawn(move || drop(session))
        .join()
        .expect("expected the drop off the runtime not to panic");

    assert_shutdown_terminal(&events_to_terminal(&mut turn).await);
    assert_state_settles_closed(&state).await;
    assert_children_reaped(&launcher).await;
}

/// Once the turn has ended, the drop has nothing left to cancel: the child is ended by the one
/// idle-session teardown, exactly once and as an ordinary shutdown.
#[tokio::test]
async fn dropping_the_session_after_the_terminal_shuts_down_once() {
    let inner = FakeLauncher::new();
    inner.push(FakeAcpAgent::new().process());
    let launcher = ReasonRecordingLauncher::new(inner.clone());
    let host = HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .build()
        .expect("expected a host");
    let session = AcpHarness::new(profile())
        .open_session(
            &host,
            OpenSession::new("after-terminal").with_configuration(permissive()),
        )
        .await
        .expect("expected a session");
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "work"))
        .await
        .expect("expected a turn");
    let events = events_to_terminal(&mut turn).await;
    assert!(
        !events
            .iter()
            .any(|kind| matches!(kind, EventKind::Cancelled { .. })),
        "expected a turn that completed by itself | received: {events:?}"
    );
    let state = session.state().clone();

    drop(session);

    assert_state_settles_closed(&state).await;
    assert_children_reaped(&inner).await;
    assert_eq!(
        launcher.kill_reasons(),
        vec![CancelReason::Shutdown],
        "expected kill reasons: [Shutdown] | received: {:?}",
        launcher.kill_reasons()
    );
}
