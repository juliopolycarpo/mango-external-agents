//! The part of reducing a turn that has to remember what it already did.
//!
//! [`crate::reducer`] maps one announcement to events on its own. A few facts need memory across
//! announcements of the same turn:
//!
//! - **Streamed progress for a running item.** Between a command's `item/started` and its
//!   `item/completed`, the app-server writes `item/commandExecution/outputDelta` and nothing else
//!   for it; an MCP call writes `item/mcpToolCall/progress`, and a patch in progress
//!   `item/fileChange/patchUpdated`. Each becomes an [`EventKind::ActivityUpdated`] for an
//!   activity this turn already announced — never for an id the host was not told about — and at
//!   most one per [`ACTIVITY_UPDATE_INTERVAL`], carrying a bounded tail of the output rather than a
//!   build log. The idle deadline is reset by the session for every such frame, emitted or not.
//!
//! - **Answer text the deltas did not carry.** An `agentMessage` item arrives as
//!   `item/agentMessage/delta` frames and again, whole, on `item/completed`. Rendering both would
//!   double the answer, and rendering only the deltas drops a message that was never streamed —
//!   which a resumed conversation can deliver. The completion therefore adds exactly the text its
//!   deltas did not. The streamed text is kept only while a completion could still begin with it;
//!   see [`TurnReducerBuilder::max_message_bytes`].
//!
//! - **The vendor's error code.** The documented failure order is an `error` notification carrying
//!   `codexErrorInfo`, then `turn/completed` with status `failed`. A completion that names no code
//!   of its own keeps the one the last non-retried report named.
//!
//! One [`TurnReducer`] belongs to one turn, and is dropped with it.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use mango_external_agents::Limits;
use mango_external_agents::event::{
    ActivityResult, ActivityStatus, ActivityUpdate, EventKind, StructureClose,
};

use crate::activity;
use crate::protocol::items::{FileUpdateChange, ThreadItem};
use crate::protocol::notifications::Notification;
use crate::reducer::{self, Outcome};

/// How often one running activity may report progress.
///
/// The same five seconds the TypeScript adapter used. The first report after a quiet window is
/// always emitted, so a command that prints slower than this is followed line for line; a noisy one
/// is sampled rather than forwarded chunk for chunk into a bounded transcript.
pub const ACTIVITY_UPDATE_INTERVAL: Duration = Duration::from_secs(5);

/// How many characters of an activity's most recent output one update carries.
pub const ACTIVITY_UPDATE_DETAIL_MAX_CHARS: usize = 2_000;

/// What one turn has announced so far, for the announcements that depend on it.
///
/// # Example
///
/// ```
/// use mango_agent_codex::protocol::Notification;
/// use mango_agent_codex::turn_reducer::TurnReducer;
/// use serde_json::json;
///
/// let mut turn = TurnReducer::new();
/// let delta = Notification::parse(
///     "item/commandExecution/outputDelta",
///     json!({"threadId": "t", "turnId": "u", "itemId": "never-started", "delta": "hi"}),
/// );
/// // An item this turn never announced has no activity to update.
/// let outcome = turn.reduce(
///     &delta,
///     "t",
///     Some("u"),
///     std::time::SystemTime::UNIX_EPOCH,
///     tokio::time::Instant::now(),
/// );
/// assert_eq!(outcome, mango_agent_codex::reducer::Outcome::Ignore);
/// ```
#[derive(Default)]
pub struct TurnReducer {
    activities: HashMap<String, OpenActivity>,
    /// The answer text already emitted per message item, until that item completes.
    streamed: HashMap<String, Streamed>,
    /// The most streamed text kept for one message; `None` keeps all of it.
    max_message_bytes: Option<usize>,
    /// The vendor code of the last error report the server did not mean to retry.
    reported_code: Option<String>,
}

impl std::fmt::Debug for TurnReducer {
    /// What the reducer is holding, by count and size: the answer text streamed so far and each open
    /// activity's latest output are the transcript itself, so neither is printed, and neither are
    /// the call ids that key them or the vendor's error code.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let streamed_bytes = self.streamed.values().map(|streamed| match streamed {
            Streamed::Text(text) => text.len(),
            Streamed::Overflowed => 0,
        });
        formatter
            .debug_struct("TurnReducer")
            .field("open_activities", &self.activities.len())
            .field(
                "activity_tails",
                &crate::redacted::Redacted::sum(
                    self.activities.values().map(|activity| activity.tail.len()),
                ),
            )
            .field("streamed_messages", &self.streamed.len())
            .field(
                "streamed_text",
                &crate::redacted::Redacted::sum(streamed_bytes),
            )
            .field("max_message_bytes", &self.max_message_bytes)
            .field(
                "reported_code",
                &crate::redacted::opt_text(self.reported_code.as_deref()),
            )
            .finish()
    }
}

/// What the deltas of one message item delivered, for the completion to be compared against.
#[derive(Debug)]
enum Streamed {
    /// Every byte delivered so far.
    Text(String),
    /// More was delivered than [`TurnReducerBuilder::max_message_bytes`] allows, so the text was
    /// dropped: no completion the transport can deliver begins with that much text.
    Overflowed,
}

impl Streamed {
    /// Appends one delta, dropping the text once it passes `bound`.
    fn push(&mut self, delta: &str, bound: Option<usize>) {
        let Self::Text(text) = self else {
            return;
        };
        if bound.is_some_and(|bound| text.len().saturating_add(delta.len()) > bound) {
            // Assigning drops the buffer, capacity included; clearing it would keep the memory.
            *self = Self::Overflowed;
            return;
        }
        text.push_str(delta);
    }
}

/// Configures a [`TurnReducer`].
///
/// # Example
///
/// ```
/// use mango_agent_codex::turn_reducer::TurnReducer;
///
/// let turn = TurnReducer::builder()
///     .max_message_bytes(2 * 1024 * 1024)
///     .build();
/// # let _ = turn;
/// ```
#[derive(Clone, Copy, Debug, Default)]
#[must_use]
pub struct TurnReducerBuilder {
    max_message_bytes: Option<usize>,
}

impl TurnReducerBuilder {
    /// The most decoded bytes one completed message can arrive in, and so the most streamed text
    /// worth keeping for it.
    ///
    /// A completed message is delivered inside one inbound frame, and the transport refuses a
    /// frame past its limit, so a message whose deltas already passed that limit can never be the
    /// prefix of a completion. Its text is dropped rather than held until the item completes, and
    /// the completion of such a message is emitted whole, which is what a rewrite does anyway. A
    /// bound below what the transport delivers would emit a fully streamed message twice.
    ///
    /// A session passes the most a decoded stdio line can hold under its `Limits::line`: the raw
    /// line is at most `max_line_bytes`, repairing invalid UTF-8 can triple it, and the repaired
    /// line still has to fit in `max_buffered_bytes`. Without a bound, which is what
    /// [`TurnReducer::new`] builds, a message's streamed text is kept whole until it completes.
    pub fn max_message_bytes(mut self, bytes: usize) -> Self {
        self.max_message_bytes = Some(bytes);
        self
    }

