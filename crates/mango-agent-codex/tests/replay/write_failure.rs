//! The live app-server link owns cleanup after a completed stdin failure, even without a turn.

use super::*;
use mango_external_agents::{Dispatch, Error};

/// The recorded handshake writes four frames before a live account read.
const OPEN_WRITES: usize = 4;

async fn open_with_broken_next_write(fail_kill: bool) -> teardown::WatchedSession {
    let inner = Arc::new(FakeLauncher::new());
    inner.push(
        Transcript::load("turn")
            .as_process()
            .failing_stdin_after(OPEN_WRITES, "EPIPE"),
    );
    let kill_gate = FakeGate::closed();
    let gated = GatedLauncher::new(Arc::clone(&inner), Some(Arc::clone(&kill_gate)), None)
        .keeping_child_alive_after_stdin_close();
    let launcher = Arc::new(if fail_kill {
        gated.failing_kills()
    } else {
        gated
    });
    let cancel = mango_external_agents::CancelToken::new();
    let host = host_context(
        Arc::clone(&launcher) as Arc<dyn ProcessLauncher>,
        None,
        mango_external_agents::Limits {
            kill_grace: std::time::Duration::from_millis(10),
            shutdown_timeout: std::time::Duration::from_secs(5),
            ..replay_limits()
        },
        cancel.clone(),
        None,
    );
    let session: Arc<dyn Session> = Arc::from(
        CodexHarness::new()
            .open_session(&host, OpenSession::new("write-failure"))
            .await
            .expect("expected the recorded handshake to succeed"),
    );
    assert_eq!(
        inner.written().len(),
        OPEN_WRITES,
        "expected EPIPE to occur after the handshake"
    );
    assert_eq!(inner.live_children(), 1);
    teardown::WatchedSession {
        session,
        launcher,
        inner,
        kill_gate,
        cancel,
    }
}

async fn fail_account_read_and_wait_for_cleanup(watched: &teardown::WatchedSession) {
    let error = watched
        .session
        .refresh_account_usage()
        .await
        .expect_err("expected the completed EPIPE");
    assert!(
        matches!(error.cause(), Error::Link { message, .. } if message == "EPIPE"),
        "expected the original typed transport failure, received {error:?}"
    );
    assert_eq!(error.dispatch(), Dispatch::AcceptanceUnknown);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        watched.kill_gate.wait_until_entered(),
    )
    .await
    .unwrap_or_else(|error| {
        panic!(
            "expected completed EPIPE to start child cleanup without an explicit close, received {} kills and {} live children: {error}",
            watched.launcher.kills(), watched.inner.live_children()
        )
    });
    assert_eq!(watched.launcher.stdin_closes(), 1);
    assert_eq!(watched.launcher.kills(), 1);
}

#[tokio::test]
async fn a_completed_live_account_write_failure_closes_and_reaps_once_without_a_turn() {
    let watched = open_with_broken_next_write(false).await;
    fail_account_read_and_wait_for_cleanup(&watched).await;
    teardown::release_kills(&watched.kill_gate);
    watched
        .session
        .close(CloseReason::Requested)
        .await
        .expect("expected shared cleanup to settle");
    watched
        .session
        .close(CloseReason::Shutdown)
        .await
        .expect("expected repeated close to join cleanup");
    teardown::settle().await;
    teardown::assert_one_teardown(&watched.launcher, &watched.inner);
    assert_eq!(watched.session.snapshot().status, SessionStatus::Closed);
    let error = watched
        .session
        .start_turn(TurnRequest::new("fresh", "hello"))
        .await
        .expect_err("expected the broken session to reject another turn");
    assert!(matches!(error.cause(), Error::Closed { .. }));
    assert_eq!(error.dispatch(), Dispatch::NotSubmitted);
    assert_eq!(watched.inner.written().len(), OPEN_WRITES);
}

#[tokio::test]
async fn a_completed_write_failure_retains_the_failed_cleanup_control_for_every_waiter() {
    let watched = open_with_broken_next_write(true).await;
    fail_account_read_and_wait_for_cleanup(&watched).await;
    teardown::release_kills(&watched.kill_gate);
    let first = watched
        .session
        .close(CloseReason::Requested)
        .await
        .expect_err("expected failed child cleanup");
    let second = watched
        .session
        .close(CloseReason::Shutdown)
        .await
        .expect_err("expected the same cleanup failure");
    let first_control = first
        .cleanup_control()
        .expect("expected retained cleanup ownership");
    let second_control = second
        .cleanup_control()
        .expect("expected repeated close to retain cleanup ownership");
    assert!(Arc::ptr_eq(&first_control, &second_control));
    assert_eq!(watched.launcher.stdin_closes(), 1);
    assert_eq!(watched.launcher.kills(), 1);
    assert_eq!(watched.inner.live_children(), 1);
    assert_eq!(watched.session.snapshot().status, SessionStatus::Closing);
    drop(watched.session);
    teardown::settle().await;
    assert_eq!(
        watched.launcher.kills(),
        1,
        "expected drop to preserve the existing cleanup owner"
    );
    let retry = first_control.kill(CancelReason::Shutdown).await;
    assert!(
        retry.is_err(),
        "expected the retained host control to preserve its truthful failure"
    );
    assert_eq!(watched.launcher.kills(), 2);
}
