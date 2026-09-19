//! The host's backoff, and the rule that a recoverable failure is never a terminal outcome.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use hub_host::testing::ScriptedJitter;
use hub_host::{RetryHint, RetryPolicy, Stop, WaitOutcome};
use mango_external_agents::testing::FrozenClock;
use mango_external_agents::{CancelReason, Clock};

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

/// A Hub knows when it will answer and the host does not, so the hint wins — up to the cap.
#[test]
fn a_hub_hint_replaces_the_computed_delay_and_is_still_clamped() {
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
        Duration::ZERO,
        "expected a hint of zero to be honoured rather than replaced by the backoff"
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
