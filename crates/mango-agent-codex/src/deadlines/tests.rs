use std::time::Duration;

use mango_external_agents::process::stop_process_with_limits;
use mango_external_agents::{CancelReason, Error, Limits};
use tokio::time::Instant;

use super::stalls::{JUST_UNDER, StallingControl};
use super::{Deadlines, close_backstop, interrupt_bound, total};

fn secs(seconds: u64) -> Duration {
    Duration::from_secs(seconds)
}

/// Limits whose every term differs, so a formula that reads the wrong one gives another sum.
fn distinct() -> Limits {
    Limits {
        request_timeout: secs(1_000),
        shutdown_timeout: secs(100),
        cancel_settle_timeout: secs(10_000),
        kill_grace: secs(7),
        ..Limits::default()
    }
}

fn assert_duration(row: &str, expected: Duration, received: Duration) {
    assert_eq!(
        received, expected,
        "{row}: expected {expected:?} | received: {received:?}"
    );
}

/// Each row of the guide's tables, at the defaults.
#[test]
fn every_row_matches_the_guide_at_the_defaults() {
    let deadlines = Deadlines::from_limits(&Limits::default());
    assert_duration("process stop", secs(14), deadlines.process_stop());
    assert_duration("teardown", secs(36), deadlines.teardown());
    assert_duration("close", secs(36), deadlines.close());
    assert_duration("close, worst case", secs(36), deadlines.close_worst_case());
    assert_duration(
        "overflow to closed",
        secs(41),
        deadlines.overflow_to_closed(),
    );
    assert_duration(
        "connection lost to closed",
        secs(46),
        deadlines.connection_lost_to_closed(),
    );
    assert_duration(
        "cancel, interrupt accepted",
        secs(336),
        deadlines.cancel_interrupt_accepted(),
    );
    assert_duration(
        "cancel, interrupt failed",
        secs(401),
        deadlines.cancel_interrupt_failed(),
    );
}

/// Every term reads its own limit: swapping two of them changes the sum.
#[test]
fn every_row_reads_its_own_limit() {
    let deadlines = Deadlines::from_limits(&distinct());
    // kg 7, st 100, rt 1000, cs 10000.
    let stop = 7 + 7 + 100 + 100;
    let teardown = 7 + 100 + 300 + stop;
    assert_duration("process stop", secs(stop), deadlines.process_stop());
    assert_duration("teardown", secs(teardown), deadlines.teardown());
    assert_duration(
        "overflow to closed",
        secs(100 + teardown),
        deadlines.overflow_to_closed(),
    );
    assert_duration(
        "connection lost to closed",
        secs(200 + teardown),
        deadlines.connection_lost_to_closed(),
    );
    assert_duration(
        "cancel, interrupt accepted",
        secs(1_000 + 1_000 + 10_000 + teardown),
        deadlines.cancel_interrupt_accepted(),
    );
    assert_duration(
        "cancel, interrupt failed",
        secs(3_000 + 100 + teardown),
        deadlines.cancel_interrupt_failed(),
    );
}

/// A close is the teardown it starts or joins; only a host timing from the overflow waits longer.
#[test]
fn a_close_is_bounded_by_the_teardown_and_no_more() {
    let deadlines = Deadlines::from_limits(&distinct());
    assert_duration("close", deadlines.teardown(), deadlines.close());
    assert_duration(
        "close, worst case",
        deadlines.teardown(),
        deadlines.close_worst_case(),
    );
    assert!(
        deadlines.overflow_to_closed() > deadlines.close_worst_case(),
        "expected the overflow window to outlast a close called inside it | received: overflow {:?}, close {:?}",
        deadlines.overflow_to_closed(),
        deadlines.close_worst_case()
    );
}

/// Limits at the edge of what a `Duration` holds saturate; none panics.
#[test]
fn extreme_limits_saturate_instead_of_panicking() {
    let limits = Limits {
        request_timeout: Duration::MAX,
        shutdown_timeout: Duration::MAX,
        cancel_settle_timeout: Duration::MAX,
        kill_grace: Duration::MAX,
        ..Limits::default()
    };
    let deadlines = Deadlines::from_limits(&limits);
    for (row, value) in [
        ("process stop", deadlines.process_stop()),
        ("teardown", deadlines.teardown()),
        ("close", deadlines.close()),
        ("close, worst case", deadlines.close_worst_case()),
        ("overflow to closed", deadlines.overflow_to_closed()),
        (
            "connection lost to closed",
            deadlines.connection_lost_to_closed(),
        ),
        (
            "cancel, interrupt accepted",
            deadlines.cancel_interrupt_accepted(),
        ),
        (
            "cancel, interrupt failed",
            deadlines.cancel_interrupt_failed(),
        ),
        ("close backstop", close_backstop(&limits)),
    ] {
        assert_duration(row, Duration::MAX, value);
    }
}

#[test]
fn the_teardown_stages_read_their_own_limit() {
    let limits = distinct();
    assert_duration("interrupt bound", secs(7), interrupt_bound(&limits));
    assert_duration("close backstop", secs(300), close_backstop(&limits));
}

#[test]
fn a_total_of_nothing_is_zero_and_never_overflows() {
    assert_duration("empty total", Duration::ZERO, total(&[]));
    assert_duration(
        "saturated total",
        Duration::MAX,
        total(&[Duration::MAX, secs(1)]),
    );
}

/// Core's stop, run against a child that spends every stage, takes the figure this module
/// states. This is what ties `process_stop` to the code that enforces it, which lives in the
/// core crate.
#[tokio::test(start_paused = true)]
async fn the_process_stop_is_what_core_spends_on_a_child_that_stalls_every_stage() {
    let limits = Limits::default();
    let expected = Deadlines::from_limits(&limits).process_stop();
    let control = StallingControl::under(&limits);

    let began = Instant::now();
    let stopped = stop_process_with_limits(&*control, CancelReason::Shutdown, &limits).await;
    let spent = began.elapsed();

    let slack = JUST_UNDER * 2;
    assert!(
        spent <= expected && spent >= expected - slack,
        "expected the stop to spend {expected:?} (less {slack:?}) | received: {spent:?}"
    );
    assert!(
        matches!(&stopped, Err(Error::Timeout { operation, .. }) if operation == "process reaping"),
        "expected the stalled stop to end at its reap: Timeout(process reaping) | received: {stopped:?}"
    );
}

/// The backstop leaves the JSON-RPC client's close room for everything it may use: twice
/// `shutdown_timeout`, and 100 ms for the two aborts it waits to land behind a handler that
/// blocks its thread, so that a close that uses all of it returns its own result and not the
/// backstop's timeout.
#[test]
fn the_close_backstop_leaves_room_for_the_clients_whole_close_at_the_defaults() {
    let limits = Limits::default();
    let clients_worst_close = limits.shutdown_timeout * 2 + Duration::from_millis(100);
    let backstop = close_backstop(&limits);
    assert!(
        backstop > clients_worst_close,
        "expected a backstop past the client's worst close {clients_worst_close:?} | received: {backstop:?}"
    );
}
