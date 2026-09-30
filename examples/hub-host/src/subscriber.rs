//! The fan-out between the operation and whatever is watching it.
//!
//! The library is explicit that dropping a [`TurnStream`](mango_external_agents::TurnStream) is
//! abandonment and that a browser disconnect must only detach from the host's supervisor. That is
//! only enforceable if the browser is holding something *other* than the stream — so the
//! supervisor keeps the stream and hands every watcher one of these instead. Dropping the last one
//! is an ordinary state of the world, not an error.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use mango_external_agents::content::{ActivityContent, FileChange, PlanStep};
use mango_external_agents::{AgentEvent, EventKind};
use tokio::sync::Notify;

/// One thing a subscriber reads: an event, or the news that events were lost before it.
///
/// [`AgentEvent`] carries no sequence number, so a subscriber cannot notice a hole by itself. A
/// slow watcher would otherwise render `4, 5, 6, 7` after missing `0..=3` as a complete answer.
/// [`Delivery::Gap`] is that notice, delivered in order: it arrives where the missing events
/// would have been, before the oldest event that is still held.
///
/// # After a gap
///
/// On a [`Delivery::Gap`] the subscriber must stop treating what it accumulated for the current
/// operation as complete. Text deltas are not idempotent, so it cannot patch the hole. It should
/// mark the view as incomplete and keep reading: the events the queue still holds are delivered
/// after the gap, so the end of the operation being read normally still arrives. It does not
/// always, because a stalled subscriber that spans several turns can have an earlier turn's
/// terminal pushed out of the queue by a later turn's events.
///
/// That is all a gap leaves recoverable from the fan-out: the events still held after it, and
/// `missed`, how many were dropped. The lost deltas cannot be rebuilt. The stream cannot be
/// replayed, and this crate keeps no transcript to rebuild them from: [`HubApi`](crate::HubApi)
/// records only an operation's committed terminal outcome, never its events, and this fan-out
/// retains only a bounded window of them. A terminal that was dropped is still on record at the
/// Hub, and [`HubApi::reconcile`](crate::HubApi::reconcile) answers with it. A subscriber's own
/// copy of the events has the same hole, so it cannot fill it either. A host that must be able to
/// show the whole text of an operation has to record the events where the supervisor drains the
/// stream, before the fan-out, and own that storage. A subscriber that only renders a live view
/// may instead show the answer as truncated.
///
/// A gap names no operation. One broadcast serves every turn a supervisor runs, so the dropped
/// events may include an earlier turn's terminal, and the next retained event may already belong
/// to a later turn. Treat a gap as invalidating *every* in-flight view the subscriber holds, and
/// mark each one incomplete, unless it has already seen that operation's terminal.
///
/// `missed` counts events dropped for *any* reason, such as a history that has to forget its
/// oldest events, so a subscriber handles every cause the same way.
///
/// # Example
///
/// ```
/// use hub_host::{Delivery, TurnBroadcast};
/// use mango_external_agents::{AttemptId, EventKind, EventSink, SessionId, SystemClock, TurnId};
/// use std::sync::Arc;
///
/// let runtime = tokio::runtime::Builder::new_current_thread()
///     .build()
///     .expect("expected a current-thread runtime");
/// runtime.block_on(async {
///     let (sink, mut source) = EventSink::new(
///         SessionId::new("chat-1"),
///         TurnId::new("turn-1"),
///         AttemptId::FIRST,
///         Arc::new(SystemClock),
///         4,
///     );
///     let events = TurnBroadcast::new(1);
///     let mut watcher = events.subscribe();
///     for text in ["a", "b", "c"] {
///         sink.emit(EventKind::TextDelta { text: String::from(text) })
///             .await
///             .expect("expected the sink to take the event");
///         events.publish(source.try_recv().expect("expected the event back"));
///     }
///
///     // Room for one event and three were published: two were missed.
///     let mut complete = true;
///     while let Some(item) = watcher.try_recv() {
///         match item {
///             Delivery::Gap { missed } => {
///                 assert_eq!(missed, 2);
///                 complete = false;
///             }
///             Delivery::Event(_) => {}
///         }
///     }
///     assert!(!complete, "expected the watcher to know its view is incomplete");
/// });
/// ```
#[derive(Debug, Clone)]
pub enum Delivery {
    /// The next event.
    Event(Arc<AgentEvent>),
    /// Events were dropped before this subscriber read them.
    Gap {
        /// How many events were dropped, always at least one.
        missed: u64,
    },
}

