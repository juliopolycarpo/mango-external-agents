//! A stop that did not prove the child gone reaches the host as `Error::CleanupRequired`, with the
//! host's own `ProcessControl` and the stage's failure as its cause, on every path that can stop a
//! Claude child. The Codex and ACP harnesses answer the same shape, so a host reconciles a leftover
//! child with one rule.
//!
//! Every test drives the public `Session` API over a fake launcher whose control stalls or refuses.
//! The carried control is compared by pointer with the one the launcher handed the harness, so a
//! control rebuilt from the same child would fail here. The clock is paused, so a stop that spends
//! its whole bound costs nothing.

mod support;

use std::sync::Arc;
use std::time::Duration;

use mango_agent_claude::ClaudeHarness;
use mango_external_agents::{
    CancelReason, CloseReason, Error, EventKind, Harness, HostContext, Limits, OpenSession,
    Session, SessionStatus, TurnRequest, TurnStream,
};
use support::{ControlTap, FakeClaudeCli, REFUSED_KILL, Run, SlowStop, host_under};

const NEVER: Duration = Duration::from_secs(60 * 60);
const MILLISECOND: Duration = Duration::from_millis(1);

fn stalled() -> Run {
    Run::stalling::<[String; 0], String>([])
}

/// A kill the host's launcher never answers: the stop gives up after one `shutdown_timeout`.
fn unanswered_kill() -> SlowStop {
    SlowStop {
        interrupt: None,
        kill: NEVER,
        kill_ends_child: false,
    }
}

/// A child that takes the interrupt and the kill and exits for neither: the reap runs out.
fn ignoring_both(limits: &Limits) -> SlowStop {
    SlowStop {
        interrupt: Some(limits.kill_grace - MILLISECOND),
        kill: limits.shutdown_timeout - MILLISECOND,
        kill_ends_child: false,
    }
}

/// A session over a launcher the test can read the host's control back from.
struct Held {
    session: Arc<dyn Session>,
    host: HostContext,
    launcher: Arc<FakeClaudeCli>,
    tap: Arc<ControlTap>,
    limits: Limits,
}

async fn hold(launcher: FakeClaudeCli) -> Held {
    let launcher = Arc::new(launcher);
    open_over(Arc::clone(&launcher), ControlTap::new).await
}

async fn hold_panicking_kill(launcher: FakeClaudeCli) -> Held {
    let launcher = Arc::new(launcher);
    open_over(Arc::clone(&launcher), |launcher| {
        ControlTap::panicking_kill(launcher)
    })
    .await
}

async fn hold_refusing_kill(launcher: FakeClaudeCli) -> Held {
    let launcher = Arc::new(launcher);
    open_over(Arc::clone(&launcher), |launcher| {
        ControlTap::refusing_kill(launcher)
    })
    .await
}

async fn open_over(
    launcher: Arc<FakeClaudeCli>,
    tap: impl FnOnce(Arc<FakeClaudeCli>) -> ControlTap,
) -> Held {
    let limits = Limits::default();
    let tap = Arc::new(tap(Arc::clone(&launcher)));
    let host = host_under(tap.clone(), limits);
    let session: Arc<dyn Session> = Arc::from(
        ClaudeHarness::new()
            .open_session(&host, OpenSession::new("chat-1"))
            .await
            .expect("expected a session"),
    );
    Held {
        session,
        host,
        launcher,
        tap,
        limits,
    }
}

impl Held {
    async fn start(&self) -> TurnStream {
        self.session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected a turn")
    }

    /// The host's end of the bargain: it ends the child itself, after reading its control back.
    fn reconcile(&self) {
        self.launcher.end_turns_for_host_cleanup();
    }
}

/// A turn's events through its terminal.
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

/// The error is `Error::CleanupRequired` and its control is the one the host's launcher returned.
fn assert_cleanup_required(path: &str, error: &Error, held: &Held) {
    assert!(
        matches!(error, Error::CleanupRequired { .. }),
        "expected {path} to return Error::CleanupRequired | received {error:?}"
    );
    assert_carries_host_control(path, error, held);
}

