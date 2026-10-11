//! The built-in launcher's stop bound, measured against real children on the real clock.
//!
//! The deadlines table in `docs/harness-claude.md` derives the launcher's stop time from the code:
//! on Unix one `kill_grace` for the exit after a delivered SIGINT, then SIGTERM and a second
//! `kill_grace`, then SIGKILL; on Windows no graceful interrupt and a Job that empties on request.
//! Each test runs `stop_process_with_limits` over a child that behaves one way. Where a stage must
//! have been waited out, the test asserts a lower bound for it, which a loaded machine cannot break
//! because a stage takes at least its grace. The upper bounds carry slack for a loaded runner and
//! only catch a gross regression: the launcher's own error, returned when its last wait expires,
//! is what enforces the ceiling, and `timed_stop` fails on it.

use super::*;
use crate::Limits;
use crate::process::{ProcessControl, StopOutcome, stop_process_with_limits};
use std::time::Instant;

/// The grace the Unix cases run under: short, so each takes about a second. The stop itself
/// fails, rather than running long, when the group outlives a SIGKILL by a whole grace, so it is
/// not made so short that a loaded runner reaping a helper could trip it.
#[cfg(unix)]
const GRACE: Duration = Duration::from_millis(500);

/// Added to every upper bound. The bound is a statement about the code and the slack is about the
/// runner: it only has to exceed the worst scheduling delay a CI machine adds to a stop.
const SLACK: Duration = Duration::from_secs(3);

/// Limits whose `kill_grace` is `grace`, with the rest at their defaults.
fn limits_with(grace: Duration) -> Limits {
    Limits {
        kill_grace: grace,
        ..Limits::default()
    }
}

/// Times one stop of `control` under `limits`: how long it took and what it reported.
///
/// Printed as well, so a run with `--no-capture` records the measurement.
async fn timed_stop(control: &dyn ProcessControl, limits: &Limits) -> (Duration, StopOutcome) {
    let begun = Instant::now();
    let outcome = stop_process_with_limits(control, CancelReason::Requested, limits)
        .await
        .expect("expected the stop to finish within its bounds");
    let elapsed = begun.elapsed();
    eprintln!(
        "stop took {elapsed:?} under kill_grace {:?}: {outcome:?}",
        limits.kill_grace
    );
    (elapsed, outcome)
}

#[cfg(unix)]
mod on_unix {
    use super::*;
    use crate::process::{ByteSink, ExitStatus, LineLimits, LineStream};
    use nix::errno::Errno;
    use nix::sys::signal::{SigSet, Signal, killpg};
    use std::sync::Arc;

    /// Whether the operating system still knows the child's process group.
    ///
    /// The group and not the leader, because the launcher signals the group. On Linux a zombie
    /// nobody reaped is still a member, so `ESRCH` also proves the reap there; elsewhere the
    /// leader's reap is proven by `status_of`.
    fn group_exists(leader: u32) -> bool {
        !matches!(
            killpg(super::super::super::group_of(leader), None),
            Err(Errno::ESRCH)
        )
    }

    /// A running fixture child, with the pipes whose closure would end it for another reason.
    struct Running {
        control: Arc<dyn ProcessControl>,
        pid: u32,
        _stdin: Option<Box<dyn ByteSink>>,
        _output: LineStream,
    }

