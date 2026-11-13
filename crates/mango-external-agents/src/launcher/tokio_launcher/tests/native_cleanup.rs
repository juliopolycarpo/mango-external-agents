//! Linux qualification of native helper cleanup through the real `TokioLauncher`.
//!
//! Every child here is a synthetic, test-owned stand-in made by re-executing this test binary
//! (`native-cleanup-leader` starts one `native-cleanup-helper` in its own process group). None is
//! a vendor CLI and no frame is a vendor frame. The tests state what `docs/adopt.md`,
//! `docs/lifecycle.md` and `docs/harness-claude.md` promise about the launcher and
//! `ProcessCleanupGuard`, and observe the operating system rather than the library's own report.
//!
//! Ownership. Only the pids this test launched are ever signalled, and only while `/proc` still
//! shows the start time recorded at launch, so a recycled pid is never touched. `Guardian`
//! signals them in `Drop`, which also runs when an assertion fails, so a failing test leaves no
//! child behind. Waits are causal where the kernel offers a cause (the helper inherits the leader's
//! stdout, so the pipe reaches end of file only once both are gone) and otherwise a bounded
//! observation of `/proc` with a named diagnostic, never a fixed sleep.

use super::{
    CancelReason, Duration, LineLimits, LineStream, ProcessLauncher, TokioLauncher, fixture,
};
use crate::Limits;
use crate::process::{ByteSink, ProcessCleanupGuard, ProcessControl, StopOutcome};
use nix::errno::Errno;
use nix::sys::signal::{SigSet, Signal, kill, killpg};
use nix::unistd::Pid;
use std::sync::Arc;
use std::time::Instant;

/// The launcher's and the guard's grace: short, so a full escalation takes about a second.
const GRACE: Duration = Duration::from_millis(300);

/// The longest any single observation waits before it fails with its named diagnostic.
const OBSERVE: Duration = Duration::from_secs(10);

/// How often a `/proc` observation looks again. It is a bound on latency, not a synchronization.
const LOOK_AGAIN: Duration = Duration::from_millis(5);

/// What `/proc` says about one launched process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Member {
    /// Alive and not a zombie.
    Running,
    /// Exited and not yet reaped. It no longer runs anything.
    Zombie,
    /// No `/proc` entry with the recorded start time.
    Gone,
}

/// A process this test launched, pinned by its start time so a recycled pid cannot match.
#[derive(Clone, Copy, Debug)]
struct Identity {
    label: &'static str,
    pid: i32,
    start_ticks: u64,
    pgid: i32,
}

/// The `/proc/<pid>/stat` fields this test reads: state, process group and start time.
fn read_stat(pid: i32) -> Option<(char, i32, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is parenthesised and may contain spaces; the fields start after the last `) `.
    let (_, rest) = stat.rsplit_once(") ")?;
    let fields: Vec<&str> = rest.split(' ').collect();
    // `rest` starts at field 3 (state), so the process group is field 5 and the start time field 22.
    Some((
        fields.first()?.chars().next()?,
        fields.get(2)?.parse().ok()?,
        fields.get(19)?.parse().ok()?,
    ))
}

impl Identity {
    fn record(label: &'static str, pid: i32) -> Self {
        let (_, pgid, start_ticks) = read_stat(pid).unwrap_or_else(|| {
            panic!("(native cleanup): expected /proc/{pid} for the {label} | received: no entry")
        });
        Self {
            label,
            pid,
            start_ticks,
            pgid,
        }
    }

    fn state(&self) -> Member {
        match read_stat(self.pid) {
            Some((_, _, start)) if start != self.start_ticks => Member::Gone,
            Some(('Z' | 'X', _, _)) => Member::Zombie,
            Some(_) => Member::Running,
            None => Member::Gone,
        }
    }
}

/// The two processes one situation launched, with the diagnostics that name them.
#[derive(Clone, Copy, Debug)]
struct Pids {
    situation: &'static str,
    leader: Identity,
    helper: Identity,
}