/// The control inside the error, even under a dispatch annotation, is the host's own.
fn assert_carries_host_control(path: &str, error: &Error, held: &Held) {
    let carried = error.cleanup_control().unwrap_or_else(|| {
        panic!("expected {path} to carry the host's process control | received {error:?}")
    });
    let issued = held
        .tap
        .newest_turn_control()
        .expect("expected the launcher to have handed out a turn control");
    assert!(
        Arc::ptr_eq(&carried, &issued),
        "expected {path} to carry the control its launcher returned | received a different control (pid {:?})",
        carried.pid()
    );
}

/// The failed stage is readable through `Error::cause()`.
fn assert_timeout(path: &str, error: &Error, operation: &str, after: Duration) {
    assert!(
        matches!(
            error.cause(),
            Error::Timeout { operation: found, after: found_after }
                if found == operation && *found_after == after
        ),
        "expected {path} to have the cause Error::Timeout {{ operation: {operation:?}, after: {after:?} }} | received {error:?}"
    );
}

fn assert_refused_kill(path: &str, error: &Error) {
    assert!(
        matches!(error.cause(), Error::Closed { subject } if *subject == REFUSED_KILL),
        "expected {path} to have the host control's own error as its cause ({REFUSED_KILL}) | received {error:?}"
    );
}

mod cancel {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_kill_the_launcher_never_answers_is_cleanup_required_with_the_timeout_as_its_cause() {
        let held = hold(
            FakeClaudeCli::new()
                .with_turn(stalled())
                .with_slow_stop(unanswered_kill()),
        )
        .await;
        let _turn = held.start().await;

        let error = tokio::time::timeout(NEVER, held.session.cancel(CancelReason::Requested))
            .await
            .expect("expected the cancel to return | received none within an hour")
            .expect_err("expected a kill nobody answered to fail the cancel");

        assert_cleanup_required("cancel over an unanswered kill", &error, &held);
        assert_timeout(
            "cancel over an unanswered kill",
            &error,
            "process-tree termination",
            held.limits.shutdown_timeout,
        );
        held.reconcile();
    }

    #[tokio::test(start_paused = true)]
    async fn a_reap_that_runs_out_is_cleanup_required_with_the_timeout_as_its_cause() {
        let limits = Limits::default();
        let held = hold(
            FakeClaudeCli::new()
                .with_turn(stalled())
                .with_slow_stop(ignoring_both(&limits)),
        )
        .await;
        let _turn = held.start().await;

        let error = tokio::time::timeout(NEVER, held.session.cancel(CancelReason::Requested))
            .await
            .expect("expected the cancel to return | received none within an hour")
            .expect_err("expected a child that ignored the stop to fail the cancel");

        assert_cleanup_required("cancel over an unreaped child", &error, &held);
        assert_timeout(
            "cancel over an unreaped child",
            &error,
            "process reaping",
            limits.shutdown_timeout,
        );
        held.reconcile();
    }

    #[tokio::test(start_paused = true)]
    async fn an_error_from_the_hosts_control_is_cleanup_required_with_that_error_as_its_cause() {
        let held = hold_refusing_kill(FakeClaudeCli::new().with_turn(stalled())).await;
        let _turn = held.start().await;

        let error = tokio::time::timeout(NEVER, held.session.cancel(CancelReason::Requested))
            .await
            .expect("expected the cancel to return | received none within an hour")
            .expect_err("expected a control that refuses the kill to fail the cancel");

        assert_cleanup_required("cancel over a refused kill", &error, &held);
        assert_refused_kill("cancel over a refused kill", &error);
        held.reconcile();
    }

    /// A host control that panics must not leave the cancel waiting for an outcome that never
    /// comes: the teardown worker publishes it, with the control, for every waiter.
    #[tokio::test(start_paused = true)]
    async fn a_panic_in_the_hosts_control_is_cleanup_required_rather_than_a_hang() {
        let held = hold_panicking_kill(FakeClaudeCli::new().with_turn(stalled())).await;
        let _turn = held.start().await;

        let error = tokio::time::timeout(NEVER, held.session.cancel(CancelReason::Requested))
            .await
            .expect("expected the cancel to return | received none within an hour")
            .expect_err("expected a control that panics to fail the cancel");

        assert_cleanup_required("cancel over a panicking kill", &error, &held);
        assert!(
            matches!(error.cause(), Error::Link { message, .. } if message.contains("panicked")),
            "expected the cause to say the host's stop panicked | received {error:?}"
        );
        held.reconcile();
    }