    /// A turn nothing has been announced for yet.
    #[must_use]
    pub fn build(self) -> TurnReducer {
        TurnReducer {
            max_message_bytes: self.max_message_bytes,
            ..TurnReducer::default()
        }
    }
}

/// What the host was told this turn opened and has not seen closed.
///
/// A [`TurnReducer`] answers "was this announcement one to emit"; this answers "what does the host
/// still hold open", which is a different fact. It is fed from what was **published**, not from
/// what was reduced: an event the stream refused because a terminal won the race was never seen by
/// the host, so a close for it would be a completion of a call nobody started.
///
/// At the terminal, [`Self::close_all`] turns whatever is left into the closes a host owes its
/// transcript: one [`EventKind::ActivityCompleted`] per open call, in the order they started, and
/// one [`EventKind::ReasoningEnded`] per open phase. It empties the set as it does, so a second
/// terminal has nothing left to close.
#[derive(Debug, Default)]
pub(crate) struct OpenStructures {
    /// Call ids, oldest first, so the closes come out in a stable order.
    activities: Vec<String>,
    /// Reasoning phases opened and not yet ended.
    reasoning: usize,
}

/// One change to [`OpenStructures`], read off an event before it is handed to the stream.
///
/// Read first because publishing consumes the event, and applied only once the stream accepted it:
/// an event the sink refused, for a payload that fails normalisation for instance, was never seen
/// by the host and opens nothing.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Mark {
    ActivityOpened(String),
    ActivityClosed(String),
    ReasoningOpened,
    ReasoningClosed,
}

impl Mark {
    /// What `event` does to the open set, when it does anything.
    ///
    /// Allocates only for an activity event; text and usage, the bulk of a turn, cost a match.
    pub(crate) fn of(event: &EventKind) -> Option<Self> {
        match event {
            EventKind::ActivityStarted { call_id, .. } => {
                Some(Self::ActivityOpened(call_id.clone()))
            }
            EventKind::ActivityCompleted { call_id, .. } => {
                Some(Self::ActivityClosed(call_id.clone()))
            }
            EventKind::ReasoningStarted => Some(Self::ReasoningOpened),
            EventKind::ReasoningEnded => Some(Self::ReasoningClosed),
            _ => None,
        }
    }
}

impl OpenStructures {
    /// Records one change the host has now been given.
    pub(crate) fn apply(&mut self, mark: Mark) {
        match mark {
            Mark::ActivityOpened(call_id) => {
                if !self.activities.contains(&call_id) {
                    self.activities.push(call_id);
                }
            }
            Mark::ActivityClosed(call_id) => self.activities.retain(|open| *open != call_id),
            Mark::ReasoningOpened => self.reasoning += 1,
            Mark::ReasoningClosed => self.reasoning = self.reasoning.saturating_sub(1),
        }
    }

    /// The closes for everything still open, ending each as `status`; leaves nothing open.
    ///
    /// Empty on the ordinary path, where every activity completed before the turn did.
    pub(crate) fn close_all(&mut self, status: ActivityStatus) -> Vec<StructureClose> {
        let activities = std::mem::take(&mut self.activities);
        let reasoning = std::mem::take(&mut self.reasoning);
        let mut closes = Vec::with_capacity(activities.len() + reasoning);
        closes.extend(
            activities
                .into_iter()
                .map(|call_id| StructureClose::activity(call_id, ActivityResult::new(status))),
        );
        closes.extend(std::iter::repeat_n(StructureClose::reasoning(), reasoning));
        closes
    }
}

/// One activity the host was told about and has not seen complete.
#[derive(Debug, Default)]
struct OpenActivity {
    /// The most recent output, at most [`ACTIVITY_UPDATE_DETAIL_MAX_CHARS`] characters.
    tail: String,
    /// Whether `tail` has dropped anything off its front.
    truncated: bool,
    /// When this activity last produced an update.
    last_update: Option<tokio::time::Instant>,
}

/// What one progress frame says about its activity.
enum Progress<'a> {
    /// More output, in order after the last.
    Output(String),
    /// Every file the patch touches, as it stands now.
    Patch(&'a [FileUpdateChange]),
}

impl TurnReducer {
    /// A turn nothing has been announced for yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Configures a reducer; see [`TurnReducerBuilder`].
    pub fn builder() -> TurnReducerBuilder {
        TurnReducerBuilder::default()
    }

    /// The reducer for a session that reads its connection under `limits`.
    ///
    /// This harness reads its app-server over stdio, where a message reaches the reducer decoded,
    /// in one line: at most `max_line_bytes` as the vendor wrote it, at most three times that once
    /// each invalid byte is repaired to a 3-byte U+FFFD, and never more than `max_buffered_bytes`,
    /// which the repaired line is counted against.
    ///
    /// The bound equals the largest decoded stdio line only because this harness is stdio-only. On
    /// a websocket the limit applies to the decoded message against `max_line_bytes` alone (see
    /// `SocketReceiver::bounded` in the websocket transport), so a websocket transport would need
    /// its own bound rather than this one.
    pub(crate) fn for_limits(limits: &Limits) -> Self {
        let line = &limits.line;
        Self::builder()
            .max_message_bytes(
                line.max_line_bytes
                    .saturating_mul(3)
                    .min(line.max_buffered_bytes),
            )
            .build()
    }