/// The default for how many bytes of events a fan-out retains for a watcher that has fallen behind.
///
/// The same 8 MiB as [`Limits::turn_buffer_bytes`](mango_external_agents::Limits): once the
/// supervisor has drained the library's own queue, the host keeps no more than the library itself
/// would have been willing to buffer.
pub const DEFAULT_RETAINED_BYTES: usize = 8 * 1024 * 1024;

/// What an activity's fixed fields (name, title, ids) are counted as, on top of its text.
const ACTIVITY_OVERHEAD_BYTES: usize = 512;

/// What one event costs against the byte budget.
///
/// The event's own size plus its payload. What can be large is text, so the payload is measured
/// by its text and never by serializing it: a text or reasoning delta is the buffer holding its text
/// (its capacity, because sanitizing can leave a short string in a large buffer), and an activity
/// (started, updated or completed) is its detail plus its content (a diff of up to
/// [`DIFF_MAX_CONTENT_LENGTH`](mango_external_agents::content::DIFF_MAX_CONTENT_LENGTH) code
/// points, or output). Every other kind is small, bounded by the library's field limits, and is
/// counted by its encoded JSON length instead.
fn event_bytes(event: &AgentEvent) -> usize {
    let payload = match &event.kind {
        EventKind::TextDelta { text } | EventKind::ReasoningDelta { text } => text.capacity(),
        EventKind::ActivityStarted { activity, .. } => activity_bytes(
            [Some(activity.title.as_str()), activity.detail.as_deref()],
            activity.content.as_ref(),
        ),
        EventKind::ActivityUpdated { update, .. } => activity_bytes(
            [update.title.as_deref(), update.detail.as_deref()],
            update.content.as_ref(),
        ),
        EventKind::ActivityCompleted { result, .. } => {
            activity_bytes([result.detail.as_deref(), None], result.content.as_ref())
        }
        other => json_len(other),
    };
    size_of::<AgentEvent>().saturating_add(payload)
}

/// An activity's text fields and structured content, counted by length.
fn activity_bytes(texts: [Option<&str>; 2], content: Option<&ActivityContent>) -> usize {
    let text: usize = texts.into_iter().flatten().map(str::len).sum();
    let content = match content {
        Some(ActivityContent::Output { text }) => text.capacity(),
        Some(ActivityContent::Diff { files }) => files.iter().map(file_change_bytes).sum(),
        Some(ActivityContent::Plan { steps }) => steps.iter().map(plan_step_bytes).sum(),
        Some(_) | None => 0,
    };
    ACTIVITY_OVERHEAD_BYTES
        .saturating_add(text)
        .saturating_add(content)
}

/// One changed file: its paths and every body it carries, plus a row's worth of fixed fields.
fn file_change_bytes(file: &FileChange) -> usize {
    [
        Some(file.path.as_str()),
        file.previous_path.as_deref(),
        file.unified_diff.as_deref(),
        file.old_text.as_deref(),
        file.new_text.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::len)
    .fold(size_of::<FileChange>(), usize::saturating_add)
}

/// One plan step: its title and id, plus a row's worth of fixed fields.
fn plan_step_bytes(step: &PlanStep) -> usize {
    let id = step.id.as_deref().map_or(0, str::len);
    size_of::<PlanStep>()
        .saturating_add(step.title.len())
        .saturating_add(id)
}

/// The encoded length of `kind`, counted without building the string.
fn json_len(kind: &EventKind) -> usize {
    #[derive(Default)]
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter::default();
    // A kind that cannot be serialized is counted as empty: the budget is an approximation
    // and `publish` must not fail.
    let _ = serde_json::to_writer(&mut counter, kind);
    counter.0
}

