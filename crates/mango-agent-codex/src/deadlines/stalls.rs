//! Named fakes for the tests that measure the bounds in [`super`] against the code that enforces
//! them.

use std::sync::Arc;
use std::time::Duration;

use mango_external_agents::process::InterruptOutcome;
use mango_external_agents::{CancelReason, ExitStatus, Limits, ProcessControl, Result};

/// How far under a limit a stalled stage stops, so the stage returns instead of timing out.
///
/// Core's stop skips the exit wait after an interrupt that timed out and skips the reap after a
/// kill that timed out. Spending both waits takes an interrupt and a kill that each answer just
/// inside their limit.
pub(crate) const JUST_UNDER: Duration = Duration::from_millis(1);

/// A child's control that spends the whole of every stop stage and never exits.
///
/// `interrupt` answers `Delivered` a moment before `kill_grace`, `wait` never answers, and `kill`
/// answers `Ok` a moment before `shutdown_timeout`. A stop against it spends both graceful
/// stages, then both shutdown stages, and ends with the reap timing out: the sum of the four
/// limits less two [`JUST_UNDER`].
pub(crate) struct StallingControl {
    interrupt_takes: Duration,
    kill_takes: Duration,
}

impl StallingControl {
    /// A control stalled to the limits of a host that runs under `limits`.
    pub(crate) fn under(limits: &Limits) -> Arc<Self> {
        Arc::new(Self {
            interrupt_takes: limits.kill_grace.saturating_sub(JUST_UNDER),
            kill_takes: limits.shutdown_timeout.saturating_sub(JUST_UNDER),
        })
    }
}

#[async_trait::async_trait]
impl ProcessControl for StallingControl {
    fn pid(&self) -> Option<u32> {
        None
    }

    fn stderr_tail(&self) -> String {
        String::new()
    }

    async fn wait(&self) -> Result<ExitStatus> {
        std::future::pending().await
    }

    async fn interrupt(&self, _reason: CancelReason) -> Result<InterruptOutcome> {
        tokio::time::sleep(self.interrupt_takes).await;
        Ok(InterruptOutcome::Delivered)
    }

    async fn kill(&self, _reason: CancelReason) -> Result<()> {
        tokio::time::sleep(self.kill_takes).await;
        Ok(())
    }
}