impl Pids {
    fn both(&self) -> [Identity; 2] {
        [self.leader, self.helper]
    }

    /// Names the first member that is still running, if any.
    fn first_running(&self) -> Option<Identity> {
        self.both()
            .into_iter()
            .find(|member| member.state() == Member::Running)
    }

    /// Reports every member's state next to the situation, so a run with `--nocapture` is a receipt.
    fn observe(&self, moment: &str) {
        eprintln!(
            "native-cleanup[{}] {moment}: leader pid={} pgid={} {:?} | helper pid={} pgid={} {:?}",
            self.situation,
            self.leader.pid,
            self.leader.pgid,
            self.leader.state(),
            self.helper.pid,
            self.helper.pgid,
            self.helper.state(),
        );
    }

    fn not_running_diagnostic(&self, member: Identity, why: &str) -> String {
        format!(
            "(native cleanup: {}): expected {} pid {} exited within {OBSERVE:?} after {why} | received: {:?}",
            self.situation,
            member.label,
            member.pid,
            member.state(),
        )
    }

    /// Waits until the process group is empty, which is the point where the launcher's worker has
    /// seen it empty too, or fails naming the member that is still there.
    async fn await_group_empty(&self, why: &str) {
        let started = Instant::now();
        while !group_is_empty(self.leader.pid) {
            let member = self
                .both()
                .into_iter()
                .find(|member| member.state() != Member::Gone)
                .unwrap_or(self.leader);
            assert!(
                started.elapsed() < OBSERVE,
                "{}",
                self.not_running_diagnostic(member, why)
            );
            tokio::time::sleep(LOOK_AGAIN).await;
        }
    }

    /// The same observation for a test that has no runtime left to sleep on.
    fn await_not_running_blocking(&self, why: &str) {
        let started = Instant::now();
        while let Some(member) = self.first_running() {
            assert!(
                started.elapsed() < OBSERVE,
                "{}",
                self.not_running_diagnostic(member, why)
            );
            std::thread::sleep(LOOK_AGAIN);
        }
    }
}

/// Signals only the pids this test learned, on every path including a failed assertion.
struct Guardian {
    situation: &'static str,
    learned: Vec<Identity>,
}

impl Guardian {
    fn any_running(&self) -> bool {
        self.learned
            .iter()
            .any(|member| member.state() == Member::Running)
    }
}

impl Drop for Guardian {
    fn drop(&mut self) {
        for member in &self.learned {
            if member.state() == Member::Running {
                eprintln!(
                    "native-cleanup[{}] emergency: SIGKILL {} pid={}",
                    self.situation, member.label, member.pid
                );
                let _ = kill(Pid::from_raw(member.pid), Signal::SIGKILL);
            }
        }
        let started = Instant::now();
        while self.any_running() && started.elapsed() < OBSERVE {
            std::thread::sleep(LOOK_AGAIN);
        }
    }
}

/// One launched leader with its helper, and the handles a host would hold.
struct Launched {
    control: Arc<dyn ProcessControl>,
    stdin: Option<Box<dyn ByteSink>>,
    lines: LineStream,
    pids: Pids,
    /// Declared last so it signals only after the handles above have had their own chance to.
    _guardian: Guardian,
}

/// The host's limits for these tests: the same short grace as the launcher, and a bounded stop.
fn limits() -> Limits {
    Limits {
        kill_grace: GRACE,
        shutdown_timeout: Duration::from_secs(5),
        ..Limits::default()
    }
}

/// Blocks the polite stop signals on the launching thread and restores the mask on drop.
///
/// A blocked mask survives `exec`, so the leader and its helper start unable to take SIGINT or
/// SIGTERM, and only the launcher's final signal ends them. The helper cannot install a handler:
/// this crate forbids `unsafe`. The mask is restored before any wait, so the test thread itself
/// keeps its signals.
struct BlockedStopSignals(SigSet);