    /// The control the launcher returned is the one the host reconciles with, even when its own
    /// control answers with a `CleanupRequired` naming another: that one stays readable as the cause.
    #[tokio::test(start_paused = true)]
    async fn a_cleanup_required_from_the_hosts_control_still_carries_the_launchers_control() {
        let launcher = Arc::new(FakeClaudeCli::new().with_turn(stalled()));
        let held = open_over(launcher, ControlTap::answering_with_its_own_cleanup).await;
        let _turn = held.start().await;

        let error = tokio::time::timeout(NEVER, held.session.cancel(CancelReason::Requested))
            .await
            .expect("expected the cancel to return | received none within an hour")
            .expect_err(
                "expected a control that answers with a cleanup of its own to fail the cancel",
            );

        assert_cleanup_required("cancel over a control with its own cleanup", &error, &held);
        let foreign = held
            .tap
            .foreign_control()
            .expect("expected the launcher to have built the control the host's answer names");
        let carried = error.cleanup_control().expect("expected a control");
        assert!(
            !Arc::ptr_eq(&carried, &foreign),
            "expected the launcher's control, not the one named inside the host's answer | received the inner one"
        );
        assert_refused_kill("cancel over a control with its own cleanup", &error);
        held.reconcile();
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_that_succeeds_is_not_wrapped() {
        let held = hold(FakeClaudeCli::new().with_turn(stalled())).await;
        let _turn = held.start().await;

        let outcome =
            tokio::time::timeout(NEVER, held.session.cancel(CancelReason::Requested)).await;

        assert!(
            matches!(outcome, Ok(Ok(()))),
            "expected a cancel whose child was reaped to return Ok | received {outcome:?}"
        );
    }

    /// A turn id the event model cannot keep is refused after the child started. The child is
    /// stopped, and a stop that fails outranks the refusal: the host needs its control.
    #[tokio::test(start_paused = true)]
    async fn a_start_refused_for_its_turn_id_reports_the_stop_that_failed() {
        let held = hold(
            FakeClaudeCli::new()
                .with_turn(stalled())
                .with_slow_stop(unanswered_kill()),
        )
        .await;

        let error = tokio::time::timeout(
            NEVER,
            held.session.start_turn(TurnRequest::new("   ", "hello")),
        )
        .await
        .expect("expected the start to return | received none within an hour")
        .expect_err("expected a turn id that cannot be kept to be refused");

        assert_carries_host_control("the refused start", &error, &held);
        assert_timeout(
            "the refused start",
            &error,
            "process-tree termination",
            held.limits.shutdown_timeout,
        );
        held.reconcile();
    }

    /// `cancel` on a pending launch has no child to stop. The `start_turn` that owns the launch
    /// stops the child when the launcher returns it, and that stop's failure reaches both the
    /// start and the next `cancel` as `CleanupRequired`.
    #[tokio::test(start_paused = true)]
    async fn a_late_child_whose_stop_fails_is_cleanup_required_for_its_start_and_the_next_cancel() {
        let launcher = FakeClaudeCli::new()
            .with_turn(stalled())
            .with_slow_stop(unanswered_kill());
        let launcher = Arc::new(launcher);
        let gate = launcher.gate_turn_spawns();
        let held = open_over(launcher, ControlTap::new).await;
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

        let pending = held.session.cancel(CancelReason::Requested).await;
        assert!(
            matches!(pending, Ok(())),
            "expected a cancel over a pending launch to return Ok at once | received {pending:?}"
        );
        gate.release();

        let started = tokio::time::timeout(NEVER, starting)
            .await
            .expect("expected the start to return once its stop ran out of time | received none within an hour")
            .expect("expected the start task to finish");
        let error = started.expect_err("expected the start to report the stop that failed");
        assert_carries_host_control("the start that owned the launch", &error, &held);
        assert_timeout(
            "the start that owned the launch",
            &error,
            "process-tree termination",
            held.limits.shutdown_timeout,
        );

        let later = held
            .session
            .cancel(CancelReason::Requested)
            .await
            .expect_err("expected the next cancel to see the kept stop failure");
        assert_cleanup_required("the next cancel", &later, &held);
        held.reconcile();
    }
}

mod close {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_turn_whose_kill_is_never_answered_is_cleanup_required_and_the_session_stays_closing()
    {
        let held = hold(
            FakeClaudeCli::new()
                .with_turn(stalled())
                .with_slow_stop(unanswered_kill()),
        )
        .await;
        let _turn = held.start().await;