/// The supervisor's side of the fan-out.
///
/// Each watcher has its own queue of the events it has not read yet, bounded by a count and by a
/// number of bytes. When either bound is exceeded the *oldest* event is dropped and the watcher
/// is told through [`Delivery::Gap`]. Events are shared through [`Arc`], so a few stalled
/// watchers cost about as much as one: their queues are all suffixes of the same stream.
///
/// Publishing never blocks on a watcher and cannot fail. With nobody watching it stores nothing,
/// and an event every watcher has read is not kept. A supervisor that propagated a "no receivers"
/// error with `?` would abandon an operation the moment a browser tab closed, which is the exact
/// failure this type exists to make impossible.
///
/// # Retention
///
/// The bound is on the size of a watcher's queue *after* dropping, with one exception: the newest
/// event is always kept, even when it alone exceeds the byte budget, because dropping it could
/// drop the operation's terminal. A queue therefore holds at most `capacity` events and at most
/// the byte budget plus the newest event.
#[derive(Debug)]
pub struct TurnBroadcast {
    slots: Mutex<Vec<Weak<Slot>>>,
    capacity: usize,
    max_retained_bytes: usize,
}

impl TurnBroadcast {
    /// A fan-out holding at most `capacity` events, and [`DEFAULT_RETAINED_BYTES`] of them, for
    /// a subscriber that has fallen behind.
    ///
    /// `capacity` is exact, and a capacity of zero holds one event.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::TurnBroadcast;
    ///
    /// let events = TurnBroadcast::new(16);
    /// assert_eq!(events.subscriber_count(), 0);
    /// ```
    pub fn new(capacity: usize) -> Self {
        Self::with_byte_budget(capacity, DEFAULT_RETAINED_BYTES)
    }

    /// A fan-out holding at most `capacity` events and about `max_retained_bytes` of them for a
    /// subscriber that has fallen behind, whichever is reached first.
    ///
    /// A byte is counted as the event's in-memory size plus its text, so the budget bounds the
    /// heap a stalled watcher pins rather than the size of the JSON a browser would receive. The
    /// newest event is kept even when it alone is larger than the budget.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::TurnBroadcast;
    ///
    /// // Room for 256 events, but never more than 1 MiB of them.
    /// let events = TurnBroadcast::with_byte_budget(256, 1024 * 1024);
    /// assert_eq!(events.subscriber_count(), 0);
    /// ```
    pub fn with_byte_budget(capacity: usize, max_retained_bytes: usize) -> Self {
        Self {
            slots: Mutex::new(Vec::new()),
            capacity: capacity.max(1),
            max_retained_bytes,
        }
    }

    /// One watcher's handle, which it may drop at any time.
    ///
    /// The watcher sees what is published from now on, never what came before.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::TurnBroadcast;
    ///
    /// let events = TurnBroadcast::new(16);
    /// let watcher = events.subscribe();
    /// assert_eq!(events.subscriber_count(), 1);
    /// drop(watcher);
    /// assert_eq!(events.subscriber_count(), 0);
    /// ```
    pub fn subscribe(&self) -> TurnSubscriber {
        let slot = Arc::new(Slot::default());
        let mut slots = lock(&self.slots);
        slots.retain(|other| other.strong_count() > 0);
        slots.push(Arc::downgrade(&slot));
        TurnSubscriber { slot }
    }

    /// How many watchers are attached right now.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::TurnBroadcast;
    ///
    /// assert_eq!(TurnBroadcast::new(4).subscriber_count(), 0);
    /// ```
    pub fn subscriber_count(&self) -> usize {
        lock(&self.slots)
            .iter()
            .filter(|slot| slot.strong_count() > 0)
            .count()
    }