impl BlockedStopSignals {
    fn on_this_thread() -> Self {
        let original = SigSet::thread_get_mask().expect("(native cleanup): the signal mask");
        let mut blocked = original;
        blocked.add(Signal::SIGINT);
        blocked.add(Signal::SIGTERM);
        blocked
            .thread_set_mask()
            .expect("(native cleanup): expected to block SIGINT and SIGTERM before launch");
        Self(original)
    }
}

impl Drop for BlockedStopSignals {
    fn drop(&mut self) {
        self.0
            .thread_set_mask()
            .expect("(native cleanup): expected to restore the signal mask");
    }
}

/// Launches the leader, then reads until both it and its helper have reported their pids.
async fn launch(situation: &'static str) -> Launched {
    let spec = fixture("native-cleanup-leader");
    eprintln!(
        "native-cleanup[{situation}] command: {:?} MEA_LAUNCHER_FIXTURE=native-cleanup-leader",
        spec.argv
    );
    // The launcher's spawn does not yield on Unix, so the whole call runs under the mask.
    let blocked = BlockedStopSignals::on_this_thread();
    let spawned = TokioLauncher::new()
        .with_kill_grace(GRACE)
        .spawn(spec)
        .await;
    drop(blocked);
    let process = spawned.unwrap_or_else(|error| {
        panic!("(native cleanup: {situation}): expected a launched leader | received: {error}")
    });
    let mut lines = LineStream::new(process.stdout, LineLimits::default());
    // The leader is pinned from the launcher's pid before anything is read, and the helper the
    // moment its marker arrives, so a failed read still cleans up whatever it saw.
    let leader_pid = process
        .control
        .pid()
        .and_then(|pid| i32::try_from(pid).ok())
        .unwrap_or_else(|| {
            panic!("(native cleanup: {situation}): expected a leader pid | received: none")
        });
    let mut guardian = Guardian {
        situation,
        learned: vec![Identity::record("leader", leader_pid)],
    };
    let mut leader_reported = false;
    let read = tokio::time::timeout(OBSERVE, async {
        while !leader_reported || guardian.learned.len() < 2 {
            let line = lines
                .next_line()
                .await
                .expect("(native cleanup): expected a readable stdout")
                .unwrap_or_else(|| {
                    panic!(
                        "(native cleanup: {situation}): expected leader and helper markers | received: stdout closed with {:?}",
                        guardian.learned
                    )
                });
            if let Some((_, pid)) = line.split_once("MEA-NATIVE-LEADER ") {
                assert_eq!(
                    pid.trim().parse::<i32>().ok(),
                    Some(leader_pid),
                    "(native cleanup: {situation}): expected the leader marker to match the launcher's pid"
                );
                leader_reported = true;
            }
            if let Some((_, pid)) = line.split_once("MEA-NATIVE-HELPER ")
                && let Ok(pid) = pid.trim().parse::<i32>()
            {
                guardian.learned.push(Identity::record("helper", pid));
            }
        }
    })
    .await;
    assert!(
        read.is_ok(),
        "(native cleanup: {situation}): expected leader and helper markers within {OBSERVE:?} | received: {:?}",
        guardian.learned
    );
    let find = |label: &str| {
        *guardian
            .learned
            .iter()
            .find(|member| member.label == label)
            .unwrap_or_else(|| {
                panic!("(native cleanup: {situation}): expected a {label} marker | received: none")
            })
    };
    let pids = Pids {
        situation,
        leader: find("leader"),
        helper: find("helper"),
    };
    assert_eq!(
        (pids.leader.pgid, pids.helper.pgid),
        (pids.leader.pid, pids.leader.pid),
        "(native cleanup: {situation}): expected leader and helper in one process group led by the leader"
    );
    pids.observe("launched");
    Launched {
        control: process.control,
        stdin: process.stdin,
        lines,
        pids,
        _guardian: guardian,
    }
}

impl Launched {
    fn guard(&self) -> ProcessCleanupGuard {
        ProcessCleanupGuard::new(Arc::clone(&self.control), limits(), CancelReason::Shutdown)
    }