        let error = tokio::time::timeout(NEVER, held.session.close(CloseReason::Requested))
            .await
            .expect("expected the close to return | received none within an hour")
            .expect_err("expected a close whose child was not stopped to fail");

        assert_cleanup_required("close with a turn active", &error, &held);
        assert_timeout(
            "close with a turn active",
            &error,
            "process-tree termination",
            held.limits.shutdown_timeout,
        );
        assert_eq!(
            held.session.snapshot().status,
            SessionStatus::Closing,
            "expected a session whose child was not stopped to stay Closing | received {:?}",
            held.session.snapshot().status
        );
        held.reconcile();
    }

    #[tokio::test(start_paused = true)]
    async fn a_reap_that_runs_out_is_cleanup_required_with_the_timeout_as_its_cause() {
        let limits = Limits::default();
        let held = hold(
            FakeClaudeCli::new()
                .with_turn(stalled())
                .with_slow_stop(ignoring_both(&limits)),
        )
        .await;
        let _turn = held.start().await;

        let error = tokio::time::timeout(NEVER, held.session.close(CloseReason::Requested))
            .await
            .expect("expected the close to return | received none within an hour")
            .expect_err("expected a close whose child was not reaped to fail");

        assert_cleanup_required("close over an unreaped child", &error, &held);
        assert_timeout(
            "close over an unreaped child",
            &error,
            "process reaping",
            limits.shutdown_timeout,
        );
        held.reconcile();
    }

    #[tokio::test(start_paused = true)]
    async fn an_error_from_the_hosts_control_is_cleanup_required_with_that_error_as_its_cause() {
        let held = hold_refusing_kill(FakeClaudeCli::new().with_turn(stalled())).await;
        let _turn = held.start().await;

        let error = tokio::time::timeout(NEVER, held.session.close(CloseReason::Requested))
            .await
            .expect("expected the close to return | received none within an hour")
            .expect_err("expected a control that refuses the kill to fail the close");

        assert_cleanup_required("close over a refused kill", &error, &held);
        assert_refused_kill("close over a refused kill", &error);
        held.reconcile();
    }

    #[tokio::test(start_paused = true)]
    async fn a_close_with_no_turn_has_no_stop_to_fail() {
        let held = hold(FakeClaudeCli::new()).await;

        let outcome = tokio::time::timeout(NEVER, held.session.close(CloseReason::Requested)).await;

        assert!(
            matches!(outcome, Ok(Ok(()))),
            "expected a close with no child to return Ok | received {outcome:?}"
        );
        let status = held.session.snapshot().status;
        assert_eq!(
            status,
            SessionStatus::Closed,
            "expected a close with no child to leave the session Closed | received {status:?}"
        );
    }

    /// The launch outlasted the close, so there is no child and no control to hand back. This is
    /// the one close failure that stays `Error::Timeout`, and it is not a stop.
    #[tokio::test(start_paused = true)]
    async fn a_pending_launch_is_still_the_launch_timeout_because_there_is_no_control_yet() {
        let launcher = Arc::new(FakeClaudeCli::new().with_turn(stalled()));
        let gate = launcher.gate_turn_spawns();
        let held = open_over(launcher, ControlTap::new).await;
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

        let error = tokio::time::timeout(NEVER, held.session.close(CloseReason::Requested))
            .await
            .expect(
                "expected the close to stop waiting for the launch | received none within an hour",
            )
            .expect_err("expected the close to report the launch it stopped waiting for");

        assert_timeout(
            "close over a pending launch",
            &error,
            "a pending Claude launch",
            held.limits.shutdown_timeout,
        );
        assert!(
            error.cleanup_control().is_none(),
            "expected no control before the launcher returned a child | received {error:?}"
        );
        gate.release();
        let started = tokio::time::timeout(NEVER, starting)
            .await
            .expect(
                "expected the start to return once the launcher did | received none within an hour",
            )
            .expect("expected the start task to finish");
        let error = started.expect_err("expected the start a close raced to be refused");
        assert!(
            matches!(
                error.cause(),
                Error::Closed { .. } | Error::Cancelled { .. }
            ),
            "expected the start to be refused as closed or cancelled once its child was stopped | received {error:?}"
        );
    }

