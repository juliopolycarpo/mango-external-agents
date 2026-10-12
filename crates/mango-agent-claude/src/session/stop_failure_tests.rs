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
    session_with(tap, None)
}

/// The same, holding an MCP artifact a close is to remove.
fn session_with(tap: Arc<ControlTap>, config: Option<crate::mcp::ConfigFile>) -> ClaudeSession {
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
        config,
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

/// A host scratch mount that takes the removal and does not answer it until it is released.
struct StalledRemover {
    arrived: Arc<tokio::sync::Notify>,
    released: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl crate::mcp::DirectoryRemover for StalledRemover {
    fn remove(&self, directory: &std::path::Path) -> std::io::Result<()> {
        self.arrived.notify_one();
        let _ = self
            .released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recv_timeout(Duration::from_secs(10));
        std::fs::remove_dir_all(directory)
    }
}

/// The stop reaped the child and the close was inside the artifact removal when its runtime shut
/// down. Only the outcome is lost: the child is gone, so a waiter on another runtime is told no
/// cleanup is owed, and the session is `Closed`.
#[test]
fn a_close_lost_inside_the_removal_after_the_child_was_reaped_demands_no_cleanup() {
    let runtime = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("expected a runtime")
    };
    let opening = runtime();
    let scratch = std::env::temp_dir().join(format!("mea-lost-close-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&scratch).expect("expected a scratch root");
    let arrived = Arc::new(tokio::sync::Notify::new());
    let (release, released) = std::sync::mpsc::channel::<()>();
    let launcher =
        Arc::new(FakeClaudeCli::new().with_turn(Run::stalling::<[String; 0], String>([])));
    let session = Arc::new(opening.block_on(async {
        let file = crate::mcp::ConfigFile::write(
            &[mango_external_agents::McpServer {
                name: String::from("docs"),
                transport: mango_external_agents::McpTransport::Stdio {
                    command: String::from("docs-mcp"),
                    args: Vec::new(),
                    env: std::collections::BTreeMap::new(),
                },
            }],
            &scratch,
        )
        .await
        .expect("expected a written artifact")
        .expect("expected servers to produce one")
        .with_remover(StalledRemover {
            arrived: Arc::clone(&arrived),
            released: std::sync::Mutex::new(released),
        });
        let session = session_with(Arc::new(ControlTap::new(Arc::clone(&launcher))), Some(file));
        let _turn = session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected a turn");
        session
    }));
    opening.block_on(async {
        let session = Arc::clone(&session);
        tokio::spawn(async move {
            session
                .close(mango_external_agents::CloseReason::Requested)
                .await
        });
        arrived.notified().await;
    });
    // The child is reaped and the removal is parked on the blocking pool; the runtime goes now.
    opening.shutdown_background();

    let later = runtime().block_on(async {
        tokio::time::timeout(
            Duration::from_secs(1),
            session.close(mango_external_agents::CloseReason::Requested),
        )
        .await
    });
    drop(release);

    let outcome = later.unwrap_or_else(|_| {
        panic!("expected the close to return within 1s | received: still waiting after 1s")
    });
    let error = outcome.expect_err("expected the lost outcome to be reported");
    assert!(
        error.cleanup_control().is_none(),
        "expected no cleanup demand for a child that was reaped | received {error:?}"
    );
    assert!(
        matches!(&error, Error::Link { peer, .. } if peer == "tokio runtime"),
        "expected a bare Link naming the runtime | received {error:?}"
    );
    assert_eq!(
        session.snapshot().status,
        mango_external_agents::SessionStatus::Closed,
        "expected a session whose child was reaped to be Closed | received {:?}",
        session.snapshot().status
    );
    let _ = std::fs::remove_dir_all(&scratch);
    launcher.end_turns_for_host_cleanup();
}

/// A session with no active turn has no child, so a close whose worker never ran because the
/// opening runtime is gone has nothing left to stop: the outcome is lost, the session is `Closed`.
#[test]
fn a_close_that_never_ran_over_a_session_without_a_turn_settles_closed() {
    let runtime = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("expected a runtime")
    };
    let opening = runtime();
    let launcher = Arc::new(FakeClaudeCli::new());
    let session =
        opening.block_on(async { session_over(Arc::new(ControlTap::new(Arc::clone(&launcher)))) });
    opening.shutdown_background();

    let later = runtime().block_on(async {
        tokio::time::timeout(
            Duration::from_secs(1),
            session.close(mango_external_agents::CloseReason::Requested),
        )
        .await
    });

    let outcome = later.unwrap_or_else(|_| {
        panic!("expected the close to return within 1s | received: still waiting after 1s")
    });
    let error = outcome.expect_err("expected the lost outcome to be reported");
    assert!(
        matches!(&error, Error::Link { peer, .. } if peer == "tokio runtime"),
        "expected a bare Link naming the runtime | received {error:?}"
    );
    assert_eq!(
        session.snapshot().status,
        mango_external_agents::SessionStatus::Closed,
        "expected session status: Closed | received: {:?}",
        session.snapshot().status
    );
}

/// The same close gives up the MCP artifact it was to remove: no child is left to read the file.
#[test]
fn a_close_that_never_ran_over_a_session_without_a_turn_removes_its_artifact() {
    let runtime = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("expected a runtime")
    };
    let opening = runtime();
    let scratch = std::env::temp_dir().join(format!("mea-unrun-close-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&scratch).expect("expected a scratch root");
    let launcher = Arc::new(FakeClaudeCli::new());
    let session = opening.block_on(async {
        let file = crate::mcp::ConfigFile::write(
            &[mango_external_agents::McpServer {
                name: String::from("docs"),
                transport: mango_external_agents::McpTransport::Stdio {
                    command: String::from("docs-mcp"),
                    args: Vec::new(),
                    env: std::collections::BTreeMap::new(),
                },
            }],
            &scratch,
        )
        .await
        .expect("expected a written artifact")
        .expect("expected servers to produce one");
        session_with(Arc::new(ControlTap::new(Arc::clone(&launcher))), Some(file))
    });
    let artifacts = || {
        std::fs::read_dir(&scratch)
            .expect("expected a readable scratch root")
            .count()
    };
    assert_eq!(artifacts(), 1, "expected one artifact before the close");
    opening.shutdown_background();

    let left = runtime().block_on(async {
        let _ = session
            .close(mango_external_agents::CloseReason::Requested)
            .await;
        // The guard hands the removal to the blocking pool and cannot wait for it.
        for _ in 0..200 {
            if artifacts() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        artifacts()
    });

    assert_eq!(
        left, 0,
        "expected artifacts left in the scratch root: 0 | received: {left}"
    );
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A turn worker lost after its stop task proved the child gone, and before it published, owes no
/// cleanup either; the instant between the two cannot be hit from outside, so the guard is dropped
/// directly with the flag the stop task sets.
#[test]
fn a_turn_worker_lost_after_its_stop_proved_the_child_gone_demands_no_cleanup() {
    use super::{LostWorker, Teardown};
    use std::sync::atomic::Ordering;

    let control: Arc<dyn mango_external_agents::ProcessControl> = Arc::new(ReapedChild);
    let teardown = Arc::new(Teardown::new(true));
    teardown.proven_gone.store(true, Ordering::Release);

    drop(LostWorker::new(&teardown, Some(control)));

    let outcome = teardown
        .outcome()
        .expect("expected the dropped guard to publish an outcome | received none");
    let error = outcome.expect_err("expected the lost outcome to be an error");
    assert!(
        error.cleanup_control().is_none(),
        "expected no cleanup demand once the stop proved the child gone | received {error:?}"
    );
}

/// A control for a child that is already gone; the guard only carries it.
struct ReapedChild;

#[async_trait::async_trait]
impl mango_external_agents::ProcessControl for ReapedChild {
    fn pid(&self) -> Option<u32> {
        None
    }

    fn stderr_tail(&self) -> String {
        String::new()
    }

    async fn wait(&self) -> mango_external_agents::Result<mango_external_agents::ExitStatus> {
        Ok(mango_external_agents::ExitStatus {
            code: Some(0),
            signal: None,
        })
    }

    async fn interrupt(
        &self,
        _reason: mango_external_agents::CancelReason,
    ) -> mango_external_agents::Result<mango_external_agents::InterruptOutcome> {
        Ok(mango_external_agents::InterruptOutcome::Unsupported)
    }

    async fn kill(
        &self,
        _reason: mango_external_agents::CancelReason,
    ) -> mango_external_agents::Result<()> {
        Ok(())
    }
}