    /// Closes the leader's stdin, which is its release, and waits for the launcher to reap it.
    async fn release_leader(&mut self) {
        self.stdin
            .take()
            .expect("(native cleanup): the leader's stdin")
            .close()
            .await
            .expect("(native cleanup): expected the release to reach the leader");
        let status = tokio::time::timeout(OBSERVE, self.control.wait())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "(native cleanup: {}): expected the released leader reaped within {OBSERVE:?} | received: still running",
                    self.pids.situation
                )
            })
            .expect("(native cleanup): expected an exit status");
        assert!(
            status.success(),
            "(native cleanup: {}): expected leader exit code 0 | received: {status:?}",
            self.pids.situation
        );
    }

    /// Reads stdout to its end. The helper holds the other end of the pipe, so end of file means
    /// the leader and the helper have both closed it.
    async fn await_stdout_end(&mut self, why: &str) {
        let ended = tokio::time::timeout(OBSERVE, async {
            while self
                .lines
                .next_line()
                .await
                .expect("(native cleanup): expected a readable stdout")
                .is_some()
            {}
        })
        .await;
        assert!(
            ended.is_ok(),
            "(native cleanup: {}): expected stdout end within {OBSERVE:?} after {why} | received: an open pipe, helper {:?}",
            self.pids.situation,
            self.pids.helper.state(),
        );
    }
}

/// The process group is empty: no member, not even an unreaped zombie, keeps the number reserved.
fn group_is_empty(leader_pid: i32) -> bool {
    killpg(Pid::from_raw(leader_pid), None) == Err(Errno::ESRCH)
}

/// Leader exit. `docs/lifecycle.md`: a successful `wait` proves only that the child exited, and
/// contained descendants may keep serving until they exit or the host stops them. `docs/adopt.md`:
/// leader exit alone does not qualify a capture, process-tree cleanup must also succeed.
#[tokio::test]
async fn leader_exit_leaves_the_helper_running_until_cleanup_ends_the_group() {
    let mut native = launch("leader-exit").await;
    let cleanup = native.guard();

    native.release_leader().await;
    native.pids.observe("leader exited");

    assert_eq!(
        native.pids.leader.state(),
        Member::Gone,
        "(native cleanup: leader-exit): expected the reaped leader pid {} gone | received: {:?}",
        native.pids.leader.pid,
        native.pids.leader.state()
    );
    assert_eq!(
        native.pids.helper.state(),
        Member::Running,
        "(native cleanup: leader-exit): expected the helper pid {} running after leader exit | received: {:?}",
        native.pids.helper.pid,
        native.pids.helper.state()
    );
    assert!(
        !group_is_empty(native.pids.leader.pid),
        "(native cleanup: leader-exit): expected group {} non-empty while the helper runs | received: empty",
        native.pids.leader.pid
    );

    let outcome = cleanup.finish().await.unwrap_or_else(|error| {
        panic!("(native cleanup: leader-exit): expected a stopped tree | received: {error}")
    });
    assert_eq!(
        outcome,
        StopOutcome::Terminated,
        "(native cleanup: leader-exit): expected Terminated for an exited leader | received: {outcome:?}"
    );
    native.pids.observe("finish returned");
    assert!(
        group_is_empty(native.pids.leader.pid),
        "(native cleanup: leader-exit): expected group {} empty when finish returned | received: a live member",
        native.pids.leader.pid
    );
    assert_eq!(native.pids.helper.state(), Member::Gone);
    native.await_stdout_end("finish").await;
}

