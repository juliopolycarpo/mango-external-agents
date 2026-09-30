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
use mango_external_agents::event::{ActivityUpdate, EventKind};

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
#[derive(Debug, Default)]
pub struct TurnReducer {
    activities: HashMap<String, OpenActivity>,
    /// The answer text already emitted per message item, until that item completes.
    streamed: HashMap<String, Streamed>,
    /// The most streamed text kept for one message; `None` keeps all of it.
    max_message_bytes: Option<usize>,
    /// The vendor code of the last error report the server did not mean to retry.
    reported_code: Option<String>,
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
        let update = match progress {
            Progress::Output(chunk) if chunk.is_empty() => return Outcome::Ignore,
            Progress::Output(chunk) => {
                open.append(&chunk);
                let mut update = ActivityUpdate::new().with_detail(open.tail.clone());
                update.truncated = open.truncated;
                update
            }
            Progress::Patch([]) => return Outcome::Ignore,
            Progress::Patch(changes) => {
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
        if open
            .last_update
            .is_some_and(|last| instant.saturating_duration_since(last) < ACTIVITY_UPDATE_INTERVAL)
        {
            return Outcome::Ignore;
        }
        open.last_update = Some(instant);
        Outcome::Emit(vec![EventKind::ActivityUpdated {
            call_id: item_id.to_owned(),
            update,
        }])
    }
}

impl OpenActivity {
    /// Appends output, keeping only the most recent characters.
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

#[cfg(test)]
mod tests {
    use super::{ACTIVITY_UPDATE_DETAIL_MAX_CHARS, ACTIVITY_UPDATE_INTERVAL, TurnReducer};
    use crate::protocol::notifications::{Notification, method};
    use crate::reducer::Outcome;
    use mango_external_agents::content::ActivityContent;
    use mango_external_agents::event::{ActivityUpdate, EventKind};
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
}