    /// Reduces one announcement for the active turn, remembering what later ones depend on.
    ///
    /// The routing is [`reducer::reduce_for_active_turn`]'s: an announcement for another
    /// conversation or another turn is [`Outcome::Ignore`] here too. `observed_at` stamps a quota
    /// snapshot; `instant` is the monotonic clock progress sampling is measured on.
    pub fn reduce(
        &mut self,
        notification: &Notification,
        thread_id: &str,
        active_native_turn_id: Option<&str>,
        observed_at: SystemTime,
        instant: tokio::time::Instant,
    ) -> Outcome {
        let progress = match notification {
            Notification::CommandOutputDelta(delta) => {
                Some((&delta.item_id, Progress::Output(delta.delta.clone())))
            }
            Notification::McpToolCallProgress(progress) => Some((
                &progress.item_id,
                Progress::Output(if progress.message.is_empty() {
                    String::new()
                } else {
                    format!("{}\n", progress.message)
                }),
            )),
            Notification::FileChangePatchUpdated(update) => {
                Some((&update.item_id, Progress::Patch(&update.changes)))
            }
            _ => None,
        };
        if let Some((item_id, progress)) = progress {
            if !reducer::routes_to_active_turn(notification, thread_id, active_native_turn_id) {
                return Outcome::Ignore;
            }
            return self.progress(item_id, progress, instant);
        }
        if let Notification::ItemCompleted(completed) = notification
            && let ThreadItem::AgentMessage { id, text } = &completed.item
        {
            if !reducer::routes_to_active_turn(notification, thread_id, active_native_turn_id) {
                return Outcome::Ignore;
            }
            return self.completed_message(id, text);
        }

        let mut outcome = reducer::reduce_for_active_turn(
            notification,
            thread_id,
            active_native_turn_id,
            observed_at,
        );
        if let Notification::Error(report) = notification
            && !report.will_retry
            && reducer::routes_to_active_turn(notification, thread_id, active_native_turn_id)
            && let Some(code) = report.error.vendor_code()
        {
            self.reported_code = Some(code);
        }
        if let Outcome::Finish {
            failure: Some(failure),
            ..
        } = &mut outcome
            && failure.vendor_code.is_none()
            && let Some(code) = self.reported_code.take()
        {
            *failure = failure.clone().with_vendor_code(code, false);
        }
        if let (Notification::AgentMessageDelta(delta), Outcome::Emit(_)) = (notification, &outcome)
        {
            self.remember_streamed(&delta.item_id, &delta.delta);
        }
        self.observe(&outcome);
        outcome
    }

    /// What a completed message adds to the text its deltas already delivered.
    ///
    /// When the completed text does not begin with what was streamed, the vendor rewrote the
    /// message and the whole of it is emitted, as the TypeScript adapter did: a correction is
    /// worth more than avoiding a repeat.
    fn completed_message(&mut self, item_id: &str, text: &str) -> Outcome {
        let remainder = match self.streamed.remove(item_id) {
            Some(Streamed::Text(streamed)) => text.strip_prefix(streamed.as_str()).unwrap_or(text),
            Some(Streamed::Overflowed) | None => text,
        };
        if remainder.is_empty() {
            return Outcome::Ignore;
        }
        Outcome::Emit(vec![EventKind::TextDelta {
            text: remainder.to_owned(),
        }])
    }

    /// Adds one emitted delta to the text its message item has delivered.
    fn remember_streamed(&mut self, item_id: &str, delta: &str) {
        let bound = self.max_message_bytes;
        if let Some(streamed) = self.streamed.get_mut(item_id) {
            streamed.push(delta, bound);
            return;
        }
        let mut streamed = Streamed::Text(String::new());
        streamed.push(delta, bound);
        self.streamed.insert(item_id.to_owned(), streamed);
    }

    /// The capacity of every streamed text still kept, for tests of what is retained.
    #[cfg(test)]
    fn retained_text_capacity(&self) -> usize {
        self.streamed
            .values()
            .map(|streamed| match streamed {
                Streamed::Text(text) => text.capacity(),
                Streamed::Overflowed => 0,
            })
            .sum()
    }

    /// Tracks which activities are open, from what the pure reducer decided to emit.
    fn observe(&mut self, outcome: &Outcome) {
        let Outcome::Emit(events) = outcome else {
            if matches!(outcome, Outcome::Finish { .. } | Outcome::Poison { .. }) {
                self.activities.clear();
            }
            return;
        };
        for event in events {
            match event {
                EventKind::ActivityStarted { call_id, .. } => {
                    self.activities
                        .insert(call_id.clone(), OpenActivity::default());
                }
                EventKind::ActivityCompleted { call_id, .. } => {
                    self.activities.remove(call_id);
                }
                _ => {}
            }
        }
    }

    /// Folds one progress frame into its activity and emits an update when the window allows.
    fn progress(
        &mut self,
        item_id: &str,
        progress: Progress<'_>,
        instant: tokio::time::Instant,
    ) -> Outcome {
        // An item this turn never announced has no call id a host could attach an update to.
        let Some(open) = self.activities.get_mut(item_id) else {
            return Outcome::Ignore;
        };
        // The window is checked before an update is built: a held update would otherwise copy a
        // whole patch, or the output tail, only to drop it. The output is still folded in first,
        // so what was held back arrives with the next update.
        let update = match progress {
            Progress::Output(chunk) if chunk.is_empty() => return Outcome::Ignore,
            Progress::Output(chunk) => {
                open.append(&chunk);
                if open.holds_back(instant) {
                    return Outcome::Ignore;
                }
                let mut update = ActivityUpdate::new().with_detail(open.tail.clone());
                update.truncated = open.truncated;
                update
            }
            Progress::Patch([]) => return Outcome::Ignore,
            Progress::Patch(changes) => {
                if open.holds_back(instant) {
                    return Outcome::Ignore;
                }
                let update = ActivityUpdate::new();
                let update = match activity::file_change_detail(changes) {
                    Some(detail) => update.with_detail(detail),
                    None => update,
                };
                match activity::file_change_content(changes) {
                    Some(content) => update.with_content(content),
                    None => update,
                }
            }
        };
        open.last_update = Some(instant);
        Outcome::Emit(vec![EventKind::ActivityUpdated {
            call_id: item_id.to_owned(),
            update,
        }])
    }
}

/// Where the last [`ACTIVITY_UPDATE_DETAIL_MAX_CHARS`] characters of `text` begin, or `None` when
/// it holds fewer.
///
/// Walks back from the end and stops after that many characters, so a long chunk costs the tail's
/// length rather than its own.
fn last_window_start(text: &str) -> Option<usize> {
    text.char_indices()
        .rev()
        .nth(ACTIVITY_UPDATE_DETAIL_MAX_CHARS - 1)
        .map(|(index, _)| index)
}

impl OpenActivity {
    /// Whether an update at `instant` falls inside the window of the one last emitted.
    ///
    /// The window opens when an update is emitted and only then, so a held update never pushes it
    /// back.
    fn holds_back(&self, instant: tokio::time::Instant) -> bool {
        self.last_update
            .is_some_and(|last| instant.saturating_duration_since(last) < ACTIVITY_UPDATE_INTERVAL)
    }

