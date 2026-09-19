//! The host's backoff, and the rule that a recoverable failure is never a terminal outcome.

mod common;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use hub_host::testing::{
    FakeHubApi, FakeVendorSession, HubCallKind, ReserveAnswer, ScriptedJitter,
};
use hub_host::{
    Commit, HubApi, HubError, HubReceipt, Reconciliation, RetryHint, RetryPolicy, Settled, Stop,
    Supervisor, WaitOutcome,
};
use mango_external_agents::testing::FrozenClock;
use mango_external_agents::{
    CancelReason, Clock, OperationRef, RequestFingerprint, SystemClock, TerminalStatus, TurnRequest,
};
use tokio::time::Instant;

const BASE: Duration = Duration::from_secs(1);
const CAP: Duration = Duration::from_secs(8);
const DEADLINE: Duration = Duration::from_secs(30);

fn policy(jitter: ScriptedJitter) -> RetryPolicy {
    RetryPolicy::new(BASE, CAP, DEADLINE, Arc::new(jitter))
}

/// The two properties the cap exists for: it grows, and it stops growing.
#[test]
fn delays_grow_and_then_stop_at_the_cap() {
    let policy = policy(ScriptedJitter::maximum());
    let delays: Vec<Duration> = (1..=10)
        .map(|failure| policy.delay_for(failure, None))
        .collect();

    assert!(
        delays.windows(2).all(|pair| pair[0] <= pair[1]),
        "expected a non-decreasing sequence, received {delays:?}"
    );
    assert!(
        delays.iter().all(|delay| *delay <= CAP),
        "expected every delay within the cap of {CAP:?}, received {delays:?}"
    );
    assert_eq!(delays[0], BASE);
    assert_eq!(
        delays[9], CAP,
        "expected the tenth failure to sit at the cap, received {:?}",
        delays[9]
    );
}

/// The bottom of the jitter band shortens every delay without breaking the growth.
///
/// Full jitter — a delay drawn from `[0, backoff]` — would fail the monotonicity assertion here,
/// which is the reason this policy uses a band instead.
#[test]
fn the_smallest_jitter_still_leaves_a_growing_capped_sequence() {
    let policy = policy(ScriptedJitter::minimum());
    let delays: Vec<Duration> = (1..=10)
        .map(|failure| policy.delay_for(failure, None))
        .collect();

    assert!(
        delays.windows(2).all(|pair| pair[0] <= pair[1]),
        "expected a non-decreasing sequence, received {delays:?}"
    );
    assert!(
        delays.iter().all(|delay| *delay <= CAP),
        "expected every delay within the cap of {CAP:?}, received {delays:?}"
    );
    assert!(
        delays[0] < policy_with_maximum_jitter_first_delay(),
        "expected the minimum jitter to shorten the first delay, received {:?}",
        delays[0]
    );
}

fn policy_with_maximum_jitter_first_delay() -> Duration {
    policy(ScriptedJitter::maximum()).delay_for(1, None)
}

/// A Hub knows when it will answer and the host does not, so the hint wins — between the two bounds.
///
/// The hint is clamped above by the cap and below by the host's own backoff. A hint of zero is not
/// a request to retry immediately: it is what an *expired* hint resolves to, and every hint the
/// supervisor passes here has already been resolved against the host's clock. Honouring it
/// literally is how a host that is being rate limited hammers the Hub at full rate, so the
/// exponential is the floor and the Hub only gets to ask for a *longer* wait than the host chose.
#[test]
fn a_hub_hint_replaces_the_computed_delay_between_the_cap_and_the_backoff() {
    let policy = policy(ScriptedJitter::maximum());

    assert_eq!(
        policy.delay_for(1, Some(Duration::from_secs(5))),
        Duration::from_secs(5),
        "expected the hint to replace the one-second computed delay"
    );
    assert_eq!(
        policy.delay_for(1, Some(Duration::from_secs(3600))),
        CAP,
        "expected a one-hour hint to be clamped to the cap of {CAP:?}"
    );
    assert_eq!(
        policy.delay_for(1, Some(Duration::ZERO)),
        BASE,
        "expected an expired hint to leave the host's own backoff standing, not to erase it"
    );
}