    /// The late child arrives after the close gave up and its stop fails. The start that owns it
    /// is the caller left to hear of it, and it gets the control.
    #[tokio::test(start_paused = true)]
    async fn a_late_childs_failed_stop_reaches_its_start_as_cleanup_required() {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_turn(stalled())
                .with_slow_stop(unanswered_kill()),
        );
        let gate = launcher.gate_turn_spawns();
        let held = open_over(launcher, ControlTap::new).await;
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
        let first = tokio::time::timeout(NEVER, held.session.close(CloseReason::Requested))
            .await
            .expect(
                "expected the close to stop waiting for the launch | received none within an hour",
            )
            .expect_err("expected the close to give up on the pending launch");
        assert_timeout(
            "the close that gave up",
            &first,
            "a pending Claude launch",
            held.limits.shutdown_timeout,
        );
        gate.release();

        let started = tokio::time::timeout(NEVER, starting)
            .await
            .expect("expected the start to return once its stop ran out of time | received none within an hour")
            .expect("expected the start task to finish");
        let error = started.expect_err("expected the start to report the stop that failed");

        assert_carries_host_control("the start that owned the late child", &error, &held);
        assert_timeout(
            "the start that owned the late child",
            &error,
            "process-tree termination",
            held.limits.shutdown_timeout,
        );

        let again = held
            .session
            .close(CloseReason::Requested)
            .await
            .expect_err("expected a second close to see the late child's failed stop");
        assert_cleanup_required(
            "the close after the late child's stop failed",
            &again,
            &held,
        );
        assert_timeout(
            "the close after the late child's stop failed",
            &again,
            "process-tree termination",
            held.limits.shutdown_timeout,
        );
        held.reconcile();
    }

    /// A host that timed out its `start_turn` and drops it, then only closes, still gets the
    /// control: nothing else is left to tell it.
    #[tokio::test(start_paused = true)]
    async fn a_close_after_the_start_was_dropped_still_gets_the_late_childs_control() {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_turn(stalled())
                .with_slow_stop(unanswered_kill()),
        );
        let gate = launcher.gate_turn_spawns();
        let held = open_over(launcher, ControlTap::new).await;
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
        let first = tokio::time::timeout(NEVER, held.session.close(CloseReason::Requested))
            .await
            .expect(
                "expected the close to stop waiting for the launch | received none within an hour",
            )
            .expect_err("expected the close to give up on the pending launch");
        assert!(
            first.cleanup_control().is_none(),
            "expected no control before the launcher returned a child | received {first:?}"
        );
        starting.abort();
        let _ = starting.await;
        gate.release();
        // The dropped start stops the child it is handed on a task of its own.
        tokio::time::sleep(held.limits.shutdown_timeout * 2).await;

        let again = tokio::time::timeout(NEVER, held.session.close(CloseReason::Requested))
            .await
            .expect("expected the second close to return | received none within an hour")
            .expect_err("expected the second close to see the late child's failed stop");

        assert_cleanup_required("the close after the start was dropped", &again, &held);
        assert_timeout(
            "the close after the start was dropped",
            &again,
            "process-tree termination",
            held.limits.shutdown_timeout,
        );
        held.reconcile();
    }
}

/// The first outcome of a teardown is kept and nothing retries: `ProcessControl::kill` documents
/// that the library asks once, so a later call must not ask again, and the host that holds the
/// control is the one that reconciles.
mod kept_outcome {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_later_cancel_and_close_repeat_the_first_failure_and_ask_the_child_once() {
        let held = hold(
            FakeClaudeCli::new()
                .with_turn(stalled())
                .with_slow_stop(unanswered_kill()),
        )
        .await;
        let _turn = held.start().await;
        let first = held
            .session
            .cancel(CancelReason::Requested)
            .await
            .expect_err("expected the first cancel to fail");
        assert_cleanup_required("the first cancel", &first, &held);

        let admitted = held
            .session
            .start_turn(TurnRequest::new("turn-2", "must not overlap"))
            .await
            .expect_err("expected the unreaped child to keep the turn slot taken");
        assert!(
            matches!(admitted.cause(), Error::Busy),
            "expected Error::Busy while the child is unreaped | received {admitted:?}"
        );