    /// Appends output, keeping only the most recent characters.
    ///
    /// A chunk that is at most [`ACTIVITY_UPDATE_DETAIL_MAX_CHARS`] bytes cannot alone fill the
    /// tail, so it takes the ordinary path: append, count, cut what overflows. Only a longer chunk
    /// looks for its own last characters, and keeps just those. Without that, one large delta left
    /// its whole size behind as the tail's capacity until the activity completed.
    ///
    /// The chunk is cut here only to bound what one activity retains, and the result is the same
    /// tail and flag either path produces. The tail is never credential-redacted on its way out,
    /// so nothing is cut ahead of a redaction.
    fn append(&mut self, chunk: &str) {
        if chunk.len() > ACTIVITY_UPDATE_DETAIL_MAX_CHARS
            && let Some(start) = last_window_start(chunk)
        {
            // The chunk alone holds a whole tail. What came before it, and what this chunk drops
            // of its own front, are both gone.
            self.truncated |= start > 0 || !self.tail.is_empty();
            // Assigned, not cleared, so the old buffer's capacity goes with it.
            self.tail = chunk[start..].to_owned();
            return;
        }
        self.tail.push_str(chunk);
        let length = self.tail.chars().count();
        if length <= ACTIVITY_UPDATE_DETAIL_MAX_CHARS {
            return;
        }
        let cut = self
            .tail
            .char_indices()
            .nth(length - ACTIVITY_UPDATE_DETAIL_MAX_CHARS)
            .map_or(0, |(index, _)| index);
        self.tail.drain(..cut);
        self.truncated = true;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ACTIVITY_UPDATE_DETAIL_MAX_CHARS, ACTIVITY_UPDATE_INTERVAL, Mark, OpenActivity,
        OpenStructures, TurnReducer,
    };
    use crate::protocol::notifications::{Notification, method};
    use crate::reducer::Outcome;
    use mango_external_agents::content::ActivityContent;
    use mango_external_agents::event::{
        ActivityResult, ActivityStatus, ActivityUpdate, EventKind, StructureClose,
    };
    use serde_json::json;
    use std::time::{Duration, SystemTime};

    const THREAD: &str = "thread-1";
    const TURN: &str = "turn-1";

    /// A turn with one running command, announced the way the app-server does.
    fn running(item_type: &str, item_id: &str) -> (TurnReducer, tokio::time::Instant) {
        let mut turn = TurnReducer::new();
        let start = tokio::time::Instant::now();
        let item = match item_type {
            "mcpToolCall" => json!({"type": "mcpToolCall", "id": item_id, "server": "exa",
                                    "tool": "search", "status": "inProgress"}),
            "fileChange" => json!({"type": "fileChange", "id": item_id, "changes": [],
                                   "status": "inProgress"}),
            _ => json!({"type": "commandExecution", "id": item_id, "command": "cargo build",
                        "status": "inProgress"}),
        };
        let started = turn.reduce(
            &Notification::parse(
                method::ITEM_STARTED,
                json!({"threadId": THREAD, "turnId": TURN, "item": item}),
            ),
            THREAD,
            Some(TURN),
            SystemTime::UNIX_EPOCH,
            start,
        );
        assert!(
            matches!(&started, Outcome::Emit(events)
                if matches!(events.as_slice(), [EventKind::ActivityStarted { .. }])),
            "expected the item to open an activity, received {started:?}"
        );
        (turn, start)
    }

    fn output(item_id: &str, delta: &str) -> Notification {
        Notification::parse(
            method::COMMAND_EXECUTION_OUTPUT_DELTA,
            json!({"threadId": THREAD, "turnId": TURN, "itemId": item_id, "delta": delta}),
        )
    }

    fn reduce_at(
        turn: &mut TurnReducer,
        notification: &Notification,
        at: tokio::time::Instant,
    ) -> Outcome {
        turn.reduce(notification, THREAD, Some(TURN), SystemTime::UNIX_EPOCH, at)
    }

    fn update_of(outcome: &Outcome) -> &ActivityUpdate {
        match outcome {
            Outcome::Emit(events) => match events.as_slice() {
                [EventKind::ActivityUpdated { update, .. }] => update,
                other => panic!("expected one activity update, received {other:?}"),
            },
            other => panic!("expected an emitted activity update, received {other:?}"),
        }
    }

    #[test]
    fn a_running_commands_first_output_is_an_update_to_its_own_activity() {
        let (mut turn, start) = running("commandExecution", "cmd-1");
        let outcome = reduce_at(&mut turn, &output("cmd-1", "compiling\n"), start);
        let Outcome::Emit(events) = &outcome else {
            panic!("expected an update, received {outcome:?}");
        };
        assert!(
            matches!(events.as_slice(), [EventKind::ActivityUpdated { call_id, .. }] if call_id == "cmd-1"),
            "expected the update to address the command's call id, received {events:?}"
        );
        assert_eq!(update_of(&outcome).detail.as_deref(), Some("compiling\n"));
    }

    /// A chatty build is sampled, not forwarded delta for delta; what was held back arrives with
    /// the next update rather than being lost.
    #[test]
    fn output_inside_the_window_is_held_and_carried_by_the_next_update() {
        let (mut turn, start) = running("commandExecution", "cmd-1");
        let _ = reduce_at(&mut turn, &output("cmd-1", "a\n"), start);
        let held = reduce_at(
            &mut turn,
            &output("cmd-1", "b\n"),
            start + Duration::from_secs(1),
        );
        assert_eq!(
            held,
            Outcome::Ignore,
            "expected output inside the window to be held"
        );
        let next = reduce_at(
            &mut turn,
            &output("cmd-1", "c\n"),
            start + ACTIVITY_UPDATE_INTERVAL,
        );
        assert_eq!(update_of(&next).detail.as_deref(), Some("a\nb\nc\n"));
    }

    #[test]
    fn an_updates_detail_is_the_bounded_tail_of_the_output() {
        let (mut turn, start) = running("commandExecution", "cmd-1");
        let long = "x".repeat(ACTIVITY_UPDATE_DETAIL_MAX_CHARS) + "tail-é";
        let outcome = reduce_at(&mut turn, &output("cmd-1", &long), start);
        let update = update_of(&outcome);
        let detail = update.detail.as_deref().unwrap_or_default();
        assert_eq!(detail.chars().count(), ACTIVITY_UPDATE_DETAIL_MAX_CHARS);
        assert!(
            detail.ends_with("tail-é"),
            "expected the most recent output, received a detail ending {:?}",
            detail.chars().rev().take(8).collect::<String>()
        );
        assert!(update.truncated, "expected the cut to be reported");
    }