    /// Offers one event to every watcher, ignoring the fact that there may be none.
    ///
    /// Never waits for a watcher: each one's queue is updated under a lock held for a few
    /// instructions, and whatever it cannot fit is dropped from the front and counted.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::TurnBroadcast;
    /// use mango_external_agents::{AttemptId, EventKind, EventSink, SessionId, SystemClock, TurnId};
    /// use std::sync::Arc;
    ///
    /// let runtime = tokio::runtime::Builder::new_current_thread()
    ///     .build()
    ///     .expect("expected a current-thread runtime");
    /// runtime.block_on(async {
    ///     let (sink, mut source) = EventSink::new(
    ///         SessionId::new("chat-1"),
    ///         TurnId::new("turn-1"),
    ///         AttemptId::FIRST,
    ///         Arc::new(SystemClock),
    ///         4,
    ///     );
    ///     sink.emit(EventKind::TextDelta { text: String::from("hello") })
    ///         .await
    ///         .expect("expected the sink to take the event");
    ///     let event = source.try_recv().expect("expected the event back");
    ///
    ///     // Nobody is watching, and publishing still succeeds.
    ///     let events = TurnBroadcast::new(4);
    ///     assert_eq!(events.subscriber_count(), 0);
    ///     events.publish(event);
    /// });
    /// ```
    pub fn publish(&self, event: AgentEvent) {
        // Snapshot the watchers and let go of the list before sizing the event, so measuring a
        // large one never holds up `subscribe`.
        let watchers: Vec<Arc<Slot>> = {
            let mut slots = lock(&self.slots);
            slots.retain(|slot| slot.strong_count() > 0);
            slots.iter().filter_map(Weak::upgrade).collect()
        };
        if watchers.is_empty() {
            return;
        }
        let held = Held {
            bytes: event_bytes(&event),
            event: Arc::new(event),
        };
        for slot in watchers {
            slot.push(held.clone(), self.capacity, self.max_retained_bytes);
        }
    }
}

impl Drop for TurnBroadcast {
    /// Tells every watcher that nothing more is coming, once it has read what is still held.
    fn drop(&mut self) {
        for slot in lock(&self.slots).iter().filter_map(Weak::upgrade) {
            slot.close();
        }
    }
}

/// One published event and what it cost, shared by every queue that holds it.
#[derive(Clone, Debug)]
struct Held {
    event: Arc<AgentEvent>,
    bytes: usize,
}

/// One watcher's queue and the way to wake it.
#[derive(Debug, Default)]
struct Slot {
    state: Mutex<Queue>,
    woken: Notify,
}

#[derive(Debug, Default)]
struct Queue {
    held: VecDeque<Held>,
    bytes: usize,
    /// Events dropped since the watcher last read, delivered as one gap before `held`.
    missed: u64,
    closed: bool,
}

/// What a watcher finds when it looks at its queue.
enum Next {
    Ready(Delivery),
    Closed,
    Empty,
}

impl Slot {
    /// Appends `held`, then drops from the front until both bounds hold again.
    ///
    /// The newest event is never dropped, so a queue can hold one event over the byte budget.
    fn push(&self, held: Held, capacity: usize, max_bytes: usize) {
        {
            let mut queue = lock(&self.state);
            queue.bytes = queue.bytes.saturating_add(held.bytes);
            queue.held.push_back(held);
            while queue.held.len() > capacity || (queue.bytes > max_bytes && queue.held.len() > 1) {
                let Some(oldest) = queue.held.pop_front() else {
                    break;
                };
                queue.bytes = queue.bytes.saturating_sub(oldest.bytes);
                queue.missed = queue.missed.saturating_add(1);
            }
        }
        self.woken.notify_one();
    }

    fn close(&self) {
        lock(&self.state).closed = true;
        self.woken.notify_one();
    }

    /// The next thing to hand the watcher: a pending gap first, then the oldest held event.
    fn next(&self) -> Next {
        let mut queue = lock(&self.state);
        if queue.missed > 0 {
            let missed = std::mem::take(&mut queue.missed);
            return Next::Ready(Delivery::Gap { missed });
        }
        if let Some(held) = queue.held.pop_front() {
            queue.bytes = queue.bytes.saturating_sub(held.bytes);
            return Next::Ready(Delivery::Event(held.event));
        }
        if queue.closed {
            return Next::Closed;
        }
        Next::Empty
    }
}

/// Locks `mutex`, carrying on with the data if a panic poisoned it: a queue of events is still a
/// queue of events, and `publish` must not fail.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One watcher's handle on a running operation.
///
/// Dropping it detaches that watcher and releases every event it had not read. It does not
/// cancel, abandon or even slow the operation: a lagged subscriber is told it lagged, through
/// [`Delivery::Gap`], and carries on.
#[derive(Debug)]
pub struct TurnSubscriber {
    slot: Arc<Slot>,
}