        let again = held
            .session
            .cancel(CancelReason::Requested)
            .await
            .expect_err("expected a second cancel to repeat the kept failure");
        assert_cleanup_required("the second cancel", &again, &held);
        let closed = held
            .session
            .close(CloseReason::Requested)
            .await
            .expect_err("expected a close to repeat the kept failure");
        assert_cleanup_required("the close after the failed cancel", &closed, &held);
        let closed_again = held
            .session
            .close(CloseReason::Requested)
            .await
            .expect_err("expected a second close to repeat the kept failure");
        assert_cleanup_required("the second close", &closed_again, &held);
        assert_timeout(
            "the second close",
            &closed_again,
            "process-tree termination",
            held.limits.shutdown_timeout,
        );

        assert_eq!(
            held.launcher.kill_requests(),
            1,
            "expected one kill request for four calls | received {}",
            held.launcher.kill_requests()
        );
        assert_eq!(
            held.session.snapshot().status,
            SessionStatus::Closing,
            "expected a session whose child was not stopped to stay Closing | received {:?}",
            held.session.snapshot().status
        );
        held.reconcile();
        tokio::time::sleep(Duration::from_secs(1)).await;
        let after = held
            .session
            .start_turn(TurnRequest::new("turn-3", "after the host ended the child"))
            .await
            .expect_err(
                "expected a closed session to refuse a turn after the host ended the child itself",
            );
        assert!(
            matches!(after.cause(), Error::Closed { .. }),
            "expected a session that closed over an unreaped child to refuse a turn as Error::Closed | received {after:?}"
        );
    }

    /// A failed cancel leaves the session open, and the turn's slot taken for good: the host
    /// ending the child through the control it was handed does not reopen it, so the host opens a
    /// new session, as it does after a failed stop in the Codex and ACP harnesses.
    #[tokio::test(start_paused = true)]
    async fn the_slot_stays_taken_after_the_host_ended_the_child_itself() {
        let held = hold(
            FakeClaudeCli::new()
                .with_turn(stalled())
                .with_slow_stop(unanswered_kill()),
        )
        .await;
        let _turn = held.start().await;
        let error = held
            .session
            .cancel(CancelReason::Requested)
            .await
            .expect_err("expected the cancel to fail");
        assert_cleanup_required("the cancel", &error, &held);

        held.reconcile();
        tokio::time::sleep(Duration::from_secs(1)).await;

        let admitted = held
            .session
            .start_turn(TurnRequest::new("turn-2", "after the host ended the child"))
            .await
            .expect_err("expected the slot to stay taken after the host ended the child itself");
        assert!(
            matches!(admitted.cause(), Error::Busy),
            "expected Error::Busy after a failed stop, reconciled by the host | received {admitted:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_close_repeats_the_first_closes_failure_and_asks_the_control_once() {
        let held = hold_refusing_kill(FakeClaudeCli::new().with_turn(stalled())).await;
        let _turn = held.start().await;
        let first = held
            .session
            .close(CloseReason::Requested)
            .await
            .expect_err("expected the first close to fail");
        assert_cleanup_required("the first close", &first, &held);

        let second = held
            .session
            .close(CloseReason::Requested)
            .await
            .expect_err("expected the second close to repeat the failure");

        assert_cleanup_required("the second close", &second, &held);
        assert_refused_kill("the second close", &second);
        assert_eq!(
            held.tap.refused_kills(),
            1,
            "expected the control to be asked to kill once | received {}",
            held.tap.refused_kills()
        );
        held.reconcile();
    }
}

/// The pump's own stops: the stream ending, the child exiting, the idle timer, the host's token.
mod pump {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_stream_that_ended_under_a_live_child_leaves_cleanup_required_for_the_next_cancel() {
        let held = hold(
            FakeClaudeCli::new()
                .with_turn(stalled())
                .with_slow_stop(unanswered_kill()),
        )
        .await;
        let mut turn = held.start().await;
        held.launcher.turn_reading().await;
        held.launcher.close_turn_output();
        let events = terminal_of(&mut turn).await;
        assert!(
            matches!(events.last(), Some(EventKind::Error { .. })),
            "expected an error terminal for a stream that ended without a result | received {events:?}"
        );