    /// Starts `mode` with `blocked` signals blocked from its first instruction, and returns once
    /// it reports ready.
    ///
    /// Blocked rather than ignored: installing a handler or `SIG_IGN` needs `unsafe`, which this
    /// crate forbids. The mask is set on the calling thread around the spawn so the child's
    /// runtime inherits it before it creates a thread, and it is restored before this returns.
    async fn spawn_ready(mode: &str, blocked: &[Signal], grace: Duration) -> Running {
        let original = SigSet::thread_get_mask().expect("signal mask");
        let mut mask = original;
        for signal in blocked {
            mask.add(*signal);
        }
        mask.thread_set_mask().expect("block signals before spawn");
        let spawned = TokioLauncher::new()
            .with_kill_grace(grace)
            .spawn(fixture(mode))
            .await;
        original
            .thread_set_mask()
            .expect("restore the test's signal mask");
        let child = spawned.expect("expected a child");
        let pid = child.control.pid().expect("expected a process id");
        let mut output = LineStream::new(child.stdout, LineLimits::default());
        while !output
            .next_line()
            .await
            .expect("ready output")
            .expect("expected the child to reach readiness")
            .ends_with("MEA-READY")
        {}
        Running {
            control: child.control,
            pid,
            _stdin: child.stdin,
            _output: output,
        }
    }

    /// The exit status after the stop returned, which `wait` reports at once.
    async fn status_of(child: &Running) -> ExitStatus {
        tokio::time::timeout(Duration::from_secs(5), child.control.wait())
            .await
            .expect("expected the stopped child to be reaped")
            .expect("expected an exit status")
    }

    /// A child and a helper in its group that both ignore SIGINT and SIGTERM cost one grace for
    /// the exit after the interrupt and one for the SIGTERM. SIGKILL then lands at once, so the
    /// table's third `kill_grace` is the ceiling and not what this takes. The whole group is gone
    /// and reaped.
    #[tokio::test]
    async fn a_group_that_ignores_sigint_and_sigterm_is_killed_after_two_graces() {
        let child = spawn_ready(
            "ready-with-helper",
            &[Signal::SIGINT, Signal::SIGTERM],
            GRACE,
        )
        .await;
        let pid = child.pid;
        assert!(
            group_exists(pid),
            "expected process group {pid} to exist before the stop | received: none"
        );

        let (elapsed, outcome) = timed_stop(child.control.as_ref(), &limits_with(GRACE)).await;

        assert_eq!(
            outcome,
            StopOutcome::Terminated,
            "expected a child that ignores both signals to be terminated"
        );
        assert!(
            elapsed >= 2 * GRACE,
            "expected the SIGINT and SIGTERM stages both waited out (>= {:?}) | received: {elapsed:?}",
            2 * GRACE
        );
        assert!(
            elapsed <= 3 * GRACE + SLACK,
            "expected a stop within the documented three graces plus slack (<= {:?}) | received: {elapsed:?}",
            3 * GRACE + SLACK
        );
        assert_eq!(
            status_of(&child).await.signal,
            Some(9),
            "expected the leader to end by SIGKILL"
        );
        assert!(
            !group_exists(pid),
            "expected process group {pid} gone and reaped | received: a live member"
        );
    }

    /// A child that dies on SIGINT is reaped inside the first stage: the stop reports
    /// `Interrupted` and does not spend the grace. The launcher and the limits share the default
    /// two-second grace, as the table assumes, so finishing under it cannot be the machine being
    /// fast.
    #[tokio::test]
    async fn a_child_that_exits_on_sigint_is_interrupted_without_waiting_out_the_grace() {
        let limits = Limits::default();
        let child = spawn_ready("interrupt-ready", &[], limits.kill_grace).await;
        let pid = child.pid;

        let (elapsed, outcome) = timed_stop(child.control.as_ref(), &limits).await;

        assert_eq!(
            outcome,
            StopOutcome::Interrupted,
            "expected a child that exits on SIGINT to report Interrupted"
        );
        assert!(
            elapsed < limits.kill_grace,
            "expected the stop to finish inside one grace ({:?}) | received: {elapsed:?}",
            limits.kill_grace
        );
        assert_eq!(
            status_of(&child).await.signal,
            Some(2),
            "expected the child to end by SIGINT"
        );
        assert!(
            !group_exists(pid),
            "expected process group {pid} gone and reaped | received: a live member"
        );
    }

