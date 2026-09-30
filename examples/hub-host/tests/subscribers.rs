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
            // The first bytes carry the index; a padded delta must not flood a failure message.
            EventKind::TextDelta { text } => format!("text:{}", &text[..text.len().min(4)]),
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

/// A watcher that stays stalled across turns can lose an earlier turn's terminal: the queue drops
/// from the front, so the next turn's events push it out. The fan-out then holds no end for the
/// first turn, and only the Hub's record has that outcome.
#[tokio::test]
async fn a_later_turns_events_can_push_an_earlier_terminal_out() {
    let events = TurnBroadcast::new(1);
    let mut watcher = events.subscribe();
    for event in turn_events(1).await {
        events.publish(event);
    }
    let second_turn = turn_events(1).await;
    events.publish(
        second_turn
            .into_iter()
            .next()
            .expect("expected the second turn's opening delta"),
    );
    drop(events);

    let seen = drain_async(&mut watcher).await;

    assert_eq!(
        seen,
        ["gap:2", "text:0"],
        "expected the first turn's terminal to be gone with only the second turn's delta held \
         | received {seen:?}"
    );
}

/// For one turn published before the subscriber drains, the terminal survives the gap, which is
/// how a lagged subscriber learns to stop trusting what it accumulated and mark the view
/// incomplete. The test above shows the case where a later turn pushes it out.
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