        let error = tokio::time::timeout(NEVER, held.session.cancel(CancelReason::Requested))
            .await
            .expect("expected the cancel to return | received none within an hour")
            .expect_err("expected the cancel to see the failed reap");

        assert_cleanup_required("cancel after the stream ended", &error, &held);
        assert_timeout(
            "cancel after the stream ended",
            &error,
            "process-tree termination",
            held.limits.shutdown_timeout,
        );
        let refused = held
            .session
            .start_turn(TurnRequest::new("turn-2", "must not overlap"))
            .await
            .expect_err("expected the unreaped child to keep the turn slot taken");
        assert!(
            matches!(refused.cause(), Error::Busy),
            "expected Error::Busy while the child is unreaped | received {refused:?}"
        );
        held.reconcile();
    }

    #[tokio::test(start_paused = true)]
    async fn a_child_that_exited_leaves_cleanup_required_for_the_next_close() {
        let held = hold_refusing_kill(FakeClaudeCli::new().with_turn(stalled())).await;
        let mut turn = held.start().await;
        held.launcher.turn_reading().await;
        held.launcher.exit_turn_holding_its_output(1);
        let events = terminal_of(&mut turn).await;
        assert!(
            matches!(events.last(), Some(EventKind::Error { .. })),
            "expected an error terminal for a child that left without a result | received {events:?}"
        );

        let error = tokio::time::timeout(NEVER, held.session.close(CloseReason::Requested))
            .await
            .expect("expected the close to return | received none within an hour")
            .expect_err("expected the close to see the failed reap");

        assert_cleanup_required("close after the child exited", &error, &held);
        assert_refused_kill("close after the child exited", &error);
        held.reconcile();
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_stream_leaves_cleanup_required_for_the_next_cancel() {
        let held = hold(
            FakeClaudeCli::new()
                .with_turn(stalled())
                .with_slow_stop(unanswered_kill()),
        )
        .await;
        let mut turn = held.start().await;
        held.launcher.turn_reading().await;
        let events = terminal_of(&mut turn).await;
        assert!(
            matches!(events.last(), Some(EventKind::Error { error }) if error.code.as_str() == "claude-stream-broken"),
            "expected a claude-stream-broken terminal | received {events:?}"
        );

        let error = tokio::time::timeout(NEVER, held.session.cancel(CancelReason::Requested))
            .await
            .expect("expected the cancel to return | received none within an hour")
            .expect_err("expected the cancel to see the failed reap");

        assert_cleanup_required("cancel after the idle timeout", &error, &held);
        held.reconcile();
    }

    #[tokio::test(start_paused = true)]
    async fn the_host_shutdown_token_leaves_cleanup_required_for_the_next_close() {
        let held = hold(
            FakeClaudeCli::new()
                .with_turn(stalled())
                .with_slow_stop(unanswered_kill()),
        )
        .await;
        let mut turn = held.start().await;

        held.host.cancel().cancel();
        let events = terminal_of(&mut turn).await;
        assert!(
            events.contains(&EventKind::Cancelled {
                reason: CancelReason::Shutdown
            }),
            "expected the turn to end cancelled by the shutdown | received {events:?}"
        );

        let error = tokio::time::timeout(NEVER, held.session.close(CloseReason::Requested))
            .await
            .expect("expected the close to return | received none within an hour")
            .expect_err("expected the close to see the failed reap");

        assert_cleanup_required("close after the host shutdown", &error, &held);
        assert_timeout(
            "close after the host shutdown",
            &error,
            "process-tree termination",
            held.limits.shutdown_timeout,
        );
        held.reconcile();
    }

    #[tokio::test(start_paused = true)]
    async fn a_dropped_stream_leaves_cleanup_required_for_the_next_cancel() {
        let held = hold_refusing_kill(FakeClaudeCli::new().with_turn(stalled())).await;
        let turn = held.start().await;
        held.launcher.turn_reading().await;

        drop(turn);
        // The pump stops the child for a stream nobody reads; give it the time to try.
        tokio::time::sleep(Duration::from_secs(1)).await;

        let error = held
            .session
            .cancel(CancelReason::Requested)
            .await
            .expect_err("expected the cancel to see the failed stop of the dropped stream");
        assert_cleanup_required("cancel after the stream was dropped", &error, &held);
        assert_refused_kill("cancel after the stream was dropped", &error);
        held.reconcile();
    }
}