    /// An update needs an activity the host was told about; inventing one would leave a part the
    /// host never sees complete.
    #[test]
    fn output_for_an_item_never_announced_or_already_completed_is_not_an_update() {
        let (mut turn, start) = running("commandExecution", "cmd-1");
        assert_eq!(
            reduce_at(&mut turn, &output("someone-else", "x"), start),
            Outcome::Ignore
        );
        let _ = reduce_at(
            &mut turn,
            &Notification::parse(
                method::ITEM_COMPLETED,
                json!({"threadId": THREAD, "turnId": TURN, "item": {
                    "type": "commandExecution", "id": "cmd-1", "command": "cargo build",
                    "status": "completed", "exitCode": 0}}),
            ),
            start,
        );
        assert_eq!(
            reduce_at(
                &mut turn,
                &output("cmd-1", "late"),
                start + ACTIVITY_UPDATE_INTERVAL
            ),
            Outcome::Ignore
        );
    }

    #[test]
    fn output_for_another_turn_or_conversation_is_not_this_turns_update() {
        let (mut turn, start) = running("commandExecution", "cmd-1");
        let foreign_turn = Notification::parse(
            method::COMMAND_EXECUTION_OUTPUT_DELTA,
            json!({"threadId": THREAD, "turnId": "older", "itemId": "cmd-1", "delta": "x"}),
        );
        let foreign_thread = Notification::parse(
            method::COMMAND_EXECUTION_OUTPUT_DELTA,
            json!({"threadId": "subagent", "turnId": TURN, "itemId": "cmd-1", "delta": "x"}),
        );
        assert_eq!(reduce_at(&mut turn, &foreign_turn, start), Outcome::Ignore);
        assert_eq!(
            reduce_at(&mut turn, &foreign_thread, start),
            Outcome::Ignore
        );
    }

    #[test]
    fn mcp_progress_messages_accumulate_one_per_line() {
        let (mut turn, start) = running("mcpToolCall", "mcp-1");
        let outcome = reduce_at(
            &mut turn,
            &Notification::parse(
                method::MCP_TOOL_CALL_PROGRESS,
                json!({"threadId": THREAD, "turnId": TURN, "itemId": "mcp-1",
                       "message": "fetched page 1"}),
            ),
            start,
        );
        assert_eq!(
            update_of(&outcome).detail.as_deref(),
            Some("fetched page 1\n")
        );
    }

    /// A patch update is the whole patch as it stands, so it replaces rather than appends, and
    /// its files stay a diff rather than becoming a paragraph.
    #[test]
    fn a_patch_update_carries_the_files_it_touches_as_structure() {
        let (mut turn, start) = running("fileChange", "patch-1");
        let outcome = reduce_at(
            &mut turn,
            &Notification::parse(
                method::FILE_CHANGE_PATCH_UPDATED,
                json!({"threadId": THREAD, "turnId": TURN, "itemId": "patch-1", "changes": [
                    {"path": "src/lib.rs", "kind": {"type": "update"}, "diff": "@@ -1 +1 @@\n"},
                    {"path": "README.md", "kind": {"type": "add"}, "diff": "+hello\n"}]}),
            ),
            start,
        );
        let update = update_of(&outcome);
        assert_eq!(update.detail.as_deref(), Some("src/lib.rs\nREADME.md"));
        assert!(
            matches!(&update.content, Some(ActivityContent::Diff { files }) if files.len() == 2),
            "expected both files as a diff, received {:?}",
            update.content
        );
    }

    fn patch(item_id: &str, diff: &str) -> Notification {
        Notification::parse(
            method::FILE_CHANGE_PATCH_UPDATED,
            json!({"threadId": THREAD, "turnId": TURN, "itemId": item_id, "changes": [
                {"path": "src/lib.rs", "kind": {"type": "update"}, "diff": diff}]}),
        )
    }

    fn diff_of(update: &ActivityUpdate) -> &str {
        match &update.content {
            Some(ActivityContent::Diff { files }) => match files.as_slice() {
                [file] => file.unified_diff.as_deref().unwrap_or_default(),
                other => panic!("expected one file in the update, received {other:?}"),
            },
            other => panic!("expected the update to carry a diff, received {other:?}"),
        }
    }

    /// A patch is sampled like output: what lands inside the window is dropped, and the next
    /// update outside it carries the patch as it stands then, not an older one.
    #[test]
    fn a_patch_update_inside_the_window_is_held_and_the_next_carries_the_latest_patch() {
        let (mut turn, start) = running("fileChange", "patch-1");
        let first = reduce_at(&mut turn, &patch("patch-1", "+first\n"), start);
        assert_eq!(diff_of(update_of(&first)), "+first\n");
        let held = reduce_at(
            &mut turn,
            &patch("patch-1", "+second\n"),
            start + Duration::from_secs(1),
        );
        assert_eq!(
            held,
            Outcome::Ignore,
            "expected a patch update inside the window to be held"
        );
        let next = reduce_at(
            &mut turn,
            &patch("patch-1", "+third\n"),
            start + ACTIVITY_UPDATE_INTERVAL,
        );
        assert_eq!(diff_of(update_of(&next)), "+third\n");
    }

    /// The window opens when an update is emitted, so a held one must not push it back.
    #[test]
    fn a_held_patch_update_does_not_restart_the_window() {
        let (mut turn, start) = running("fileChange", "patch-1");
        let _ = reduce_at(&mut turn, &patch("patch-1", "+a\n"), start);
        let just_inside = start + ACTIVITY_UPDATE_INTERVAL - Duration::from_millis(1);
        assert_eq!(
            reduce_at(&mut turn, &patch("patch-1", "+b\n"), just_inside),
            Outcome::Ignore,
            "expected an update a millisecond before the window closes to be held"
        );
        let at_close = reduce_at(
            &mut turn,
            &patch("patch-1", "+c\n"),
            start + ACTIVITY_UPDATE_INTERVAL,
        );
        assert_eq!(
            diff_of(update_of(&at_close)),
            "+c\n",
            "expected the window to close ACTIVITY_UPDATE_INTERVAL after the emitted update"
        );
    }

    fn message_delta(item_id: &str, delta: &str) -> Notification {
        Notification::parse(
            method::AGENT_MESSAGE_DELTA,
            json!({"threadId": THREAD, "turnId": TURN, "itemId": item_id, "delta": delta}),
        )
    }

    fn message_completed(item_id: &str, text: &str) -> Notification {
        Notification::parse(
            method::ITEM_COMPLETED,
            json!({"threadId": THREAD, "turnId": TURN,
                   "item": {"type": "agentMessage", "id": item_id, "text": text}}),
        )
    }

