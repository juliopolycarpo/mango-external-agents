//! The stop a dropped session starts, which no public call can observe, and the error a stop task
//! that never answered is reported as.
//!
//! Every other path of the `CleanupRequired` contract is driven through the `Session` API from
//! `tests/cleanup_required.rs`. A drop cannot be: the stop runs detached and its outcome goes to
//! the teardown the session retains, so this reads it through a probe that outlives the session.

use std::sync::Arc;
use std::time::Duration;

use mango_external_agents::testing::FrozenClock;
use mango_external_agents::{
    Error, ExecutablePath, HarnessIdentity, Limits, Session, SessionId, SessionIds,
    SessionSnapshot, SessionState, TransportKind, TransportSelection, TurnRequest,
};
use support::{ControlTap, FakeClaudeCli, Run, SlowStop, host_under};

use super::ClaudeSession;
use crate::test_support as support;

/// A session over the tapped fake, built the way the harness builds one.
fn session_over(tap: Arc<ControlTap>) -> ClaudeSession {
    let host = host_under(tap, Limits::default());
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
    ClaudeSession::new(
        host,
        ExecutablePath::default(),
        state,
        crate::permissions::ModeAvailability::default(),
        None,
        false,
        None,
    )
}

#[tokio::test(start_paused = true)]
async fn a_dropped_session_keeps_cleanup_required_for_the_stop_it_started() {
    let launcher = Arc::new(
        FakeClaudeCli::new()
            .with_turn(Run::stalling::<[String; 0], String>([]))
            .with_slow_stop(SlowStop {
                interrupt: None,
                kill: Duration::from_secs(60 * 60),
                kill_ends_child: false,
            }),
    );
    let tap = Arc::new(ControlTap::new(Arc::clone(&launcher)));
    let session = session_over(Arc::clone(&tap));
    let _turn = session
        .start_turn(TurnRequest::new("turn-1", "hold"))
        .await
        .expect("expected a turn");
    let retained = session.retained_stop();
    assert!(
        retained.outcome().is_none(),
        "expected no stop outcome before the session was dropped | received {:?}",
        retained.outcome()
    );

    drop(session);
    tokio::time::sleep(Duration::from_secs(60)).await;

    let outcome = retained
        .outcome()
        .expect("expected the dropped session's stop to have an outcome | received none");
    let error = outcome.expect_err("expected a kill nobody answered to fail the dropped stop");
    assert!(
        matches!(error, Error::CleanupRequired { .. }),
        "expected the dropped session's stop to end as Error::CleanupRequired | received {error:?}"
    );
    let carried = error.cleanup_control().unwrap_or_else(|| {
        panic!(
            "expected the dropped session's stop to carry the host's control | received {error:?}"
        )
    });
    let issued = tap
        .newest_turn_control()
        .expect("expected the launcher to have handed out a turn control");
    assert!(
        Arc::ptr_eq(&carried, &issued),
        "expected the control the host's launcher returned | received a different control"
    );
    assert!(
        matches!(
            error.cause(),
            Error::Timeout { operation, after }
                if operation == "process-tree termination"
                    && *after == Limits::default().shutdown_timeout
        ),
        "expected the cause Error::Timeout naming process-tree termination | received {error:?}"
    );
    assert_eq!(
        launcher.kill_requests(),
        1,
        "expected one kill request for the dropped session | received {}",
        launcher.kill_requests()
    );
    launcher.end_turns_for_host_cleanup();
}

/// A stop task the runtime lost, by a panic or by an abort, says which in a typed link error.
#[tokio::test]
async fn a_stop_task_that_never_answered_names_how_it_was_lost() {
    let panicked = tokio::spawn(async { panic!("the host's control panicked") });
    let panic_error = panicked
        .await
        .expect_err("expected the panicking task to fail its join");
    let aborted = tokio::spawn(std::future::pending::<()>());
    aborted.abort();
    let abort_error = aborted
        .await
        .expect_err("expected the aborted task to fail its join");

    for (joined, shape) in [(panic_error, "panicked"), (abort_error, "was cancelled")] {
        let lost = super::stop_task_lost(&joined);
        assert!(
            matches!(&lost, Error::Link { peer, message }
                if peer == "the host's process control"
                    && message.ends_with(&format!("received: it {shape}"))),
            "expected a Link error from the host's process control ending in `received: it {shape}` | received {lost:?}"
        );
    }
}
