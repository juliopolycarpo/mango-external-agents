//! A browser is a watcher, not the owner of an operation.
//!
//! The library is explicit that dropping a `TurnStream` is abandonment. That only stays true if
//! the browser never holds one — so the supervisor keeps the stream and hands out subscribers,
//! and a tab closing mid-turn is an ordinary state of the world.

mod common;

use std::sync::Arc;

use hub_host::testing::{FakeHubApi, FakeVendorSession, HubCallKind, TurnAnswer};
use hub_host::{Commit, Settled, Stop, TurnBroadcast};
use mango_external_agents::{EventKind, TurnRequest};

/// The browser disconnects mid-turn and the operation still reaches its terminal and commits.
#[tokio::test(start_paused = true)]
async fn dropping_every_watcher_mid_turn_does_not_abandon_the_operation() {
    let hub = Arc::new(FakeHubApi::new());
    let session = FakeVendorSession::new().by_default(TurnAnswer::CompleteWhenReleased);
    let stop = Arc::new(Stop::new());
    let mut supervisor = common::supervisor(&session, &hub, &stop);
    let mut watcher = supervisor.subscribe();
    let running = tokio::spawn({
        let request = TurnRequest::new("turn-1", "ship it");
        async move { supervisor.run(request).await }
    });

    let first = watcher
        .recv()
        .await
        .expect("expected the turn's opening event");
    assert!(
        matches!(first.kind, EventKind::TurnStarted { .. }),
        "expected the turn to have started, received {:?}",
        first.kind
    );
    // The tab closes. Nothing else is watching.
    drop(watcher);
    session.release();

    let settled = running
        .await
        .expect("expected the run task to finish")
        .expect("expected the operation to settle");

    assert_eq!(
        settled,
        Settled::Committed {
            terminal: common::COMPLETED,
            commit: Commit::Recorded,
        }
    );
    assert_eq!(
        hub.count(HubCallKind::Commit),
        1,
        "expected the terminal to reach the hub with nobody watching"
    );
    assert!(
        session.cancels().is_empty(),
        "expected a disconnect not to cancel the vendor, received {:?}",
        session.cancels()
    );
}

/// An operation nobody ever watched still runs.
#[tokio::test(start_paused = true)]
async fn an_operation_with_no_watcher_at_all_still_commits() {
    let hub = Arc::new(FakeHubApi::new());
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
    assert_eq!(supervisor.subscriber_count(), 0);
}

/// Publishing to nobody is not a failure, which is the whole reason the supervisor may ignore it.
#[test]
fn publishing_with_no_subscriber_is_not_an_error() {
    let events = TurnBroadcast::new(4);
    let watcher = events.subscribe();
    assert_eq!(events.subscriber_count(), 1);
    drop(watcher);

    assert_eq!(events.subscriber_count(), 0);
}
