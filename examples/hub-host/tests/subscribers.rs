//! A browser is a watcher, not the owner of an operation.
//!
//! The library is explicit that dropping a `TurnStream` is abandonment. That only stays true if
//! the browser never holds one — so the supervisor keeps the stream and hands out subscribers,
//! and a tab closing mid-turn is an ordinary state of the world.

mod common;

use std::sync::Arc;

use hub_host::testing::{FakeHubApi, FakeVendorSession, HubCallKind, TurnAnswer};
use hub_host::{Commit, Delivery, Settled, Stop, TurnBroadcast, TurnSubscriber};
use mango_external_agents::{
    AgentEvent, AttemptId, EventKind, EventSink, SessionId, SystemClock, TurnId, TurnRequest,
};

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
        matches!(
            &first,
            Delivery::Event(event) if matches!(event.kind, EventKind::TurnStarted { .. })
        ),
        "expected the turn to have started, received {first:?}"
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

/// Text deltas `0..count`, then a `Completed` terminal, in the order a supervisor publishes them.
async fn turn_events(count: usize) -> Vec<AgentEvent> {
    let (sink, mut source) = EventSink::new(
        SessionId::new("chat-1"),
        TurnId::new("turn-1"),
        AttemptId::FIRST,
        Arc::new(SystemClock),
        count + 1,
    );
    for index in 0..count {
        sink.emit(EventKind::TextDelta {
            text: index.to_string(),
        })
        .await
        .expect("expected the sink to take the delta");
    }
    sink.emit(EventKind::Completed)
        .await
        .expect("expected the sink to take the terminal");
    let mut events = Vec::new();
    while let Ok(event) = source.try_recv() {
        events.push(event);
    }
    events
}

/// What a subscriber saw, as text a failure message can show: `gap:4`, `text:4`, `completed`.
fn label(item: &Delivery) -> String {
    match item {
        Delivery::Gap { missed } => format!("gap:{missed}"),
        Delivery::Event(event) => match &event.kind {
            EventKind::TextDelta { text } => format!("text:{text}"),
            EventKind::Completed => String::from("completed"),
            other => format!("other:{other:?}"),
        },
    }
}

/// Reads a subscriber to the end through `recv`, the way a UI task would.
async fn drain_async(watcher: &mut TurnSubscriber) -> Vec<String> {
    let mut seen = Vec::new();
    while let Some(item) = watcher.recv().await {
        seen.push(label(&item));
    }
    seen
}

/// Reads whatever a subscriber already holds through `try_recv`, the way a polling loop would.
fn drain_sync(watcher: &mut TurnSubscriber) -> Vec<String> {
    let mut seen = Vec::new();
    while let Some(item) = watcher.try_recv() {
        seen.push(label(&item));
    }
    seen
}

/// Seven deltas and a terminal into room for four: the subscriber is told four were missed, then
/// gets the four newest.
#[tokio::test]
async fn recv_reports_the_events_a_slow_subscriber_missed_then_delivers_the_rest() {
    let events = TurnBroadcast::new(4);
    let mut watcher = events.subscribe();
    for event in turn_events(7).await {
        events.publish(event);
    }
    drop(events);

    let seen = drain_async(&mut watcher).await;

    assert_eq!(
        seen,
        ["gap:4", "text:4", "text:5", "text:6", "completed"],
        "expected a gap of 4 before the four newest events | received {seen:?}"
    );
}

/// The polling path reports the same gap, because a poller is as blind as a task.
#[tokio::test]
async fn try_recv_reports_the_events_a_slow_subscriber_missed_then_delivers_the_rest() {
    let events = TurnBroadcast::new(4);
    let mut watcher = events.subscribe();
    for event in turn_events(7).await {
        events.publish(event);
    }

    let seen = drain_sync(&mut watcher);

    assert_eq!(
        seen,
        ["gap:4", "text:4", "text:5", "text:6", "completed"],
        "expected a gap of 4 before the four newest events | received {seen:?}"
    );
}

/// The terminal survives the gap, which is how a lagged subscriber learns to stop trusting what
/// it accumulated and resync.
#[tokio::test]
async fn the_terminal_is_delivered_after_a_gap() {
    let events = TurnBroadcast::new(2);
    let mut watcher = events.subscribe();
    for event in turn_events(10).await {
        events.publish(event);
    }
    drop(events);

    let seen = drain_async(&mut watcher).await;

    assert_eq!(
        seen,
        ["gap:9", "text:9", "completed"],
        "expected the gap first and the terminal last | received {seen:?}"
    );
}

/// A subscriber that keeps up is never told it missed anything.
#[tokio::test]
async fn a_subscriber_that_keeps_up_sees_no_gap() {
    let events = TurnBroadcast::new(4);
    let mut watcher = events.subscribe();
    let mut seen = Vec::new();
    for event in turn_events(20).await {
        events.publish(event);
        seen.extend(drain_sync(&mut watcher));
    }

    assert_eq!(
        seen.len(),
        21,
        "expected 20 deltas and a terminal | received {seen:?}"
    );
    assert!(
        seen.iter().all(|item| !item.starts_with("gap")),
        "expected no gap for a subscriber that keeps up | received {seen:?}"
    );
}

/// Only the subscriber that fell behind is told; one that kept up is not.
#[tokio::test]
async fn a_gap_is_reported_only_to_the_subscriber_that_fell_behind() {
    let events = TurnBroadcast::new(4);
    let mut slow = events.subscribe();
    let mut fast = events.subscribe();
    let mut fast_seen = Vec::new();
    for event in turn_events(7).await {
        events.publish(event);
        fast_seen.extend(drain_sync(&mut fast));
    }

    let slow_seen = drain_sync(&mut slow);

    assert_eq!(
        slow_seen.first().map(String::as_str),
        Some("gap:4"),
        "expected the slow subscriber to be told of a gap of 4 | received {slow_seen:?}"
    );
    assert!(
        fast_seen.iter().all(|item| !item.starts_with("gap")),
        "expected no gap for the fast subscriber | received {fast_seen:?}"
    );
}

/// A stalled subscriber never makes `publish` wait or fail, which is what lets the supervisor
/// keep draining the native stream no matter who is watching.
#[tokio::test]
async fn publishing_past_a_stalled_subscriber_neither_blocks_nor_fails() {
    let events = TurnBroadcast::new(2);
    let _stalled = events.subscribe();

    for event in turn_events(1_000).await {
        events.publish(event);
    }

    assert_eq!(events.subscriber_count(), 1);
}

/// A subscriber that lags, catches up and lags again is told about each episode separately.
#[tokio::test]
async fn each_lag_episode_is_reported_on_its_own() {
    let events = TurnBroadcast::new(2);
    let mut watcher = events.subscribe();
    let mut published = turn_events(9).await.into_iter();
    for event in published.by_ref().take(5) {
        events.publish(event);
    }
    let first = drain_sync(&mut watcher);
    for event in published {
        events.publish(event);
    }
    let second = drain_sync(&mut watcher);

    assert_eq!(
        first,
        ["gap:3", "text:3", "text:4"],
        "expected a gap of 3 for the first episode | received {first:?}"
    );
    assert_eq!(
        second,
        ["gap:3", "text:8", "completed"],
        "expected a gap of 3 for the second episode | received {second:?}"
    );
}