impl TurnSubscriber {
    /// The next delivery, or `None` once the supervisor has finished publishing.
    ///
    /// A watcher that fell too far behind is not disconnected, because a UI that missed three
    /// deltas still wants the fourth. It receives a [`Delivery::Gap`] counting what it missed,
    /// then continues from the oldest event still held. Events dropped between two reads are
    /// counted into a single gap, but more drops after one was read produce another, so gaps
    /// can arrive back to back.
    ///
    /// Cancel-safe: dropping the future loses nothing, and the next call reads the same queue.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::TurnBroadcast;
    ///
    /// let runtime = tokio::runtime::Builder::new_current_thread()
    ///     .build()
    ///     .expect("expected a current-thread runtime");
    /// runtime.block_on(async {
    ///     let events = TurnBroadcast::new(4);
    ///     let mut watcher = events.subscribe();
    ///     drop(events);
    ///     assert!(watcher.recv().await.is_none());
    /// });
    /// ```
    pub async fn recv(&mut self) -> Option<Delivery> {
        loop {
            // `notify_one` keeps a permit when nobody is waiting yet, so an event published
            // between the check and the wait below is not missed.
            match self.slot.next() {
                Next::Ready(delivery) => return Some(delivery),
                Next::Closed => return None,
                Next::Empty => self.slot.woken.notified().await,
            }
        }
    }