    fn text_of(outcome: &Outcome) -> String {
        match outcome {
            Outcome::Emit(events) => events
                .iter()
                .filter_map(|event| match event {
                    EventKind::TextDelta { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect(),
            _ => String::new(),
        }
    }

    #[test]
    fn a_message_that_was_never_streamed_is_emitted_whole_on_completion() {
        let mut turn = TurnReducer::new();
        let at = tokio::time::Instant::now();
        let outcome = reduce_at(&mut turn, &message_completed("m", "whole answer"), at);
        assert_eq!(text_of(&outcome), "whole answer");
    }

    #[test]
    fn a_completion_adds_only_the_text_its_deltas_left_out() {
        let mut turn = TurnReducer::new();
        let at = tokio::time::Instant::now();
        let streamed = [
            reduce_at(&mut turn, &message_delta("m", "who"), at),
            reduce_at(&mut turn, &message_delta("m", "le "), at),
        ];
        let completed = reduce_at(&mut turn, &message_completed("m", "whole answer"), at);
        assert_eq!(
            streamed.iter().map(text_of).collect::<String>() + &text_of(&completed),
            "whole answer"
        );
        assert_eq!(
            reduce_at(&mut turn, &message_completed("m2", ""), at),
            Outcome::Ignore,
            "expected an empty message to add nothing"
        );
    }

    #[test]
    fn a_fully_streamed_message_adds_nothing_on_completion() {
        let mut turn = TurnReducer::new();
        let at = tokio::time::Instant::now();
        let _ = reduce_at(&mut turn, &message_delta("m", "done"), at);
        assert_eq!(
            reduce_at(&mut turn, &message_completed("m", "done"), at),
            Outcome::Ignore
        );
    }

    /// A turn whose messages keep at most `bound` streamed bytes each.
    fn bounded(bound: usize) -> TurnReducer {
        TurnReducer::builder().max_message_bytes(bound).build()
    }

    fn stream(turn: &mut TurnReducer, item_id: &str, deltas: &[&str]) {
        let at = tokio::time::Instant::now();
        for delta in deltas {
            let outcome = reduce_at(turn, &message_delta(item_id, delta), at);
            assert_eq!(
                text_of(&outcome),
                *delta,
                "expected the delta to reach the host whatever is kept for the completion"
            );
        }
    }

    #[test]
    fn streamed_text_past_the_bound_is_not_kept_for_the_completion() {
        let mut turn = bounded(8);
        stream(&mut turn, "m", &["1234", "5678"]);
        assert!(
            turn.retained_text_capacity() >= 8,
            "expected the text within the bound to be kept, received {} bytes of capacity",
            turn.retained_text_capacity()
        );
        stream(
            &mut turn,
            "m",
            &["9", "more text the completion can never repeat"],
        );
        assert_eq!(
            turn.retained_text_capacity(),
            0,
            "expected no text kept past the bound of 8 bytes, received {} bytes of capacity",
            turn.retained_text_capacity()
        );
    }

    #[test]
    fn a_message_of_exactly_the_bound_is_still_deduplicated() {
        let at = tokio::time::Instant::now();
        let mut turn = bounded(8);
        stream(&mut turn, "m", &["1234", "5678"]);
        assert_eq!(
            reduce_at(&mut turn, &message_completed("m", "12345678"), at),
            Outcome::Ignore,
            "expected a message of exactly the bound to add nothing on completion"
        );

        stream(&mut turn, "n", &["1234", "5678"]);
        let completed = reduce_at(&mut turn, &message_completed("n", "12345678 tail"), at);
        assert_eq!(
            text_of(&completed),
            " tail",
            "expected only the text the deltas left out"
        );
    }

    /// The vendor may correct a message it already streamed, and the correction can be shorter
    /// than what the deltas delivered. The completion is the whole message either way.
    #[test]
    fn a_rewrite_shorter_than_what_streamed_is_emitted_whole_past_the_bound() {
        let at = tokio::time::Instant::now();
        for bound in [4, 4096] {
            let mut turn = bounded(bound);
            stream(
                &mut turn,
                "m",
                &["a long first draft, ", "that is then rewritten"],
            );
            let completed = reduce_at(&mut turn, &message_completed("m", "rewritten"), at);
            assert_eq!(
                text_of(&completed),
                "rewritten",
                "expected the rewrite whole with a bound of {bound}"
            );
        }
    }

    /// A transport that delivers larger frames has to be able to deduplicate larger messages, so
    /// the bound is what the builder was given rather than a constant.
    #[test]
    fn a_message_larger_than_the_default_line_limit_is_deduplicated_under_a_larger_bound() {
        let at = tokio::time::Instant::now();
        let two_mib = "x".repeat(2 * 1024 * 1024);
        let mut turn = bounded(4 * 1024 * 1024);
        stream(&mut turn, "m", &[two_mib.as_str()]);
        assert_eq!(
            reduce_at(&mut turn, &message_completed("m", &two_mib), at),
            Outcome::Ignore,
            "expected a fully streamed message under a 4 MiB bound to add nothing"
        );
    }

    /// What a session keeps is what its connection can deliver, not a constant: a message that
    /// fits the host's limits and fully streamed must not be emitted a second time.
    #[test]
    fn a_session_reducer_keeps_what_its_limits_can_deliver() {
        use mango_external_agents::{Limits, LineLimits};

        let at = tokio::time::Instant::now();
        let line = |max_line_bytes, max_buffered_bytes| Limits {
            line: LineLimits {
                max_line_bytes,
                max_buffered_bytes,
            },
            ..Limits::default()
        };
        for (limits, message_bytes) in [
            (Limits::default(), 2 * 1024 * 1024),
            // Repairing invalid UTF-8 triples a raw line, within the buffered budget.
            (line(1024 * 1024, 8 * 1024 * 1024), 3 * 1024 * 1024),
            (line(4 * 1024 * 1024, 3 * 1024 * 1024), 3 * 1024 * 1024),
            (line(1024, 3 * 1024 * 1024), 3 * 1024),
        ] {
            let message = "x".repeat(message_bytes);
            let mut turn = TurnReducer::for_limits(&limits);
            stream(&mut turn, "m", &[message.as_str()]);
            assert_eq!(
                reduce_at(&mut turn, &message_completed("m", &message), at),
                Outcome::Ignore,
                "expected a {message_bytes}-byte message to be deduplicated under {limits:?}"
            );
        }

        // Nothing longer than the buffered budget can be read, whatever the line limit says.
        for limits in [line(16, 16), line(4 * 1024 * 1024, 16)] {
            let mut turn = TurnReducer::for_limits(&limits);
            stream(&mut turn, "m", &["seventeen bytes!!"]);
            assert_eq!(
                turn.retained_text_capacity(),
                0,
                "expected no text kept past the 16 bytes {limits:?} can deliver"
            );
        }
    }

    #[test]
    fn a_reducer_without_a_bound_keeps_every_message_whole() {
        let at = tokio::time::Instant::now();
        let mut turn = TurnReducer::new();
        stream(&mut turn, "m", &["a message ", "of any length"]);
        assert_eq!(
            reduce_at(
                &mut turn,
                &message_completed("m", "a message of any length"),
                at
            ),
            Outcome::Ignore
        );
    }

    #[test]
    fn every_message_item_has_its_own_bound() {
        let at = tokio::time::Instant::now();
        let mut turn = bounded(8);
        stream(&mut turn, "long", &["0123456789"]);
        stream(&mut turn, "short", &["12", "34"]);
        assert_eq!(
            reduce_at(&mut turn, &message_completed("short", "1234"), at),
            Outcome::Ignore,
            "expected the short message to be deduplicated beside an overflowed one"
        );
    }

    #[test]
    fn another_turns_completed_message_is_not_this_turns_answer() {
        let mut turn = TurnReducer::new();
        let foreign = Notification::parse(
            method::ITEM_COMPLETED,
            json!({"threadId": THREAD, "turnId": "older",
                   "item": {"type": "agentMessage", "id": "m", "text": "late"}}),
        );
        assert_eq!(
            reduce_at(&mut turn, &foreign, tokio::time::Instant::now()),
            Outcome::Ignore
        );
    }

    /// The documented order is an `error` notification carrying `codexErrorInfo`, then a failed
    /// `turn/completed`. A completion without a code of its own keeps the one the report named.
    #[test]
    fn a_failed_completion_without_a_code_keeps_the_one_its_error_report_named() {
        let mut turn = TurnReducer::new();
        let at = tokio::time::Instant::now();
        let reported = Notification::parse(
            method::ERROR,
            json!({"threadId": THREAD, "turnId": TURN, "willRetry": false,
                   "error": {"message": "limit", "codexErrorInfo": "usageLimitExceeded"}}),
        );
        assert_eq!(reduce_at(&mut turn, &reported, at), Outcome::Ignore);
        let completed = Notification::parse(
            method::TURN_COMPLETED,
            json!({"threadId": THREAD, "turn": {"id": TURN, "status": "failed",
                   "error": {"message": "limit"}}}),
        );
        let Outcome::Finish { failure, .. } = reduce_at(&mut turn, &completed, at) else {
            panic!("expected the turn to end");
        };
        assert_eq!(
            failure.and_then(|failure| failure.vendor_code).as_deref(),
            Some("usageLimitExceeded")
        );
    }

    /// A report the server means to retry says nothing about how the turn ends.
    #[test]
    fn a_retried_error_report_does_not_name_the_turns_failure() {
        let mut turn = TurnReducer::new();
        let at = tokio::time::Instant::now();
        let reported = Notification::parse(
            method::ERROR,
            json!({"threadId": THREAD, "turnId": TURN, "willRetry": true,
                   "error": {"message": "busy", "codexErrorInfo": "serverOverloaded"}}),
        );
        let _ = reduce_at(&mut turn, &reported, at);
        let completed = Notification::parse(
            method::TURN_COMPLETED,
            json!({"threadId": THREAD, "turn": {"id": TURN, "status": "failed",
                   "error": {"message": "gave up"}}}),
        );
        let Outcome::Finish { failure, .. } = reduce_at(&mut turn, &completed, at) else {
            panic!("expected the turn to end");
        };
        assert_eq!(failure.and_then(|failure| failure.vendor_code), None);
    }

    #[test]
    fn empty_progress_is_not_an_update() {
        let (mut turn, start) = running("commandExecution", "cmd-1");
        assert_eq!(
            reduce_at(&mut turn, &output("cmd-1", ""), start),
            Outcome::Ignore
        );
    }

    fn activity_started(call_id: &str) -> EventKind {
        EventKind::ActivityStarted {
            call_id: call_id.to_owned(),
            activity: mango_external_agents::event::Activity::new(
                "command",
                mango_external_agents::event::ActivityKind::Command,
                "cargo build",
            ),
        }
    }

    fn open_after(events: &[EventKind]) -> OpenStructures {
        let mut open = OpenStructures::default();
        for event in events {
            if let Some(mark) = Mark::of(event) {
                open.apply(mark);
            }
        }
        open
    }

    #[test]
    fn a_mark_is_read_only_from_activity_and_reasoning_events() {
        assert_eq!(
            Mark::of(&activity_started("c1")),
            Some(Mark::ActivityOpened(String::from("c1")))
        );
        assert_eq!(
            Mark::of(&EventKind::ReasoningEnded),
            Some(Mark::ReasoningClosed)
        );
        assert_eq!(
            Mark::of(&EventKind::TextDelta {
                text: String::from("hi")
            }),
            None,
            "expected text to open nothing"
        );
    }

    #[test]
    fn close_all_closes_activities_oldest_first_then_reasoning_and_only_once() {
        let mut open = open_after(&[
            EventKind::ReasoningStarted,
            activity_started("b"),
            activity_started("a"),
        ]);
        let closes = open.close_all(ActivityStatus::Failed);
        let names: Vec<String> = closes
            .iter()
            .map(|close| match close {
                StructureClose::Activity { call_id, result } => {
                    format!("{call_id}:{:?}", result.status)
                }
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            names,
            ["b:Failed", "a:Failed", "Reasoning"],
            "expected closes in start order, reasoning last | received {names:?}"
        );
        assert_eq!(
            open.close_all(ActivityStatus::Failed),
            Vec::<StructureClose>::new(),
            "expected a second settle to find nothing open"
        );
    }

    #[test]
    fn close_all_skips_what_the_vendor_already_closed() {
        let mut open = open_after(&[
            EventKind::ReasoningStarted,
            activity_started("a"),
            EventKind::ReasoningEnded,
            EventKind::ActivityCompleted {
                call_id: String::from("a"),
                result: ActivityResult::new(ActivityStatus::Completed),
            },
        ]);
        assert_eq!(
            open.close_all(ActivityStatus::Cancelled),
            Vec::<StructureClose>::new()
        );
    }

    #[test]
    fn a_reasoning_end_without_a_start_does_not_go_below_zero() {
        let mut open = open_after(&[EventKind::ReasoningEnded, EventKind::ReasoningStarted]);
        assert_eq!(
            open.close_all(ActivityStatus::Cancelled),
            vec![StructureClose::reasoning()],
            "expected the one real start to be closed once"
        );
    }

    /// The tail exactly as it was kept before an oversized chunk got a path of its own, so the
    /// tests can hold the new code to the same output rather than to a description of it.
    #[derive(Default)]
    struct ReferenceTail {
        tail: String,
        truncated: bool,
    }

    impl ReferenceTail {
        fn append(&mut self, chunk: &str) {
            self.tail.push_str(chunk);
            let length = self.tail.chars().count();
            if length <= ACTIVITY_UPDATE_DETAIL_MAX_CHARS {
                return;
            }
            let cut = self
                .tail
                .char_indices()
                .nth(length - ACTIVITY_UPDATE_DETAIL_MAX_CHARS)
                .map_or(0, |(index, _)| index);
            self.tail.drain(..cut);
            self.truncated = true;
        }
    }

    /// Feeds both tails the same chunks and fails at the first chunk where they differ.
    fn assert_same_tail(label: &str, chunks: &[String]) {
        let mut open = OpenActivity::default();
        let mut reference = ReferenceTail::default();
        for (index, chunk) in chunks.iter().enumerate() {
            open.append(chunk);
            reference.append(chunk);
            assert_eq!(
                (open.tail.as_str(), open.truncated),
                (reference.tail.as_str(), reference.truncated),
                "expected the reference tail after chunk {index} of {label} ({} bytes, {} chars) | received a different tail or flag",
                chunk.len(),
                chunk.chars().count()
            );
        }
    }

    /// `count` characters of `unit`, so a chunk's length is exact in characters, not bytes.
    fn of(unit: char, count: usize) -> String {
        std::iter::repeat_n(unit, count).collect()
    }

    /// Distinct characters, so a tail cut in the wrong place cannot match by accident.
    fn numbered(start: usize, count: usize) -> String {
        (start..start + count)
            .map(|n| char::from_u32(0x4E00 + (n % 2000) as u32).unwrap_or('?'))
            .collect()
    }

    const UNITS: [char; 4] = ['a', 'é', '€', '😀'];
    const LIMIT: usize = ACTIVITY_UPDATE_DETAIL_MAX_CHARS;

    #[test]
    fn a_chunk_straddling_the_tail_limit_gives_the_reference_tail_for_every_width() {
        for unit in UNITS {
            for chars in [
                0,
                1,
                LIMIT - 1,
                LIMIT,
                LIMIT + 1,
                2 * LIMIT - 1,
                2 * LIMIT,
                2 * LIMIT + 1,
            ] {
                assert_same_tail(
                    &format!("a lone {chars}-char chunk of {unit:?}"),
                    &[of(unit, chars)],
                );
                assert_same_tail(
                    &format!("a short chunk then a {chars}-char chunk of {unit:?}"),
                    &[of('x', 7), of(unit, chars)],
                );
                assert_same_tail(
                    &format!("a full tail then a {chars}-char chunk of {unit:?}"),
                    &[of('y', LIMIT), of(unit, chars)],
                );
            }
        }
    }

    #[test]
    fn a_chunk_of_exactly_the_limit_only_truncates_when_something_came_before() {
        let mut fresh = OpenActivity::default();
        fresh.append(&of('a', LIMIT));
        assert!(
            !fresh.truncated && fresh.tail.chars().count() == LIMIT,
            "expected a lone {LIMIT}-char chunk kept whole and not truncated | received truncated={} chars={}",
            fresh.truncated,
            fresh.tail.chars().count()
        );

        let mut after = OpenActivity::default();
        after.append("x");
        after.append(&of('a', LIMIT));
        assert!(
            after.truncated && after.tail == of('a', LIMIT),
            "expected the earlier byte dropped and the tail flagged truncated | received truncated={}",
            after.truncated
        );
    }

    #[test]
    fn a_chunk_longer_in_bytes_than_in_characters_is_not_taken_for_an_oversized_one() {
        // 1500 characters of 3 bytes: over the limit in bytes, under it in characters.
        let wide = of('€', 1500);
        assert!(wide.len() > LIMIT && wide.chars().count() < LIMIT);
        assert_same_tail("a wide chunk under the limit", std::slice::from_ref(&wide));
        assert_same_tail("two wide chunks", &[wide.clone(), wide]);
    }

    #[test]
    fn many_small_chunks_followed_by_one_huge_one_give_the_reference_tail() {
        for unit in UNITS {
            let mut chunks: Vec<String> = (0..300).map(|n| numbered(n * 7, 1 + n % 40)).collect();
            chunks.push(of(unit, 100_000));
            chunks.extend((0..20).map(|n| numbered(n * 11, 30)));
            assert_same_tail(&format!("small chunks then a huge {unit:?} chunk"), &chunks);
        }
    }

    #[test]
    fn a_seeded_mix_of_widths_and_sizes_gives_the_reference_tail() {
        // xorshift64: a fixed seed, so a failure names a chunk that reproduces.
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let sizes = [
            0,
            1,
            17,
            LIMIT - 1,
            LIMIT,
            LIMIT + 1,
            2 * LIMIT,
            5 * LIMIT + 3,
        ];
        let chunks: Vec<String> = (0..2_000)
            .map(|_| {
                let unit = UNITS[(next() % 4) as usize];
                let chars = sizes[(next() % sizes.len() as u64) as usize];
                if next() % 3 == 0 {
                    numbered((next() % 1000) as usize, chars)
                } else {
                    of(unit, chars)
                }
            })
            .collect();
        assert_same_tail("a seeded mix", &chunks);
    }

    #[test]
    fn one_large_delta_does_not_leave_its_size_behind_as_capacity() {
        for (unit, chars) in [('a', 1_000_000), ('😀', 250_000)] {
            let mut open = OpenActivity::default();
            open.append(&of(unit, chars));
            assert!(
                open.tail.capacity() <= 8 * 1024,
                "expected a tail capacity of at most 8192 bytes after one {chars}-char delta of {unit:?} | received {}",
                open.tail.capacity()
            );
            assert_eq!(open.tail.chars().count(), LIMIT);
        }
    }

    #[test]
    fn a_huge_chunk_after_small_ones_releases_what_they_held() {
        let mut open = OpenActivity::default();
        for _ in 0..50 {
            open.append(&of('a', 1_500));
        }
        open.append(&of('b', 1_000_000));
        assert!(
            open.tail.capacity() <= 8 * 1024,
            "expected a tail capacity of at most 8192 bytes after a huge chunk | received {}",
            open.tail.capacity()
        );
    }

    #[test]
    fn last_window_start_finds_the_start_of_the_last_limit_characters() {
        assert_eq!(super::last_window_start(&of('a', LIMIT - 1)), None);
        assert_eq!(super::last_window_start(&of('a', LIMIT)), Some(0));
        assert_eq!(super::last_window_start(&of('a', LIMIT + 5)), Some(5));
        let wide = format!("{}{}", of('x', 3), of('😀', LIMIT));
        assert_eq!(
            super::last_window_start(&wide),
            Some(3),
            "expected the window to start after the three narrow characters, counted in bytes"
        );
    }
}