/// An expired hint, repeated, must still grow the wait — or the host spins at full rate.
///
/// A Hub that keeps answering with a `not_before` already in the past resolves to
/// [`Duration::ZERO`] every time. Without the floor the whole capped-exponential computation is
/// discarded on every failure and the sequence is a flat zero, which is a hot loop against a Hub
/// that has just said it is overloaded.
#[test]
fn a_hint_that_has_already_expired_never_shortens_the_backoff() {
    let policy = policy(ScriptedJitter::maximum());
    let expired: Vec<Duration> = (1..=10)
        .map(|failure| policy.delay_for(failure, Some(Duration::ZERO)))
        .collect();
    let unhinted: Vec<Duration> = (1..=10)
        .map(|failure| policy.delay_for(failure, None))
        .collect();

    assert_eq!(
        expired, unhinted,
        "expected an expired hint to leave the computed sequence untouched, received {expired:?}"
    );
    assert!(
        expired.windows(2).all(|pair| pair[0] <= pair[1]),
        "expected a non-decreasing sequence, received {expired:?}"
    );
    assert_eq!(
        expired[0], BASE,
        "expected the first expired-hint delay to be the base of {BASE:?}, received {:?}",
        expired[0]
    );
    assert_eq!(
        expired[9], CAP,
        "expected the tenth expired-hint delay to sit at the cap of {CAP:?}, received {:?}",
        expired[9]
    );
}

/// A hint that named an instant is resolved when the host is about to wait, not when it arrived.
#[test]
fn a_hint_is_resolved_against_the_host_clock_at_the_moment_of_waiting() {
    let clock = FrozenClock::at(SystemTime::UNIX_EPOCH);
    let hint = RetryHint::not_before(SystemTime::UNIX_EPOCH + Duration::from_secs(30));

    assert_eq!(hint.remaining(clock.now()), Duration::from_secs(30));
    clock.advance(Duration::from_secs(29));
    assert_eq!(
        hint.remaining(clock.now()),
        Duration::from_secs(1),
        "expected a hint to shrink as the host's own clock moves"
    );
    clock.advance(Duration::from_secs(5));
    assert_eq!(
        hint.remaining(clock.now()),
        Duration::ZERO,
        "expected an expired hint to be nothing rather than a fresh full-length delay"
    );
}

/// A wait must end at the stop, not at the end of the delay.
///
/// The evidence is the elapsed time under a paused clock: the delay is ten minutes and the stop
/// lands after fifty milliseconds, so a wait that merely returned would not prove anything.
#[tokio::test(start_paused = true)]
async fn a_wait_ends_at_the_stop_rather_than_at_the_end_of_the_delay() {
    let policy = policy(ScriptedJitter::maximum());
    let stop = Arc::new(Stop::new());
    let stopping = tokio::spawn({
        let stop = Arc::clone(&stop);
        async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            stop.stop(CancelReason::Shutdown);
        }
    });

    let started = tokio::time::Instant::now();
    let outcome = policy.wait(Duration::from_secs(600), &stop).await;
    let elapsed = started.elapsed();
    stopping
        .await
        .expect("expected the stopping task to finish");

    assert_eq!(
        elapsed,
        Duration::from_millis(50),
        "expected the wait to end at the stop rather than after the full ten minutes"
    );
    assert_eq!(outcome, WaitOutcome::Stopped);
}

/// A recoverable failure that has happened eight times is still a recoverable failure.
///
/// A retry-count ceiling would turn "the network is down" into a terminal outcome the Hub never
/// recorded. Nothing here counts attempts for the purpose of giving up.
#[tokio::test(start_paused = true)]
async fn many_recoverable_failures_never_become_a_terminal_outcome() {
    let hub = Arc::new(FakeHubApi::new().reserving_after_recoverable_failures(8));
    let session = FakeVendorSession::new();
    let stop = Arc::new(Stop::new());
    let mut supervisor = common::supervisor(&session, &hub, &stop);

    let settled = supervisor
        .run(TurnRequest::new("turn-1", "ship it"))
        .await
        .expect("expected the operation to settle");

    assert_eq!(
        settled,
        Settled::Committed {
            terminal: common::COMPLETED,
            commit: Commit::Recorded,
        }
    );
    let attempts = hub.attempts(HubCallKind::Reserve);
    assert_eq!(
        attempts.len(),
        9,
        "expected eight failures and one success, received {attempts:?}"
    );
    assert!(
        attempts.windows(2).all(|pair| pair[1] > pair[0]),
        "expected strictly increasing attempt generations, received {attempts:?}"
    );
    let fingerprints = hub.fingerprints(HubCallKind::Reserve);
    assert!(
        fingerprints.windows(2).all(|pair| pair[0] == pair[1]),
        "expected one fingerprint across every attempt, received {} distinct submissions",
        fingerprints.len()
    );
    assert_eq!(
        session.start_count(),
        1,
        "expected the vendor work to run exactly once, after the hub finally accepted it"
    );
}