    /// A child that ignores SIGINT and dies on SIGTERM waits out the first stage and ends in the
    /// second: SIGTERM, not SIGKILL, ends it.
    #[tokio::test]
    async fn a_child_that_ignores_sigint_but_dies_on_sigterm_ends_in_the_second_stage() {
        let child = spawn_ready("interrupt-ready", &[Signal::SIGINT], GRACE).await;
        let pid = child.pid;

        let (elapsed, outcome) = timed_stop(child.control.as_ref(), &limits_with(GRACE)).await;

        assert_eq!(
            outcome,
            StopOutcome::Terminated,
            "expected a child that ignores SIGINT to be terminated"
        );
        assert!(
            elapsed >= GRACE,
            "expected the SIGINT stage waited out (>= {GRACE:?}) | received: {elapsed:?}"
        );
        assert!(
            elapsed <= 3 * GRACE + SLACK,
            "expected a stop within the documented three graces plus slack (<= {:?}) | received: {elapsed:?}",
            3 * GRACE + SLACK
        );
        assert_eq!(
            status_of(&child).await.signal,
            Some(15),
            "expected the child to end by SIGTERM, before any SIGKILL"
        );
        assert!(
            !group_exists(pid),
            "expected process group {pid} gone and reaped | received: a live member"
        );
    }
}

#[cfg(windows)]
mod on_windows {
    use super::*;

    /// The grace the Windows Job case runs under. The kill reports an error when the Job has not
    /// emptied within two of these, so it is long enough that a slow runner does not trip it.
    const GRACE: Duration = Duration::from_secs(1);

    /// There is no graceful interrupt on Windows, so a stop is only the Job's termination: it
    /// does not spend the grace on a signal nobody can send. The grace is long here so that
    /// finishing under it cannot be the machine being fast.
    #[tokio::test]
    async fn a_windows_stop_skips_the_grace_stages_and_ends_the_child() {
        let grace = Duration::from_secs(4);
        let child = TokioLauncher::new()
            .with_kill_grace(grace)
            .spawn(fixture("forever"))
            .await
            .expect("expected a child");
        let pid = child.control.pid().expect("expected a process id");
        assert_eq!(
            child
                .control
                .interrupt(CancelReason::Requested)
                .await
                .expect("interrupt"),
            crate::InterruptOutcome::Unsupported,
            "expected Windows to report no graceful interrupt"
        );

        let (elapsed, outcome) = timed_stop(child.control.as_ref(), &limits_with(grace)).await;

        assert_eq!(
            outcome,
            StopOutcome::Terminated,
            "expected a Windows child to be terminated"
        );
        assert!(
            elapsed < grace,
            "expected the stop to spend no grace stage (< {grace:?}) | received: {elapsed:?}"
        );
        await_windows_process_exit(pid).await;
    }

    /// The Job takes the leader and the helper it started, inside the documented two graces.
    #[tokio::test]
    async fn a_windows_stop_ends_the_whole_job_within_two_graces() {
        let (child, mut fixture_files) = windows_helper_child_under(GRACE).await;
        let leader = child.control.pid().expect("leader process id");
        let helper = fixture_files.await_helper().await;
        let mut stdout = LineStream::new(child.stdout, LineLimits::default());
        await_fixture_line(&mut stdout, "MEA-HELPER-READY").await;
        assert!(
            windows_process_is_running(leader) && windows_process_is_running(helper),
            "expected leader {leader} and helper {helper} to run before the stop"
        );

        let (elapsed, outcome) = timed_stop(child.control.as_ref(), &limits_with(GRACE)).await;

        assert_eq!(
            outcome,
            StopOutcome::Terminated,
            "expected a Windows Job to be terminated"
        );
        assert!(
            elapsed <= 2 * GRACE + SLACK,
            "expected a stop within the documented two graces plus slack (<= {:?}) | received: {elapsed:?}",
            2 * GRACE + SLACK
        );
        assert!(
            !windows_process_is_running(helper),
            "expected helper process {helper} gone with its Job | received: still running"
        );
        await_windows_process_exit(leader).await;
        fixture_files.clear_helper();
        fixture_files.clear_leader();
    }
}
