//! One deadline for broker deliberation and host approval, measured by the runtime clock.

use std::future::Future;
use std::time::SystemTime;

/// A permission request's wall-clock deadline translated once to the runtime's monotonic clock.
///
/// Reuse this value through every stage of an approval so waiting on a broker or a full event
/// channel does not restart its timeout. The host's clock supplies `now`; runtime timers supply
/// elapsed time, including when a test pauses time.
#[derive(Clone, Copy, Debug)]
pub struct ApprovalDeadline(tokio::time::Instant);

impl ApprovalDeadline {
    /// Starts the timer with the time remaining on the request.
    ///
    /// ```
    /// use mango_external_agents::approval::ApprovalDeadline;
    /// let now = std::time::SystemTime::now();
    /// let deadline = ApprovalDeadline::new(now, now);
    /// assert!(deadline.is_elapsed());
    /// ```
    #[must_use]
    pub fn new(expires_at: SystemTime, now: SystemTime) -> Self {
        Self(tokio::time::Instant::now() + expires_at.duration_since(now).unwrap_or_default())
    }

    /// Checks the deadline before accepting a decision, even if the timer task has not run yet.
    ///
    /// See [`Self::new`] for an example of an already elapsed deadline.
    #[must_use]
    pub fn is_elapsed(self) -> bool {
        tokio::time::Instant::now() >= self.0
    }

    /// Waits for this deadline without restarting it.
    ///
    /// ```
    /// # async fn example() {
    /// use mango_external_agents::approval::ApprovalDeadline;
    /// let now = std::time::SystemTime::now();
    /// ApprovalDeadline::new(now, now).wait().await;
    /// # }
    /// ```
    pub async fn wait(self) {
        tokio::time::sleep_until(self.0).await;
    }

    /// Runs broker deliberation within the same deadline as the later host response.
    ///
    /// Returns `None` once expired, including when a ready decision races the deadline.
    ///
    /// ```
    /// # async fn example() {
    /// use mango_external_agents::approval::ApprovalDeadline;
    /// let now = std::time::SystemTime::now();
    /// assert_eq!(ApprovalDeadline::new(now, now).run(async { "allow" }).await, None);
    /// # }
    /// ```
    pub async fn run<T>(self, future: impl Future<Output = T>) -> Option<T> {
        if self.is_elapsed() {
            return None;
        }
        tokio::select! {
            biased;
            () = self.wait() => None,
            result = future => (!self.is_elapsed()).then_some(result),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ApprovalDeadline;
    use std::time::{Duration, SystemTime};

    #[tokio::test(start_paused = true)]
    async fn elapsed_deadlines_never_accept_ready_decisions() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
        for expires in [now, now - Duration::from_secs(1)] {
            let deadline = ApprovalDeadline::new(expires, now);
            assert!(deadline.is_elapsed());
            assert_eq!(deadline.run(async { "allow" }).await, None);
            deadline.wait().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn broker_wait_does_not_restart_the_host_deadline() {
        let now = SystemTime::UNIX_EPOCH;
        let deadline = ApprovalDeadline::new(now + Duration::from_secs(10), now);
        assert_eq!(deadline.run(async { "ask host" }).await, Some("ask host"));
        tokio::time::advance(Duration::from_secs(7)).await;
        let start = tokio::time::Instant::now();
        deadline.wait().await;
        assert_eq!(start.elapsed(), Duration::from_secs(3));
        assert!(deadline.is_elapsed());
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_broker_cannot_outlive_the_deadline() {
        let now = SystemTime::UNIX_EPOCH;
        let deadline = ApprovalDeadline::new(now + Duration::from_secs(10), now);
        assert_eq!(deadline.run(std::future::pending::<()>()).await, None);
        assert!(deadline.is_elapsed());
    }
}
