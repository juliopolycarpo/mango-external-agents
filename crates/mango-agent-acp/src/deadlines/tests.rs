//! The figures [`Deadlines`](super::Deadlines) reports, held to the guide's table, to their own
//! formulas under limits that tell the terms apart, and to the stage values the harness enforces.
//!
//! The stages themselves are driven to their limits in `tests/session/deadlines.rs`; these tests
//! pin the arithmetic and the one stage no session-level fake can reach, the wait for a turn
//! slot another task holds.

use std::sync::Arc;
use std::time::Duration;

use mango_external_agents::testing::FakeLauncher;
use mango_external_agents::{
    AttemptId, Configuration, Error, EventSink, HarnessIdentity, HostContext, Limits, SessionIds,
    SessionSnapshot, TransportKind, TransportSelection, TurnId,
};

use super::{
    Deadlines, cancel_grace, connection_close_bound, exit_notice_bound, process_stop_bound,
    session_close_bound, turn_release_bound,
};
use crate::client::SessionState;

fn secs(seconds: u64) -> Duration {
    Duration::from_secs(seconds)
}

/// Every row of `deadlines`, named, in the order the guide lists them.
fn rows(deadlines: &Deadlines) -> [(&'static str, Duration); 8] {
    [
        ("process_stop", deadlines.process_stop()),
        ("teardown", deadlines.teardown()),
        ("close", deadlines.close()),
        ("close_worst_case", deadlines.close_worst_case()),
        ("cancel_to_terminal", deadlines.cancel_to_terminal()),
        (
            "request_timeout_to_closed",
            deadlines.request_timeout_to_closed(),
        ),
        ("exit_to_closed", deadlines.exit_to_closed()),
        (
            "dropped_session_mid_turn",
            deadlines.dropped_session_mid_turn(),
        ),
    ]
}

fn assert_rows(deadlines: &Deadlines, expected: [u64; 8], under: &str) {
    for ((name, received), expected) in rows(deadlines).into_iter().zip(expected) {
        assert_eq!(
            received,
            secs(expected),
            "expected {name} under {under}: {expected} s | received: {received:?}"
        );
    }
}

/// The guide's second table, row by row, at the default limits.
#[test]
fn the_defaults_equal_the_guides_table() {
    let deadlines = Deadlines::from_limits(&Limits::default());
    // process stop, teardown, close, close worst case, cancel, request timeout, exit, drop.
    assert_rows(
        &deadlines,
        [14, 19, 29, 29, 21, 144, 34, 19],
        "the defaults",
    );
}

/// kill_grace 3 s, shutdown_timeout 7 s, request_timeout 101 s: no two terms share a value, so a
/// formula that reads the wrong limit for one of them lands on a different number.
#[test]
fn every_term_reads_its_own_limit() {
    let limits = Limits {
        kill_grace: secs(3),
        shutdown_timeout: secs(7),
        request_timeout: secs(101),
        // Above the cancel-settle floor the core asks of a host, and read by none of the rows.
        cancel_settle_timeout: secs(600),
        ..Limits::default()
    };
    // process stop: 2*3 + 2*7 = 20; teardown: 7 + 20 = 27; close: 7 + 27 + 7 = 41;
    // cancel: 3 + 27 = 30; request: 101 + 27 + 7 = 135; exit: 7 + 7 + 27 + 7 = 48.
    assert_rows(
        &Deadlines::from_limits(&limits),
        [20, 27, 41, 41, 30, 135, 48, 27],
        "kill_grace 3 s, shutdown_timeout 7 s, request_timeout 101 s",
    );
}

/// The two limits that are not the one a row names do not move it.
#[test]
fn a_row_ignores_the_limits_it_does_not_count() {
    let base = Limits::default();
    let louder = Limits {
        idle_timeout: secs(9999),
        approval_timeout: secs(9999),
        cancel_settle_timeout: secs(9999),
        ..base
    };
    assert_eq!(
        rows(&Deadlines::from_limits(&base)),
        rows(&Deadlines::from_limits(&louder)),
        "expected rows unchanged by idle_timeout, approval_timeout and cancel_settle_timeout | received: changed"
    );
    let slower_requests = Limits {
        request_timeout: secs(999),
        ..base
    };
    let before = Deadlines::from_limits(&base);
    let after = Deadlines::from_limits(&slower_requests);
    for (name, was) in rows(&before) {
        let now = rows(&after)
            .into_iter()
            .find(|(candidate, _)| *candidate == name)
            .map(|(_, now)| now)
            .expect("expected every row in both lists");
        let moves = name == "request_timeout_to_closed";
        assert_eq!(
            was != now,
            moves,
            "expected {name} to {} when only request_timeout changes | received: {was:?} then {now:?}",
            if moves { "change" } else { "stay" }
        );
    }
}

/// A limit of `Duration::MAX` ("no bound") saturates to `Duration::MAX` and never panics.
#[test]
fn a_limit_of_duration_max_saturates_without_panicking() {
    let unbounded = Limits {
        kill_grace: Duration::MAX,
        shutdown_timeout: Duration::MAX,
        request_timeout: Duration::MAX,
        cancel_settle_timeout: Duration::MAX,
        ..Limits::default()
    };
    for (name, received) in rows(&Deadlines::from_limits(&unbounded)) {
        assert_eq!(
            received,
            Duration::MAX,
            "expected {name} with every limit at Duration::MAX: {:?} | received: {received:?}",
            Duration::MAX
        );
    }
}

/// One huge term saturates only the rows that count it.
#[test]
fn one_huge_limit_saturates_only_the_rows_that_count_it() {
    let huge_request = Limits {
        request_timeout: Duration::MAX,
        ..Limits::default()
    };
    let deadlines = Deadlines::from_limits(&huge_request);
    assert_eq!(
        deadlines.request_timeout_to_closed(),
        Duration::MAX,
        "expected request_timeout_to_closed with request_timeout MAX: {:?} | received: {:?}",
        Duration::MAX,
        deadlines.request_timeout_to_closed()
    );
    assert_eq!(
        deadlines.close(),
        secs(29),
        "expected close unaffected by request_timeout MAX: 29 s | received: {:?}",
        deadlines.close()
    );
    let huge_grace = Limits {
        kill_grace: Duration::MAX,
        ..Limits::default()
    };
    let deadlines = Deadlines::from_limits(&huge_grace);
    assert_eq!(
        deadlines.exit_to_closed(),
        Duration::MAX,
        "expected exit_to_closed with kill_grace MAX: {:?} | received: {:?}",
        Duration::MAX,
        deadlines.exit_to_closed()
    );
}

/// `close_worst_case` is `close`, and `dropped_session_mid_turn` is `teardown`, whatever the limits.
#[test]
fn the_aliased_rows_follow_the_rows_they_name() {
    let limits = Limits {
        kill_grace: secs(11),
        shutdown_timeout: secs(13),
        request_timeout: secs(17),
        cancel_settle_timeout: secs(600),
        ..Limits::default()
    };
    let deadlines = Deadlines::from_limits(&limits);
    assert_eq!(
        deadlines.close_worst_case(),
        deadlines.close(),
        "expected close_worst_case == close | received: {:?} and {:?}",
        deadlines.close_worst_case(),
        deadlines.close()
    );
    assert_eq!(
        deadlines.dropped_session_mid_turn(),
        deadlines.teardown(),
        "expected dropped_session_mid_turn == teardown | received: {:?} and {:?}",
        deadlines.dropped_session_mid_turn(),
        deadlines.teardown()
    );
}

/// `Deadlines` holds what it was made from: a copy compares equal, a different limit does not.
#[test]
fn deadlines_compare_by_the_limits_they_were_made_from() {
    let base = Deadlines::from_limits(&Limits::default());
    let copy = base;
    assert_eq!(base, copy, "expected a copy to equal its source");
    let other = Deadlines::from_limits(&Limits {
        kill_grace: secs(1),
        ..Limits::default()
    });
    assert_ne!(base, other, "expected different limits to compare unequal");
}

/// Each stage the harness enforces is one named limit, and the sums are built from the same
/// functions: a swapped limit in either place shows up as a different number here.
#[test]
fn each_enforced_stage_reads_its_own_limit() {
    let limits = Limits {
        kill_grace: secs(3),
        shutdown_timeout: secs(7),
        request_timeout: secs(101),
        cancel_settle_timeout: secs(600),
        ..Limits::default()
    };
    let received = [
        ("cancel_grace", cancel_grace(&limits), secs(3)),
        (
            "connection_close_bound",
            connection_close_bound(&limits),
            secs(7),
        ),
        ("session_close_bound", session_close_bound(&limits), secs(7)),
        ("turn_release_bound", turn_release_bound(&limits), secs(7)),
        ("process_stop_bound", process_stop_bound(&limits), secs(20)),
        ("exit_notice_bound", exit_notice_bound(&limits), secs(14)),
    ];
    for (name, received, expected) in received {
        assert_eq!(
            received, expected,
            "expected {name}: {expected:?} | received: {received:?}"
        );
    }
}

fn host_under(limits: Limits) -> HostContext {
    HostContext::builder()
        .launcher(Arc::new(FakeLauncher::new()))
        .cwd(std::env::temp_dir())
        .client_info("deadlines-tests", "0.1.0")
        .limits(limits)
        .build()
        .expect("expected a host context")
}

/// The wait for a turn another task still holds ends exactly at `turn_release_bound`, with the
/// typed timeout naming that bound: the last term of `close`, `request_timeout_to_closed` and
/// `exit_to_closed`, driven to its limit.
#[tokio::test(start_paused = true)]
async fn a_held_turn_slot_is_waited_for_exactly_the_turn_release_bound() {
    let limits = Limits {
        shutdown_timeout: secs(7),
        // Above it, so the release wait is the only deadline in play.
        kill_grace: secs(3),
        cancel_settle_timeout: secs(600),
        ..Limits::default()
    };
    let host = host_under(limits);
    let state = SessionState::new(
        mango_external_agents::SessionId::new("session-1"),
        &host,
        Configuration::default(),
        mango_external_agents::SessionState::new(
            Arc::clone(host.clock()),
            SessionSnapshot::opening(
                SessionIds {
                    session_id: mango_external_agents::SessionId::new("session-1"),
                    native_session_id: String::from("native-1"),
                },
                HarnessIdentity::claude(),
                TransportSelection::new(None, TransportKind::Acp),
                std::time::SystemTime::UNIX_EPOCH,
            ),
        ),
    );
    let (sink, _events) = EventSink::new(
        mango_external_agents::SessionId::new("session-1"),
        TurnId::new("turn-held"),
        AttemptId::default(),
        Arc::clone(host.clock()),
        1,
    );
    let _held = state
        .begin_turn(sink, None)
        .expect("expected the turn to take the slot");

    let bound = turn_release_bound(host.limits());
    let began = tokio::time::Instant::now();
    let released = crate::session::wait_for_turn_release(&state, bound).await;
    let waited = began.elapsed();

    let timed_out = matches!(
        &released,
        Err(Error::Timeout { operation, after })
            if operation.contains("turn slot release") && *after == bound
    );
    assert!(
        timed_out && waited == bound,
        "expected Err(Timeout) naming the turn slot release after {bound:?}, waited {bound:?} | received: {released:?} after {waited:?}"
    );
}