/// `count` text deltas of `size` bytes each, tagged by index in their first bytes, then a
/// `Completed` terminal.
async fn sized_turn_events(count: usize, size: usize) -> Vec<AgentEvent> {
    let (sink, mut source) = EventSink::new(
        SessionId::new("chat-1"),
        TurnId::new("turn-1"),
        AttemptId::FIRST,
        Arc::new(SystemClock),
        count + 1,
    );
    for index in 0..count {
        let mut text = format!("{index:04}");
        text.push_str(&"x".repeat(size - text.len()));
        sink.emit(EventKind::TextDelta { text })
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

/// The bytes of delta text a subscriber was handed, and how many events that was.
fn held_text(seen: &[Delivery]) -> (usize, usize) {
    seen.iter()
        .filter_map(|item| match item {
            Delivery::Event(event) => match &event.kind {
                EventKind::TextDelta { text } => Some(text.len()),
                _ => Some(0),
            },
            Delivery::Gap { .. } => None,
        })
        .fold((0, 0), |(bytes, events), size| (bytes + size, events + 1))
}

/// Reads everything a subscriber already holds, keeping the deliveries themselves.
fn collect_sync(watcher: &mut TurnSubscriber) -> Vec<Delivery> {
    let mut seen = Vec::new();
    while let Some(item) = watcher.try_recv() {
        seen.push(item);
    }
    seen
}

const KIB: usize = 1024;

/// Room for 256 events by count but only a few kilobytes by size: the count alone would hold all
/// eleven of these events, so the stalled watcher must be told the byte budget dropped some.
#[tokio::test]
async fn a_stalled_subscriber_is_told_when_the_byte_budget_drops_events() {
    let events = TurnBroadcast::with_byte_budget(256, 3 * KIB);
    let mut stalled = events.subscribe();
    let published = sized_turn_events(10, KIB).await;
    let count = published.len();
    for event in published {
        events.publish(event);
    }

    let seen = collect_sync(&mut stalled);

    let first = seen.first().map(label);
    assert!(
        matches!(seen.first(), Some(Delivery::Gap { missed }) if *missed > 0),
        "expected the first delivery to be a gap for events dropped over the byte budget | \
         received {first:?}"
    );
    let missed: u64 = seen
        .iter()
        .map(|item| match item {
            Delivery::Gap { missed } => *missed,
            Delivery::Event(_) => 0,
        })
        .sum();
    let (held_bytes, held_events) = held_text(&seen);
    // The queue bills each event its own size as well as its text, so the same sum with the
    // per-event size added is what must fit; text alone would pass even if that drifted.
    let billed = held_bytes + held_events * size_of::<AgentEvent>();
    assert!(
        billed <= 3 * KIB,
        "expected at most {} bytes retained, counting each event's own size | received {billed} \
         bytes ({held_bytes} of text in {held_events} events)",
        3 * KIB
    );
    assert_eq!(
        missed + held_events as u64,
        count as u64,
        "expected every published event to be delivered or counted as missed | received \
         {held_events} delivered and {missed} missed of {count}"
    );
    let last = seen.last().map(label);
    assert_eq!(
        last.as_deref(),
        Some("completed"),
        "expected the terminal to survive the byte budget | received {last:?}"
    );
}

/// The byte budget is the same signal as the count: one gap, then the newest events.
#[tokio::test]
async fn a_byte_gap_arrives_before_the_events_that_are_still_held() {
    let events = TurnBroadcast::with_byte_budget(256, 3 * KIB);
    let mut stalled = events.subscribe();
    for event in sized_turn_events(6, KIB).await {
        events.publish(event);
    }
    drop(events);

    let seen = drain_async(&mut stalled).await;

    let gaps = seen.iter().filter(|item| item.starts_with("gap")).count();
    assert_eq!(
        gaps, 1,
        "expected one gap for one lag episode | received {seen:?}"
    );
    assert!(
        seen[0].starts_with("gap") && seen.len() < 7,
        "expected the gap first and fewer than the 7 published events after it | received {seen:?}"
    );
}

/// A subscriber that keeps up holds nothing back, so no byte budget can drop anything for it.
#[tokio::test]
async fn a_subscriber_that_keeps_up_sees_no_byte_gap() {
    let events = TurnBroadcast::with_byte_budget(256, 3 * KIB);
    let mut watcher = events.subscribe();
    let mut seen = Vec::new();
    for event in sized_turn_events(50, KIB).await {
        events.publish(event);
        seen.extend(drain_sync(&mut watcher));
    }

    assert_eq!(
        seen.len(),
        51,
        "expected 50 deltas and a terminal | received {} items",
        seen.len()
    );
    assert!(
        seen.iter().all(|item| !item.starts_with("gap")),
        "expected no gap for a subscriber that keeps up | received {seen:?}"
    );
}

/// Only the subscriber over the byte budget is told; a fast one beside it is not.
#[tokio::test]
async fn a_byte_gap_is_reported_only_to_the_subscriber_that_fell_behind() {
    let events = TurnBroadcast::with_byte_budget(256, 3 * KIB);
    let mut slow = events.subscribe();
    let mut fast = events.subscribe();
    let mut fast_seen = Vec::new();
    for event in sized_turn_events(10, KIB).await {
        events.publish(event);
        fast_seen.extend(drain_sync(&mut fast));
    }

    let slow_seen = drain_sync(&mut slow);

    assert!(
        slow_seen
            .first()
            .is_some_and(|item| item.starts_with("gap")),
        "expected the stalled subscriber to be told of a gap | received {slow_seen:?}"
    );
    assert!(
        fast_seen.iter().all(|item| !item.starts_with("gap")),
        "expected no gap for the fast subscriber | received {fast_seen:?}"
    );
}

/// An event bigger than the whole budget is still delivered: dropping the newest event would
/// drop the terminal, and the operation's end must never be lost.
#[tokio::test]
async fn an_event_larger_than_the_budget_is_still_delivered() {
    let events = TurnBroadcast::with_byte_budget(256, 64);
    let mut watcher = events.subscribe();
    let mut published = sized_turn_events(1, 4 * KIB).await.into_iter();
    events.publish(published.next().expect("expected the oversized delta"));

    let alone = drain_sync(&mut watcher);
    assert_eq!(
        alone.len(),
        1,
        "expected the lone oversized event to be delivered | received {alone:?}"
    );
    assert!(
        !alone[0].starts_with("gap"),
        "expected no gap for a lone oversized event | received {alone:?}"
    );

    events.publish(published.next().expect("expected the terminal"));
    let after = drain_sync(&mut watcher);
    assert_eq!(
        after,
        ["completed"],
        "expected the terminal to survive | received {after:?}"
    );
}

/// The count is exact: three events of room hold three, not the four tokio would round up to.
#[tokio::test]
async fn the_count_capacity_is_exact_even_when_not_a_power_of_two() {
    let events = TurnBroadcast::new(3);
    let mut watcher = events.subscribe();
    for event in turn_events(7).await {
        events.publish(event);
    }

    let seen = drain_sync(&mut watcher);

    assert_eq!(
        seen,
        ["gap:5", "text:5", "text:6", "completed"],
        "expected a gap of 5 before the 3 newest events | received {seen:?}"
    );
}

/// A watcher parked in `recv` wakes when the supervisor publishes, with no polling in between.
#[tokio::test]
async fn a_parked_recv_wakes_on_publish() {
    let events = TurnBroadcast::new(4);
    let mut watcher = events.subscribe();
    let waiting = tokio::spawn(async move { watcher.recv().await });
    tokio::task::yield_now().await;

    for event in turn_events(1).await.into_iter().take(1) {
        events.publish(event);
    }

    let woke = waiting.await.expect("expected the watcher task to finish");
    let seen = woke.as_ref().map(label);
    assert_eq!(
        seen.as_deref(),
        Some("text:0"),
        "expected the parked watcher to receive the published delta | received {seen:?}"
    );
}

/// A `recv` dropped before anything arrives loses nothing: the next call still sees the event.
#[tokio::test]
async fn a_cancelled_recv_loses_no_event() {
    let events = TurnBroadcast::new(4);
    let mut watcher = events.subscribe();
    let idle = tokio::time::timeout(std::time::Duration::ZERO, watcher.recv()).await;
    assert!(
        idle.is_err(),
        "expected nothing to be ready before anything is published | received {idle:?}"
    );

    for event in turn_events(1).await {
        events.publish(event);
    }
    drop(events);

    let seen = drain_async(&mut watcher).await;
    assert_eq!(
        seen,
        ["text:0", "completed"],
        "expected both events after a cancelled recv | received {seen:?}"
    );
}

/// With nobody attached the fan-out stores nothing, so a late subscriber sees no history.
#[tokio::test]
async fn publishing_with_no_subscriber_retains_nothing() {
    let events = TurnBroadcast::with_byte_budget(256, 3 * KIB);
    for event in turn_events(2).await {
        events.publish(event);
    }

    let mut late = events.subscribe();

    let seen = late.try_recv().map(|item| label(&item));
    assert!(
        seen.is_none(),
        "expected a late subscriber to see no history | received {seen:?}"
    );
}

/// An event every subscriber has read is released, not held until the queue wraps around.
#[tokio::test]
async fn an_event_every_subscriber_has_read_is_released() {
    let events = TurnBroadcast::new(256);
    let mut first = events.subscribe();
    let mut second = events.subscribe();
    for event in turn_events(1).await.into_iter().take(1) {
        events.publish(event);
    }
    let a = first.try_recv();
    let b = second.try_recv();
    let (Some(Delivery::Event(a)), Some(Delivery::Event(b))) = (a, b) else {
        panic!("expected both subscribers to receive the delta");
    };
    let weak = Arc::downgrade(&a);
    drop((a, b));

    assert!(
        weak.upgrade().is_none(),
        "expected the fan-out to hold no reference to an event everyone read | received {} \
         strong references",
        weak.strong_count()
    );
}

/// Dropping a stalled subscriber releases everything it had not read.
#[tokio::test]
async fn dropping_a_subscriber_releases_its_unread_events() {
    let events = TurnBroadcast::new(256);
    let mut reader = events.subscribe();
    let stalled = events.subscribe();
    for event in turn_events(3).await {
        events.publish(event);
    }
    let Some(Delivery::Event(first)) = reader.try_recv() else {
        panic!("expected the reader to receive the first delta");
    };
    let weak = Arc::downgrade(&first);
    drop(first);
    assert!(
        weak.upgrade().is_some(),
        "expected the stalled subscriber to still hold the first event"
    );

    drop(stalled);

    assert!(
        weak.upgrade().is_none(),
        "expected dropping the stalled subscriber to release its events | received {} strong \
         references",
        weak.strong_count()
    );
}
