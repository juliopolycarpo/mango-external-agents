//! The host's backoff policy. None of it belongs in the library.
//!
//! `docs/lifecycle.md` puts network retries and backoff on the host, so the library carries no
//! `Backoff` type and no `retry_after` field. What it does carry is the contract this policy has
//! to respect: bounded attempt deadlines, cancellation-aware waits, capped delays, vendor hints
//! honoured, and **no retry-count exhaustion** — a recoverable failure is not a terminal outcome,
//! so a counter that gave up would invent one.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::stop::Stop;

/// How much of a computed delay jitter is allowed to remove.
///
/// A band rather than full jitter. Full jitter — a delay drawn uniformly from `[0, backoff]` —
/// makes the sequence of delays non-monotonic, which means a host under load can wait *less* after
/// its fifth failure than after its first. A band keeps the growth visible while still breaking
/// the lockstep that makes a thundering herd.
const JITTER_SPREAD: f64 = 0.25;

/// Where a delay's randomness comes from.
///
/// Injected rather than drawn inline, so a test can force the maximum and the minimum of the
/// jitter band and assert on an exact [`Duration`] instead of a range.
pub trait Jitter: Send + Sync {
    /// A factor in `[0.0, 1.0]`, where `1.0` means no jitter is removed from the delay.
    ///
    /// A factor outside the range is clamped by [`RetryPolicy::delay_for`], so an implementation
    /// that gets its arithmetic wrong lengthens or shortens a wait rather than producing a
    /// negative duration.
    fn factor(&self) -> f64;
}

/// Jitter from the standard library's per-process random hash seed.
///
/// The production implementation. It pulls no new crate into a workspace whose `deny.toml` is a
/// policy surface: [`RandomState`] is seeded randomly per process, and hashing a monotonic counter
/// through it gives a different sequence in every host process without a dependency on `rand`.
#[derive(Debug)]
pub struct HashJitter {
    state: RandomState,
    counter: AtomicU64,
}

impl Default for HashJitter {
    fn default() -> Self {
        Self::new()
    }
}

impl HashJitter {
    /// A source seeded by this process.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::{HashJitter, Jitter};
    ///
    /// let jitter = HashJitter::new();
    /// let factor = jitter.factor();
    /// assert!((0.0..=1.0).contains(&factor), "expected [0.0, 1.0], received {factor}");
    /// ```
    pub fn new() -> Self {
        Self {
            state: RandomState::new(),
            counter: AtomicU64::new(0),
        }
    }
}

impl Jitter for HashJitter {
    fn factor(&self) -> f64 {
        let mut hasher = self.state.build_hasher();
        hasher.write_u64(self.counter.fetch_add(1, Ordering::Relaxed));
        // The top 53 bits are the ones a f64 can hold exactly, so the quotient lands in [0, 1).
        (hasher.finish() >> 11) as f64 / (1_u64 << 53) as f64
    }
}

/// How a cancellation-aware wait ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    /// The delay elapsed.
    Elapsed,
    /// The host stopped the operation before the delay elapsed.
    Stopped,
}

/// Capped exponential backoff with jitter, a bounded attempt deadline, and no attempt ceiling.
///
/// Deliberately has no `max_attempts`. A recoverable failure that has happened nine times is still
/// a recoverable failure; giving up on the tenth would turn "the network is down" into a terminal
/// outcome the Hub never recorded, and the host would then have to guess what it means.
#[derive(Clone)]
pub struct RetryPolicy {
    base: Duration,
    cap: Duration,
    attempt_deadline: Duration,
    jitter: Arc<dyn Jitter>,
}

impl std::fmt::Debug for RetryPolicy {
    /// Reports the bounds without naming the injected jitter source.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RetryPolicy")
            .field("base", &self.base)
            .field("cap", &self.cap)
            .field("attempt_deadline", &self.attempt_deadline)
            .finish_non_exhaustive()
    }
}

impl RetryPolicy {
    /// A policy that doubles from `base` up to `cap`, bounding each call by `attempt_deadline`.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::{HashJitter, RetryPolicy};
    /// use std::sync::Arc;
    /// use std::time::Duration;
    ///
    /// let policy = RetryPolicy::new(
    ///     Duration::from_millis(100),
    ///     Duration::from_secs(30),
    ///     Duration::from_secs(10),
    ///     Arc::new(HashJitter::new()),
    /// );
    /// assert_eq!(policy.attempt_deadline(), Duration::from_secs(10));
    /// ```
    pub fn new(
        base: Duration,
        cap: Duration,
        attempt_deadline: Duration,
        jitter: Arc<dyn Jitter>,
    ) -> Self {
        Self {
            base,
            cap,
            attempt_deadline,
            jitter,
        }
    }

