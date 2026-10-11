//! Named fakes that stall a child's stop at the edge of every bound the host's `Limits` set.
//!
//! The fakes in `mango_external_agents::testing` end a child the moment it is killed, so no stage
//! of a stop ever runs long. These keep each stage open for one millisecond less than its limit and
//! then let it finish, which is the slowest a stage can be without failing: the stop then takes
//! the sum of its limits, less a few milliseconds, and the tests can read that sum off a paused
//! clock.
//!
//! The file depends only on the core crate, so the unit tests inside the crate include it by path
//! and share the fakes with `tests/session.rs`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mango_external_agents::testing::FakeLauncher;
use mango_external_agents::{
    ByteSink, CancelReason, ExitStatus, InterruptOutcome, LaunchSpec, Limits, ManagedProcess,
    ProcessControl, ProcessLauncher, Result,
};

/// How much shorter than its limit each stalled stage is: the slack the tests allow below a sum is
/// ten times this, which is wider than the four stages that can each lose it.
pub const EPSILON: Duration = Duration::from_millis(1);

/// What the reap at the end of a stop does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reap {
    /// Finishes one [`EPSILON`] before its limit: the stop succeeds, and whatever waits on a
    /// successful stop runs after it.
    JustInTime,
    /// Never finishes: the stop fails with `Error::Timeout` after the whole limit.
    Never,
}

/// The switches one stalled child shares with its test: the limits it stalls against, what its
/// reap does, and whether the writes to its input still land.
#[derive(Clone)]
pub struct Stalls {
    kill_grace: Duration,
    shutdown_timeout: Duration,
    reap: Reap,
    writes_blocked: Arc<AtomicBool>,
}

impl Stalls {
    /// Stalls every stage of a stop against `limits`.
    pub fn against(limits: &Limits, reap: Reap) -> Self {
        Self {
            kill_grace: limits.kill_grace,
            shutdown_timeout: limits.shutdown_timeout,
            reap,
            writes_blocked: Arc::new(AtomicBool::new(false)),
        }
    }

    /// From now on a write to, or a close of, the child's input never completes: the agent has
    /// stopped reading.
    pub fn block_writes(&self) {
        self.writes_blocked.store(true, Ordering::Release);
    }

    /// The process with its input and its control replaced by the stalled ones.
    pub fn stall(&self, process: ManagedProcess) -> ManagedProcess {
        ManagedProcess {
            stdin: process.stdin.map(|inner| {
                Box::new(BlockableSink {
                    inner,
                    blocked: Arc::clone(&self.writes_blocked),
                }) as Box<dyn ByteSink>
            }),
            control: Arc::new(StallingControl {
                inner: process.control,
                stalls: self.clone(),
                interrupted: AtomicBool::new(false),
                killed: AtomicBool::new(false),
            }),
            ..process
        }
    }

    /// The control alone, over `inner`, for a test that drives the stop without a connection.
    pub fn control_over(&self, inner: Arc<dyn ProcessControl>) -> Arc<dyn ProcessControl> {
        Arc::new(StallingControl {
            inner,
            stalls: self.clone(),
            interrupted: AtomicBool::new(false),
            killed: AtomicBool::new(false),
        })
    }

    /// How long the interrupt takes to report it was delivered.
    fn interrupt_stall(&self) -> Duration {
        self.kill_grace.saturating_sub(EPSILON)
    }

    /// How long the kill takes to report the tree was ended.
    fn kill_stall(&self) -> Duration {
        self.shutdown_timeout.saturating_sub(EPSILON)
    }
}

/// A child's input that stops taking writes once the test says the agent stopped reading.
struct BlockableSink {
    inner: Box<dyn ByteSink>,
    blocked: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl ByteSink for BlockableSink {
    async fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        if self.blocked.load(Ordering::Acquire) {
            return std::future::pending().await;
        }
        self.inner.write_all(bytes).await
    }

    async fn close(&mut self) -> Result<()> {
        if self.blocked.load(Ordering::Acquire) {
            return std::future::pending().await;
        }
        self.inner.close().await
    }
}

/// A control for a child that ignores the graceful interrupt and is slow to be ended.
///
/// The interrupt is delivered one [`EPSILON`] before `kill_grace` and the child does not exit; the
/// kill reports success one [`EPSILON`] before `shutdown_timeout`; and the reap after it does what
/// [`Reap`] says. `wait` before the kill never returns, which is what makes the stop spend the
/// whole grace on the exit it asked for.
struct StallingControl {
    inner: Arc<dyn ProcessControl>,
    stalls: Stalls,
    interrupted: AtomicBool,
    killed: AtomicBool,
}

#[async_trait::async_trait]
impl ProcessControl for StallingControl {
    fn pid(&self) -> Option<u32> {
        self.inner.pid()
    }

    fn stderr_tail(&self) -> String {
        self.inner.stderr_tail()
    }

    async fn wait(&self) -> Result<ExitStatus> {
        let reaped = self.killed.load(Ordering::Acquire);
        if !reaped || self.stalls.reap == Reap::Never {
            return std::future::pending().await;
        }
        tokio::time::sleep(self.stalls.shutdown_timeout.saturating_sub(EPSILON)).await;
        self.inner.wait().await
    }

    async fn interrupt(&self, _reason: CancelReason) -> Result<InterruptOutcome> {
        self.interrupted.store(true, Ordering::Release);
        tokio::time::sleep(self.stalls.interrupt_stall()).await;
        Ok(InterruptOutcome::Delivered)
    }

    async fn kill(&self, reason: CancelReason) -> Result<()> {
        tokio::time::sleep(self.stalls.kill_stall()).await;
        self.inner.kill(reason).await?;
        self.killed.store(true, Ordering::Release);
        Ok(())
    }
}

/// A launcher whose children are stalled by `stalls`.
#[derive(Clone)]
pub struct StallingLauncher {
    inner: FakeLauncher,
    stalls: Stalls,
}

impl StallingLauncher {
    /// Hands out the children queued on `inner`, stalled.
    pub fn over(inner: FakeLauncher, stalls: Stalls) -> Self {
        Self { inner, stalls }
    }
}

#[async_trait::async_trait]
impl ProcessLauncher for StallingLauncher {
    async fn spawn(&self, spec: LaunchSpec) -> Result<ManagedProcess> {
        Ok(self.stalls.stall(self.inner.spawn(spec).await?))
    }
}

/// Asserts `elapsed` is within `slack` below `ceiling`, and not above it: the shape of every driven
/// bound, which is reached to within the stalls' own milliseconds and never passed.
///
/// `what` names the operation, so the failure reads `expected X: A..=B | received: C`.
pub fn assert_reaches(what: &str, elapsed: Duration, ceiling: Duration, slack: Duration) {
    let floor = ceiling.saturating_sub(slack);
    assert!(
        (floor..=ceiling).contains(&elapsed),
        "expected {what} to take {floor:?}..={ceiling:?} | received: {elapsed:?}"
    );
}
