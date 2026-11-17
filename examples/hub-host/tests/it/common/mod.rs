//! Construction shared by the integration tests, and nothing else.
//!
//! Every test builds the same four collaborators, so the assembly lives here and the tests stay
//! about what they are proving.
//!
//! Each integration binary compiles the whole module, so the helpers a given binary does not use
//! are dead code in that binary and only in that binary.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use hub_host::testing::{FakeHubApi, FakeVendorSession, ScriptedJitter};
use hub_host::{HubApi, RetryPolicy, Stop, Supervisor};
use mango_external_agents::{SystemClock, TerminalStatus};

/// A short base delay, so a test that backs off eight times is still quick under a paused clock.
pub const BASE_DELAY: Duration = Duration::from_millis(10);
/// A cap low enough that a test can reach it in a handful of doublings.
pub const MAX_DELAY: Duration = Duration::from_millis(80);
/// Far longer than anything a fake takes, so no test trips the per-attempt deadline by accident.
pub const ATTEMPT_DEADLINE: Duration = Duration::from_secs(30);

/// The policy every supervisor test runs under, with jitter pinned to the top of the band.
pub fn policy() -> RetryPolicy {
    RetryPolicy::new(
        BASE_DELAY,
        MAX_DELAY,
        ATTEMPT_DEADLINE,
        Arc::new(ScriptedJitter::maximum()),
    )
}

/// A supervisor over the given fakes, on the system clock.
///
/// The clock is real because nothing in the supervisor tests asserts on an instant; the
/// backoff tests that do assert on time use tokio's paused clock instead.
pub fn supervisor(
    session: &FakeVendorSession,
    hub: &Arc<FakeHubApi>,
    stop: &Arc<Stop>,
) -> Supervisor {
    Supervisor::new(
        Box::new(session.clone()),
        Arc::clone(hub) as Arc<dyn HubApi>,
        policy(),
        Arc::clone(stop),
        Arc::new(SystemClock),
    )
}

/// The terminal a healthy fake turn produces.
pub const COMPLETED: TerminalStatus = TerminalStatus::Completed;

/// Polls `condition` until it holds, advancing the runtime's timers, and gives up bounded.
///
/// A `yield_now` is not a synchronisation primitive: it hands the scheduler one turn and proves
/// nothing about a task that needs three. This waits for the observable fact instead, and fails
/// with what it actually saw rather than hanging.
pub async fn until(label: &str, mut condition: impl FnMut() -> bool) {
    for _ in 0..10_000 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("expected {label}, received a bounded poll that never saw it");
}