/// Explicit joined cleanup. `ProcessCleanupGuard::finish` documents bounded cleanup the caller
/// awaits, and `docs/lifecycle.md` says a successful stop observed the containment group empty:
/// when it returns, nothing is left to wait for.
#[tokio::test]
async fn joined_finish_returns_only_with_the_whole_group_gone() {
    let mut native = launch("joined-finish").await;

    let outcome = native.guard().finish().await.unwrap_or_else(|error| {
        panic!("(native cleanup: joined-finish): expected a stopped tree | received: {error}")
    });
    native.pids.observe("finish returned");

    // The leader honours SIGINT and the helper does not, so the stop is a delivered interrupt
    // followed by the group escalation.
    assert_eq!(
        outcome,
        StopOutcome::Interrupted,
        "(native cleanup: joined-finish): expected Interrupted for a leader that honours SIGINT | received: {outcome:?}"
    );
    assert!(
        group_is_empty(native.pids.leader.pid),
        "(native cleanup: joined-finish): expected group {} empty at return | received: a live member",
        native.pids.leader.pid
    );
    assert_eq!(
        (native.pids.leader.state(), native.pids.helper.state()),
        (Member::Gone, Member::Gone),
        "(native cleanup: joined-finish): expected leader and helper gone at return"
    );
    // The leader's status is already settled, so this does not wait.
    let status = tokio::time::timeout(Duration::from_millis(100), native.control.wait())
        .await
        .expect("(native cleanup: joined-finish): expected a settled exit status at return")
        .expect("(native cleanup: joined-finish): expected an exit status");
    assert!(
        !status.success(),
        "(native cleanup: joined-finish): expected a stopped leader not to report success | received: {status:?}"
    );
    native.await_stdout_end("finish").await;
}

/// Cancellation with the runtime alive. `ProcessCleanupGuard::finish` starts its worker before it
/// awaits, so dropping the awaiting future detaches a worker that still ends the group.
#[tokio::test]
async fn cancelled_finish_keeps_the_started_worker_running_while_the_runtime_lives() {
    let mut native = launch("cancelled-finish").await;
    let mut finish = Box::pin(native.guard().finish());
    std::future::poll_fn(|cx| {
        assert!(
            finish.as_mut().poll(cx).is_pending(),
            "(native cleanup: cancelled-finish): expected finish pending on its first poll | received: ready"
        );
        std::task::Poll::Ready(())
    })
    .await;
    drop(finish);
    native.pids.observe("finish cancelled");

    native.await_stdout_end("a cancelled finish").await;
    native.pids.await_group_empty("a cancelled finish").await;
    native.pids.observe("worker finished");
    assert_eq!(native.pids.helper.state(), Member::Gone);
}

/// Cancellation by dropping the owner. `docs/adopt.md` says dropping a capture future starts the
/// bounded cleanup worker while its runtime remains alive; the guard's `Drop` is that start.
#[tokio::test]
async fn a_dropped_guard_starts_cleanup_while_the_runtime_lives() {
    let mut native = launch("dropped-guard").await;
    drop(native.guard());
    native.pids.observe("guard dropped");

    native.await_stdout_end("a dropped guard").await;
    native.pids.await_group_empty("a dropped guard").await;
    native.pids.observe("worker finished");
    assert_eq!(native.pids.helper.state(), Member::Gone);
}

/// A runtime of the kind a host builds for a short-lived capture.
fn capture_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("(native cleanup): expected a test runtime")
}

/// Early runtime shutdown, handles released afterwards. `docs/lifecycle.md` promises no cleanup
/// after the host runtime ends; `docs/harness-claude.md` says the control's own `Drop` then sends
/// SIGKILL to the process group on Unix. Releasing the last handle outside any runtime is that case.
///
/// Dropping stdin releases the leader by itself, so only the helper's death proves the group
/// signal. The leader stays an unreaped zombie: its reaper task went with the runtime.
#[test]
fn handles_released_after_runtime_shutdown_end_the_group_through_the_controls_drop() {
    let runtime = capture_runtime();
    let native = runtime.block_on(launch("shutdown-then-release"));
    let guard = runtime.block_on(async { native.guard() });
    let Launched {
        control,
        stdin,
        lines,
        pids,
        _guardian,
    } = native;

    drop(runtime);
    pids.observe("runtime shut down, handles held");

    drop(guard);
    drop(stdin);
    drop(lines);
    drop(control);
    pids.await_not_running_blocking("the last handle dropped outside a runtime");
    pids.observe("last handle dropped");
}