    /// The next delivery if one is already waiting, without suspending the caller.
    ///
    /// Reports a [`Delivery::Gap`] exactly as [`recv`](Self::recv) does.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::TurnBroadcast;
    ///
    /// let events = TurnBroadcast::new(4);
    /// let mut watcher = events.subscribe();
    /// assert!(watcher.try_recv().is_none());
    /// ```
    pub fn try_recv(&mut self) -> Option<Delivery> {
        match self.slot.next() {
            Next::Ready(delivery) => Some(delivery),
            Next::Closed | Next::Empty => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_RETAINED_BYTES, event_bytes, json_len};
    use mango_external_agents::content::{ActivityContent, FileChange, PlanStep};
    use mango_external_agents::{
        Activity, ActivityKind, ActivityResult, ActivityStatus, ActivityUpdate, AgentEvent,
        AttemptId, EventKind, EventSink, Limits, SessionId, SystemClock, TurnId,
    };
    use std::sync::Arc;

    /// The event the library would stamp around `kind`.
    async fn event(kind: EventKind) -> AgentEvent {
        let (sink, mut source) = EventSink::new(
            SessionId::new("chat-1"),
            TurnId::new("turn-1"),
            AttemptId::FIRST,
            Arc::new(SystemClock),
            1,
        );
        sink.emit(kind)
            .await
            .expect("expected the sink to take the event");
        source.try_recv().expect("expected the event back")
    }

    #[tokio::test]
    async fn a_text_delta_costs_its_event_plus_its_text() {
        let text = "x".repeat(1_000);
        let cost = event_bytes(&event(EventKind::TextDelta { text }).await);
        assert_eq!(
            cost,
            size_of::<AgentEvent>() + 1_000,
            "expected the event size plus 1000 bytes of text | received {cost}"
        );
    }

    /// Sanitizing a delta leaves a short string in a buffer sized for the raw one, and it is the
    /// buffer that a stalled watcher keeps alive.
    #[tokio::test]
    async fn a_delta_costs_the_buffer_it_pins_not_only_its_length() {
        // Bidirectional overrides are stripped, so 90 KB of them leave five bytes of text.
        let raw = format!("{}short", "\u{202e}".repeat(30_000));
        let stamped = event(EventKind::TextDelta { text: raw }).await;
        let EventKind::TextDelta { text } = &stamped.kind else {
            panic!("expected a text delta back, received {stamped:?}");
        };
        assert_eq!(text, "short", "expected the overrides to be stripped");
        let held = text.capacity();
        let cost = event_bytes(&stamped);
        assert!(
            cost >= size_of::<AgentEvent>() + held,
            "expected the cost to include the {held} byte buffer of a {} byte string | \
             received {cost}",
            text.len()
        );
    }

    #[tokio::test]
    async fn a_reasoning_delta_costs_its_text_too() {
        let text = "y".repeat(500);
        let cost = event_bytes(&event(EventKind::ReasoningDelta { text }).await);
        assert_eq!(
            cost,
            size_of::<AgentEvent>() + 500,
            "expected the event size plus 500 bytes of text | received {cost}"
        );
    }

    #[tokio::test]
    async fn a_kind_without_text_costs_its_event_plus_its_encoded_length() {
        let kind = EventKind::Completed;
        let encoded = json_len(&kind);
        assert_eq!(
            encoded,
            r#"{"type":"completed"}"#.len(),
            "expected the length of the encoded kind | received {encoded}"
        );
        let cost = event_bytes(&event(kind).await);
        assert_eq!(
            cost,
            size_of::<AgentEvent>() + encoded,
            "expected the event size plus the encoded kind | received {cost}"
        );
    }

    #[tokio::test]
    async fn an_activity_carrying_a_diff_costs_at_least_the_diff() {
        // The library bounds each file's diff body, so a large diff is many files.
        let body = "+".repeat(4_000);
        let files: Vec<FileChange> = (0..30)
            .map(|index| {
                FileChange::new(format!("src/file{index}.rs")).with_unified_diff(body.clone())
            })
            .collect();
        let result = ActivityResult::new(ActivityStatus::Completed)
            .with_content(ActivityContent::Diff { files });
        let carried = 30 * body.len();
        let cost = event_bytes(
            &event(EventKind::ActivityCompleted {
                call_id: String::from("call-1"),
                result,
            })
            .await,
        );
        assert!(
            cost >= carried,
            "expected an event carrying {carried} bytes of diff to cost at least that | \
             received {cost}"
        );
    }

    /// A diff arrives at the start of an edit as well as at its end, and must be sized by its
    /// bodies: encoding it would cost roughly twice as much here, because every newline in a
    /// diff is escaped in JSON.
    #[tokio::test]
    async fn an_activity_started_with_a_diff_is_sized_by_its_bodies_not_encoded() {
        let body = "+\n".repeat(2_000);
        let files: Vec<FileChange> = (0..30)
            .map(|index| {
                FileChange::new(format!("src/file{index}.rs")).with_unified_diff(body.clone())
            })
            .collect();
        let mut activity = Activity::new("Edit", ActivityKind::FileChange, "Edit files");
        activity.content = Some(ActivityContent::Diff { files });
        let carried = 30 * body.len();
        let cost = event_bytes(
            &event(EventKind::ActivityStarted {
                call_id: String::from("call-1"),
                activity,
            })
            .await,
        );
        let ceiling = carried + 30 * (size_of::<FileChange>() + 32) + 2_048;
        assert!(
            (carried..=ceiling).contains(&cost),
            "expected an ActivityStarted carrying {carried} bytes of diff to cost between \
             {carried} and {ceiling} | received {cost}"
        );
    }

    #[test]
    fn the_default_budget_is_the_librarys_own_turn_buffer() {
        assert_eq!(
            DEFAULT_RETAINED_BYTES,
            Limits::default().turn_buffer_bytes,
            "expected the default retained bytes to equal the library's turn buffer budget"
        );
    }

    #[tokio::test]
    async fn an_activity_update_costs_its_output_and_its_detail() {
        let update = ActivityUpdate::new()
            .with_detail("d".repeat(300))
            .with_content(ActivityContent::Output {
                text: "o".repeat(3_000),
            });
        let cost = event_bytes(
            &event(EventKind::ActivityUpdated {
                call_id: String::from("call-1"),
                update,
            })
            .await,
        );
        assert!(
            cost >= 3_300,
            "expected an update with 3000 bytes of output and 300 of detail to cost at least \
             3300 | received {cost}"
        );
    }

    #[tokio::test]
    async fn an_activity_result_costs_its_detail_and_its_plan() {
        let steps = vec![PlanStep::new("t".repeat(100)); 10];
        let result = ActivityResult::new(ActivityStatus::Completed)
            .with_detail("d".repeat(200))
            .with_content(ActivityContent::Plan { steps });
        let cost = event_bytes(
            &event(EventKind::ActivityCompleted {
                call_id: String::from("call-1"),
                result,
            })
            .await,
        );
        assert!(
            cost >= 1_200,
            "expected a result with 200 bytes of detail and ten 100 byte plan steps to cost at \
             least 1200 | received {cost}"
        );
    }
}