    /// The longest any single Hub or vendor call may take before it is a recoverable failure.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::{HashJitter, RetryPolicy};
    /// use std::sync::Arc;
    /// use std::time::Duration;
    ///
    /// let policy = RetryPolicy::new(
    ///     Duration::from_millis(1),
    ///     Duration::from_secs(1),
    ///     Duration::from_secs(4),
    ///     Arc::new(HashJitter::new()),
    /// );
    /// assert_eq!(policy.attempt_deadline(), Duration::from_secs(4));
    /// ```
    pub fn attempt_deadline(&self) -> Duration {
        self.attempt_deadline
    }

    /// The longest delay this policy will ever wait.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::{HashJitter, RetryPolicy};
    /// use std::sync::Arc;
    /// use std::time::Duration;
    ///
    /// let policy = RetryPolicy::new(
    ///     Duration::from_millis(1),
    ///     Duration::from_secs(2),
    ///     Duration::from_secs(4),
    ///     Arc::new(HashJitter::new()),
    /// );
    /// assert_eq!(policy.cap(), Duration::from_secs(2));
    /// ```
    pub fn cap(&self) -> Duration {
        self.cap
    }

    /// How long to wait before recoverable failure number `failures` is tried again.
    ///
    /// Pure and synchronous, so the cap, the monotonicity and the hint clamp are assertable
    /// without a runtime. `failures` counts the failures already seen: the first retry passes `1`.
    /// A Hub's `hint` replaces the computed delay — the Hub knows when it will answer and the
    /// host does not — but it is still clamped by the cap, because a Hub that asks for an hour
    /// must not be able to park a host's operation for an hour.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::{RetryPolicy, testing::ScriptedJitter};
    /// use std::sync::Arc;
    /// use std::time::Duration;
    ///
    /// let policy = RetryPolicy::new(
    ///     Duration::from_secs(1),
    ///     Duration::from_secs(8),
    ///     Duration::from_secs(30),
    ///     Arc::new(ScriptedJitter::maximum()),
    /// );
    /// assert_eq!(policy.delay_for(1, None), Duration::from_secs(1));
    /// assert_eq!(policy.delay_for(2, None), Duration::from_secs(2));
    /// assert_eq!(policy.delay_for(9, None), Duration::from_secs(8));
    /// assert_eq!(policy.delay_for(1, Some(Duration::from_secs(600))), Duration::from_secs(8));
    /// ```
    pub fn delay_for(&self, failures: u32, hint: Option<Duration>) -> Duration {
        if let Some(hint) = hint {
            return hint.min(self.cap);
        }
        let doublings = failures.saturating_sub(1).min(u32::BITS - 1);
        let exponential = self
            .base
            .checked_mul(1_u32 << doublings)
            .unwrap_or(self.cap)
            .min(self.cap);
        let factor = self.jitter.factor().clamp(0.0, 1.0);
        exponential.mul_f64(1.0 - JITTER_SPREAD + JITTER_SPREAD * factor)
    }

    /// Waits out `delay`, ending early and promptly when the host stops the operation.
    ///
    /// The whole reason the delay is not a bare `sleep`. A host that shuts down while an operation
    /// is backing off must not have to wait out the backoff to shut down, and a stop that lands
    /// mid-delay must be the thing that ends it.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::{HashJitter, RetryPolicy, Stop, WaitOutcome};
    /// use mango_external_agents::CancelReason;
    /// use std::sync::Arc;
    /// use std::time::Duration;
    ///
    /// let runtime = tokio::runtime::Builder::new_current_thread()
    ///     .enable_time()
    ///     .build()
    ///     .expect("expected a current-thread runtime");
    /// runtime.block_on(async {
    ///     let policy = RetryPolicy::new(
    ///         Duration::from_secs(1),
    ///         Duration::from_secs(60),
    ///         Duration::from_secs(30),
    ///         Arc::new(HashJitter::new()),
    ///     );
    ///     let stop = Stop::new();
    ///     stop.stop(CancelReason::Shutdown);
    ///     assert_eq!(policy.wait(Duration::from_secs(3600), &stop).await, WaitOutcome::Stopped);
    /// });
    /// ```
    pub async fn wait(&self, delay: Duration, stop: &Stop) -> WaitOutcome {
        tokio::select! {
            // Biased so an already-pulled stop never loses a coin toss against a zero delay: a
            // stopped operation must not get one more turn round the loop.
            biased;
            () = stop.stopped() => WaitOutcome::Stopped,
            () = tokio::time::sleep(delay) => WaitOutcome::Elapsed,
        }
    }
}
