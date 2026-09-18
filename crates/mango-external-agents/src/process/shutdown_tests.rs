use super::*;
use crate::host::CancelToken;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

struct ControlledProcess {
    interrupt: InterruptOutcome,
    exit_on_interrupt: bool,
    exits: CancelToken,
    kills: AtomicUsize,
}

#[async_trait::async_trait]
impl ProcessControl for ControlledProcess {
    fn pid(&self) -> Option<u32> {
        None
    }
    fn stderr_tail(&self) -> String {
        String::new()
    }
    async fn wait(&self) -> Result<ExitStatus> {
        self.exits.cancelled().await;
        Ok(ExitStatus::default())
    }
    async fn interrupt(&self, _: CancelReason) -> Result<InterruptOutcome> {
        if self.exit_on_interrupt {
            self.exits.cancel();
        }
        Ok(self.interrupt)
    }
    async fn kill(&self, _: CancelReason) -> Result<()> {
        self.kills.fetch_add(1, Ordering::AcqRel);
        self.exits.cancel();
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn graceful_leader_exit_still_cleans_up_its_process_tree() {
    let process = ControlledProcess {
        interrupt: InterruptOutcome::Delivered,
        exit_on_interrupt: true,
        exits: CancelToken::new(),
        kills: AtomicUsize::new(0),
    };
    assert_eq!(
        stop_process(&process, CancelReason::Requested, Duration::from_secs(3))
            .await
            .expect("stop"),
        StopOutcome::Interrupted
    );
    assert_eq!(process.kills.load(Ordering::Acquire), 1);
}

#[tokio::test(start_paused = true)]
async fn unacknowledged_interrupt_escalates_after_the_host_grace() {
    let process = ControlledProcess {
        interrupt: InterruptOutcome::Delivered,
        exit_on_interrupt: false,
        exits: CancelToken::new(),
        kills: AtomicUsize::new(0),
    };
    let start = tokio::time::Instant::now();
    assert_eq!(
        stop_process(&process, CancelReason::Requested, Duration::from_secs(3))
            .await
            .expect("stop"),
        StopOutcome::Terminated
    );
    assert_eq!(start.elapsed(), Duration::from_secs(3));
    assert_eq!(process.kills.load(Ordering::Acquire), 1);
}

#[tokio::test(start_paused = true)]
async fn unsupported_interrupt_uses_explicit_termination() {
    let process = ControlledProcess {
        interrupt: InterruptOutcome::Unsupported,
        exit_on_interrupt: false,
        exits: CancelToken::new(),
        kills: AtomicUsize::new(0),
    };
    let start = tokio::time::Instant::now();
    assert_eq!(
        stop_process(&process, CancelReason::Shutdown, Duration::from_secs(3))
            .await
            .expect("stop"),
        StopOutcome::Terminated
    );
    assert_eq!(start.elapsed(), Duration::ZERO);
    assert_eq!(process.kills.load(Ordering::Acquire), 1);
}

#[tokio::test(start_paused = true)]
async fn host_limits_supply_the_interrupt_deadline() {
    let process = ControlledProcess {
        interrupt: InterruptOutcome::Delivered,
        exit_on_interrupt: false,
        exits: CancelToken::new(),
        kills: AtomicUsize::new(0),
    };
    let limits = crate::Limits {
        kill_grace: Duration::from_secs(7),
        shutdown_timeout: Duration::from_secs(13),
        ..crate::Limits::default()
    };
    let start = tokio::time::Instant::now();
    assert_eq!(
        stop_process_with_limits(&process, CancelReason::Shutdown, &limits)
            .await
            .expect("stop"),
        StopOutcome::Terminated
    );
    assert_eq!(start.elapsed(), limits.kill_grace);
}