/// A [`HubApi`] that stamps when each submission arrived and otherwise defers to a [`FakeHubApi`].
///
/// The gap between one submission and the next *is* the backoff the supervisor waited out. It is
/// the only way to see the delays a loop nobody can step through actually took, and it is what
/// separates "the policy computes a growing sequence" from "the supervisor waits it out".
struct TimingHubApi {
    inner: Arc<FakeHubApi>,
    submissions: Mutex<Vec<Instant>>,
}

impl TimingHubApi {
    fn new(inner: Arc<FakeHubApi>) -> Self {
        Self {
            inner,
            submissions: Mutex::new(Vec::new()),
        }
    }

    /// How long the supervisor waited between each pair of consecutive submissions.
    fn gaps(&self) -> Vec<Duration> {
        let submissions = self
            .submissions
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        submissions
            .windows(2)
            .map(|pair| pair[1].duration_since(pair[0]))
            .collect()
    }
}

#[async_trait::async_trait]
impl HubApi for TimingHubApi {
    async fn reserve(
        &self,
        operation: &OperationRef,
        fingerprint: &RequestFingerprint,
    ) -> Result<HubReceipt, HubError> {
        self.submissions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Instant::now());
        self.inner.reserve(operation, fingerprint).await
    }

    async fn withdraw(&self, operation: &OperationRef) -> Result<(), HubError> {
        self.inner.withdraw(operation).await
    }

    async fn reconcile(&self, operation: &OperationRef) -> Result<Reconciliation, HubError> {
        self.inner.reconcile(operation).await
    }

    async fn commit(
        &self,
        operation: &OperationRef,
        terminal: &TerminalStatus,
    ) -> Result<Commit, HubError> {
        self.inner.commit(operation, terminal).await
    }
}

/// The delays this policy produces for the first eight failures, with jitter at the top of its band.
const EXPECTED_GAPS: [Duration; 8] = [
    Duration::from_millis(10),
    Duration::from_millis(20),
    Duration::from_millis(40),
    Duration::from_millis(80),
    Duration::from_millis(80),
    Duration::from_millis(80),
    Duration::from_millis(80),
    Duration::from_millis(80),
];

/// A Hub echoing a `Retry-After` that has already elapsed must not be hammered at full rate.
///
/// Eight recoverable failures in a row, each carrying a `not_before` in the past. Every one of them
/// resolves to [`Duration::ZERO`] against the host's clock, so a policy that honours a hint
/// literally waits nothing at all, eight times over, against a Hub that has just said it is
/// overloaded. The evidence is the gap between consecutive submissions under a paused clock: the
/// capped-exponential sequence, not a flat zero.
#[tokio::test(start_paused = true)]
async fn an_expired_hub_hint_does_not_collapse_the_backoff_to_a_spin() {
    let elapsed_hint = RetryHint::not_before(SystemTime::UNIX_EPOCH);
    let failures = (0..8).map(|attempt| {
        ReserveAnswer::Fail(
            HubError::recoverable(format!("rate limited {attempt}")).with_hint(elapsed_hint),
        )
    });
    let hub = Arc::new(TimingHubApi::new(Arc::new(
        FakeHubApi::new().reserving(failures),
    )));
    let session = FakeVendorSession::new();
    let stop = Arc::new(Stop::new());
    let mut supervisor = Supervisor::new(
        Box::new(session.clone()),
        Arc::clone(&hub) as Arc<dyn HubApi>,
        common::policy(),
        stop,
        Arc::new(SystemClock),
    );

    let started = Instant::now();
    let settled = supervisor
        .run(TurnRequest::new("turn-1", "ship it"))
        .await
        .expect("expected the operation to settle");
    let total = started.elapsed();

    assert_eq!(
        settled,
        Settled::Committed {
            terminal: common::COMPLETED,
            commit: Commit::Recorded,
        }
    );
    let gaps = hub.gaps();
    assert_eq!(
        gaps,
        EXPECTED_GAPS.to_vec(),
        "expected the capped-exponential sequence between submissions, received {gaps:?}"
    );
    assert!(
        gaps.windows(2).all(|pair| pair[0] <= pair[1]),
        "expected a non-decreasing sequence of waits, received {gaps:?}"
    );
    assert_eq!(
        total,
        EXPECTED_GAPS.iter().sum::<Duration>(),
        "expected the run to have waited out every backoff, received {total:?}"
    );
}
