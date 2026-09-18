//! One `claude --print --output-format stream-json` run, reduced to neutral events.
//!
//! Pure and synchronous on purpose: it takes a record and hands back events. Nothing here awaits,
//! spawns or emits, which is what lets a captured transcript be replayed against it byte for byte
//! with no process and no runtime. The turn loop feeds it and puts what comes out on the
//! [`EventSink`](mango_external_agents::EventSink).
//!
//! Three properties this module exists to hold.
//!
//! **Text is delivered once.** With `--include-partial-messages` the stream carries the same
//! assistant output twice: as `stream_event` deltas while it is being produced, and again as a
//! whole `assistant` message when the block closes. Emitting both would double every reply. So
//! token-level text and thinking come from `stream_event` only, and `assistant` records are read
//! for the things deltas cannot express — the completed `tool_use` block, whose streaming form is
//! partial JSON, and subagent output.
//!
//! **Claude's subagents stay Claude's.** The `Task` tool spawns them, and
//! `--forward-subagent-text` emits their messages with `parent_tool_use_id` set. Those are nested
//! under the parent activity as detail, never promoted into the main transcript: the host's own
//! delegation vocabulary would tell a user the host made a hand-off it did not make.
//!
//! **Unknown records are ignored, not fatal.** The record vocabulary is wider than anything a plan
//! enumerated — `system/status`, `system/thinking_tokens`, `system/api_retry` and
//! `rate_limit_event` all appear on one live run — and it will keep growing. A type this reducer
//! has never seen is dropped silently; only a `result` ends the turn.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use mango_external_agents::normalize::TextLimit;
use mango_external_agents::{
    Activity, ActivityKind, ActivityResult, ActivityStatus, ActivityUpdate, Command, ErrorCode,
    EventKind, VendorError,
};
use serde_json::Value;

use crate::commands;
use crate::protocol::{ContentBlock, PermissionDenied, StreamRecord};

/// How much text any detail carries on its way to the sink.
///
/// Two call sites, one bound. It caps what a long-running subagent accumulates in memory, and it
/// caps every detail `detail_for` builds. The head is kept rather than the tail, which is what the
/// reader is following.
///
/// Deliberately **above** the sink's own `TextLimit::Detail` rather than equal to it. `EventSink`
/// bounds a detail again on the way out and ORs `truncated` from whatever *it* had to cut, so a
/// detail handed over already at the sink's bound is not cut there and the flag reads false — a
/// host would render a truncated detail as complete. Cutting at twice the bound costs one
/// allocation and keeps the cut visible to the one place that reports it; see
/// `keeps_a_cut_visible_to_the_sink_that_reports_it`.
const DETAIL_CARRY_MAX_CHARS: usize = 2 * TextLimit::Detail.max_code_points();

/// The fields Claude's own built-ins use for "what is this call about", in the order that answers
/// it best.
///
/// Order matters: a `Bash` call has both `command` and `description`, and the command is what the
/// user is looking for.
const TITLE_FIELDS: [&str; 7] = [
    "command",
    "file_path",
    "pattern",
    "url",
    "path",
    "prompt",
    "description",
];

/// What discovery and the session learn from the first record of a run.
///
/// Session-scoped facts, not turn events: [`SessionState`](mango_external_agents::SessionState) is
/// where they belong now, and the turn loop folds this back onto it — see
/// `session::apply_init`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunInit {
    /// The vendor's own session handle, which proves the conversation now exists on disk.
    pub session_id: Option<String>,
    /// The model the run resolved to.
    pub model: Option<String>,
    /// The slash commands this run announced, when it announced any.
    ///
    /// `None` (rather than an empty vector) is "this run said nothing publishable" — see
    /// [`commands::catalog`] for why that is not the same as an empty catalog.
    pub commands: Option<Vec<Command>>,
}

/// What one record produced.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reduction {
    /// The neutral events, in order.
    pub events: Vec<EventKind>,
    /// What the run said about itself, on the one record that says it.
    pub init: Option<RunInit>,
}

impl Reduction {
    fn of(events: Vec<EventKind>) -> Self {
        Self { events, init: None }
    }
}

/// Which delivery channel a block streams on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Channel {
    Text,
    Reasoning,
}

/// What the deltas actually delivered for one block of the message now streaming.
#[derive(Clone, Debug)]
struct Delivered {
    channel: Channel,
    text: String,
}

/// One run's records, reduced to neutral events.
#[derive(Clone, Debug, Default)]
pub struct TurnReducer {
    finished: bool,
    /// Tool calls this run has opened, in the order it opened them, so an unclosed call can be
    /// closed in a stable order.
    open_activities: Vec<String>,
    /// Forwarded subagent text per parent call, so updates accumulate rather than replace.
    nested_text: BTreeMap<String, String>,
    /// A held `system/permission_denied` reason, keyed by the call it refused, until the
    /// `tool_result` that closes the call arrives.
    denied_activities: BTreeMap<String, String>,
    /// What the deltas delivered for each block of the message now streaming, keyed by the index
    /// the stream itself supplies.
    ///
    /// Cleared at every message boundary: block indices restart at zero for each message, so a
    /// buffer that outlived its own would be matched against the next message's blocks. The
    /// channel travels with the text because a `thinking` buffer and a `text` buffer often carry
    /// the same sentence — Claude restates the plan it just reasoned through — and without the
    /// restriction the wrong one could claim the other's delivery.
    delivered_by_block: BTreeMap<u64, Delivered>,
    /// Indices of the reasoning blocks this message opened and has not closed.
    ///
    /// `content_block_stop` states an index and nothing else — not the type of the block it closes
    /// — so the type has to be remembered from the `content_block_start` that opened it.
    open_reasoning_blocks: BTreeSet<u64>,
}

impl TurnReducer {
    /// A reducer for one run.
    ///
    /// Whether this run was launched against an existing conversation is no longer this type's
    /// business: that is session state, reported through
    /// [`SessionState::set_native_session_id`](mango_external_agents::SessionState::set_native_session_id)
    /// and [`SessionSnapshot::resumed`](mango_external_agents::SessionSnapshot::resumed), which the
    /// session already knows without asking a reducer that has not read a byte yet.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_agent_claude::{protocol::StreamRecord, reducer::TurnReducer};
    /// use mango_external_agents::EventKind;
    ///
    /// let mut reducer = TurnReducer::new();
    /// let record = StreamRecord::parse(r#"{"type":"result","is_error":false}"#).expect("record");
    /// assert_eq!(reducer.reduce(&record).events, vec![EventKind::Completed]);
    /// assert!(reducer.finished());
    /// ```
    pub fn new() -> Self {
        Self {
            finished: false,
            open_activities: Vec::new(),
            nested_text: BTreeMap::new(),
            denied_activities: BTreeMap::new(),
            delivered_by_block: BTreeMap::new(),
            open_reasoning_blocks: BTreeSet::new(),
        }
    }

    /// Whether a `result` record, or an [`abort`](Self::abort), has already ended this run.
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// Reduces one record.
    pub fn reduce(&mut self, record: &StreamRecord) -> Reduction {
        if self.finished {
            return Reduction::default();
        }
        match record.kind() {
            Some("system") => self.reduce_system(record),
            Some("stream_event") => Reduction::of(self.reduce_stream_event(record)),
            Some("assistant") => Reduction::of(self.reduce_assistant(record)),
            Some("user") => Reduction::of(self.reduce_user(record)),
            Some("result") => Reduction::of(self.reduce_result(record)),
            // `rate_limit_event`, and whatever the vendor adds next.
            _ => Reduction::default(),
        }
    }

    /// Closes a run whose process ended without a `result` record.
    ///
    /// A crash, a killed process tree or an exhausted budget all land here. The turn has to reach
    /// a terminal state either way, and an open activity has to stop claiming it is still running.
    /// Returns nothing for a run that already ended, so a late abort cannot end a turn twice.
    pub fn abort(&mut self, error: VendorError) -> Vec<EventKind> {
        if self.finished {
            return Vec::new();
        }
        let mut events = self.end_run();
        events.push(EventKind::Error { error });
        events
    }

    /// Ends the run without a failure, closing every call it left open.
    ///
    /// The terminal pair is the caller's: a cancellation's marker carries a reason only the caller
    /// knows, so this closes the activities and
    /// [`EventSink::cancel`](mango_external_agents::EventSink::cancel) says why the turn stopped.
    /// Returns nothing for a run that already ended, so a cancel racing a `result` cannot end a
    /// turn twice.
    pub fn cancel(&mut self) -> Vec<EventKind> {
        if self.finished {
            return Vec::new();
        }
        self.end_run()
    }

    /// Marks the run over and closes whatever it left open, for [`abort`](Self::abort),
    /// [`cancel`](Self::cancel) and a terminal `result` record alike.
    fn end_run(&mut self) -> Vec<EventKind> {
        self.finished = true;
        self.close_open_activities()
    }

    fn reduce_system(&mut self, record: &StreamRecord) -> Reduction {
        match record.subtype() {
            // Held rather than forwarded: it names the call it refuses, but the activity it
            // belongs to closes through the `tool_result` that always follows, and that is the one
            // rendering the user should see.
            Some("permission_denied") => {
                self.record_denial(&record.permission_denied());
                Reduction::default()
            }
            Some("init") => self.reduce_init(record),
            // `status`, `thinking_tokens` and `api_retry` are progress reporting with no neutral
            // event behind them. The contract has no member that means "still working", and
            // inventing one out of a vendor's telemetry would put counts in a transcript no other
            // harness can produce.
            _ => Reduction::default(),
        }
    }

    /// The one record that names the vendor's own session handle and, when the build says so,
    /// its slash-command catalog.
    ///
    /// Both are session state rather than turn events now — see [`RunInit`] — so nothing is
    /// pushed onto the turn stream here. The catalog is read every time this record arrives
    /// rather than once per session, because the CLI is spawned again for every turn and re-reads
    /// its command directories each time; that is also why it is read even when the session id
    /// was missing, rather than tied to a handle it does not need.
    fn reduce_init(&mut self, record: &StreamRecord) -> Reduction {
        let init = record.init();
        let run = RunInit {
            session_id: init.session_id().map(str::to_owned),
            model: init.model().map(str::to_owned),
            commands: commands::catalog(&init),
        };
        Reduction {
            events: Vec::new(),
            init: Some(run),
        }
    }

    fn record_denial(&mut self, denial: &PermissionDenied<'_>) {
        let (Some(call_id), Some(message)) = (denial.tool_use_id(), denial.message()) else {
            return;
        };
        self.denied_activities
            .insert(call_id.to_owned(), message.to_owned());
    }

    /// Token-level output. The only source of [`EventKind::TextDelta`] and
    /// [`EventKind::ReasoningDelta`].
    ///
    /// `signature_delta` and `input_json_delta` are deliberately dropped: the first is a thinking
    /// block's signature with nothing to render, and the second is partial JSON for a tool call
    /// whose completed form arrives as an `assistant` record.
    fn reduce_stream_event(&mut self, record: &StreamRecord) -> Vec<EventKind> {
        // A subagent's own token stream, if one ever arrives, is nested through the `assistant`
        // path rather than promoted into the main transcript.
        if record.parent_tool_use_id().is_some() {
            return Vec::new();
        }
        let Some(event) = record.stream_event() else {
            return Vec::new();
        };
        let kind = event.kind();

        // Both ends of a message. Block indices are scoped to the message that opened them, so
        // they mean nothing once it is over — but a reasoning phase still open here was closed by
        // the message ending, whether or not its own `content_block_stop` arrived, and has to say
        // so before the index it is keyed by is discarded.
        if matches!(kind, Some("message_start" | "message_stop")) {
            let closing = self.close_reasoning_blocks();
            self.delivered_by_block.clear();
            return closing;
        }

        let index = event.index();
        match kind {
            Some("content_block_start") => {
                self.reduce_block_start(event.content_block_type(), index)
            }
            Some("content_block_stop") => {
                let Some(index) = index else {
                    return Vec::new();
                };
                if !self.open_reasoning_blocks.remove(&index) {
                    return Vec::new();
                }
                vec![EventKind::ReasoningEnded]
            }
            Some("content_block_delta") => self.reduce_delta(&event, index),
            _ => Vec::new(),
        }
    }

    /// A block opening: records the channel it will stream on and, for a reasoning phase, opens it.
    ///
    /// Opened with nothing delivered yet. Recorded even so: a withheld reasoning phase streams only
    /// empty `thinking_delta`s, and this is what says those deltas were still this block's delivery
    /// channel. A block whose kind will never be renderable — `tool_use` chief among them — gets no
    /// entry, since one could never match anything in `undelivered_remainder`.
    ///
    /// The reasoning-phase announcement fires once per block, by protocol — the only signal a
    /// reasoning phase produces on an account whose `thinking_delta` text is withheld.
    /// `redacted_thinking` qualifies and then some: its text is encrypted, so no renderable delta
    /// can ever follow and the announcement is the whole of what that phase will show.
    fn reduce_block_start(
        &mut self,
        block_type: Option<&str>,
        index: Option<u64>,
    ) -> Vec<EventKind> {
        if let (Some(index), Some(channel)) = (index, opening_channel(block_type)) {
            self.delivered_by_block.insert(
                index,
                Delivered {
                    channel,
                    text: String::new(),
                },
            );
        }
        if !is_reasoning_block(block_type) {
            return Vec::new();
        }
        if let Some(index) = index {
            self.open_reasoning_blocks.insert(index);
        }
        vec![EventKind::ReasoningStarted]
    }

    fn reduce_delta(
        &mut self,
        event: &crate::protocol::StreamEvent<'_>,
        index: Option<u64>,
    ) -> Vec<EventKind> {
        let Some(delta) = event.delta() else {
            return Vec::new();
        };
        match (delta.kind(), delta.text(), delta.thinking()) {
            (Some("text_delta"), Some(text), _) => {
                self.record_delivered(index, Channel::Text, text);
                vec![EventKind::TextDelta {
                    text: text.to_owned(),
                }]
            }
            (Some("thinking_delta"), _, Some(thinking)) => {
                self.record_delivered(index, Channel::Reasoning, thinking);
                vec![EventKind::ReasoningDelta {
                    text: thinking.to_owned(),
                }]
            }
            _ => Vec::new(),
        }
    }

    /// Closes every reasoning phase still open, one [`EventKind::ReasoningEnded`] each.
    ///
    /// The safety net for a message that ended without a `content_block_stop` for its reasoning
    /// block: the phase is over either way, and a projection left holding an open one would go on
    /// treating a finished turn as stopped inside it. A no-op on every recorded run — the stops do
    /// arrive.
    fn close_reasoning_blocks(&mut self) -> Vec<EventKind> {
        let open = self.open_reasoning_blocks.len();
        self.open_reasoning_blocks.clear();
        vec![EventKind::ReasoningEnded; open]
    }

    /// Appends what one delta just delivered to its own block's running copy.
    fn record_delivered(&mut self, index: Option<u64>, channel: Channel, text: &str) {
        let Some(index) = index else {
            return;
        };
        let entry = self.delivered_by_block.entry(index).or_insert(Delivered {
            channel,
            text: String::new(),
        });
        entry.channel = channel;
        entry.text.push_str(text);
    }

    /// The part of a completed block that reached nobody.
    ///
    /// Blocks are matched by what streamed for them rather than by index: the `assistant` record
    /// arrives interleaved with its own `content_block_stop`, so whichever index is open at that
    /// moment is not reliably the record's. The longest delivered buffer of the same channel that
    /// this text extends is that block's own copy, and the empty string is a prefix of everything
    /// — so a block nothing streamed for matches nothing, and its whole content is the remainder.
    ///
    /// Restricted to buffers of the matching channel, and the match is consumed once found: a
    /// `thinking` buffer and a `text` buffer often carry the same sentence, and without that
    /// restriction the shorter, wrong-channel buffer could be picked as the text block's own
    /// delivery — leaving only the tail past where the thinking text stopped matching, silently
    /// dropping the rest of a reply that never actually streamed as text.
    fn undelivered_remainder(&mut self, channel: Channel, text: &str) -> String {
        let mut matched = None;
        let mut delivered = 0;
        for (index, entry) in &self.delivered_by_block {
            if entry.channel != channel {
                continue;
            }
            if entry.text.len() > delivered && text.starts_with(&entry.text) {
                delivered = entry.text.len();
                matched = Some(*index);
            }
        }
        if let Some(index) = matched {
            self.delivered_by_block.remove(&index);
        }
        // `delivered` is the byte length of a prefix of `text`, so it is a character boundary.
        text[delivered..].to_owned()
    }

    /// Completed assistant blocks.
    ///
    /// Main-conversation `text` and `thinking` are emitted for exactly the part of themselves the
    /// deltas never carried. Usually that is nothing — the deltas delivered the block in full, and
    /// replaying it would double every reply. When `--include-partial-messages` produced no deltas
    /// for the block at all, the remainder is the block entire and this completed record is the
    /// only copy of it that will ever exist. A stream cut off mid-block lands between the two and
    /// contributes its tail, which is the case neither all-or-nothing reading of "already
    /// delivered" could express.
    fn reduce_assistant(&mut self, record: &StreamRecord) -> Vec<EventKind> {
        let parent = record.parent_tool_use_id();
        let mut events = Vec::new();
        for block in record.content_blocks() {
            // Every arm below the parent check belongs to a subagent, which is why the main
            // conversation's two are taken first: a `tool_use` a subagent made would otherwise
            // open an activity beside the `Task` that spawned it, as though the assistant had run
            // the call itself — and its `tool_result` arrives under the same parent, so nothing
            // would ever close it. A subagent's work is reported through the parent activity; see
            // this module's own header.
            let Some(parent) = parent else {
                if block.kind() == Some("tool_use") {
                    events.extend(self.start_activity(&block));
                } else {
                    events.extend(self.undelivered_event_for(&block));
                }
                continue;
            };
            // Only under a call that is still open. A completed activity has been removed from the
            // map, and an update *replaces* an activity's detail — so a late message would reopen
            // a closed activity, and one for a parent that never existed would address an activity
            // that is not there.
            if !self.is_open(parent) {
                continue;
            }
            let Some(text) = block.text().filter(|_| block.kind() == Some("text")) else {
                continue;
            };
            // Accumulated, not replaced: an update overwrites downstream, so emitting each block
            // on its own would leave only the last one — a subagent that reported three findings
            // would render as having found the third. Borrowed rather than cloned: the buffer runs
            // to twice a detail's bound, and `detail_for` allocates the copy that is kept anyway.
            let merged = self.append_nested(parent, text);
            events.push(EventKind::ActivityUpdated {
                call_id: parent.to_owned(),
                update: ActivityUpdate::new().with_optional_detail(detail_for(merged)),
            });
        }
        events
    }

    /// The delta event carrying whatever of a completed main-conversation block the stream never
    /// delivered, or nothing when it delivered all of it.
    fn undelivered_event_for(&mut self, block: &ContentBlock<'_>) -> Option<EventKind> {
        let (channel, text) = renderable_block(block)?;
        let remainder = self.undelivered_remainder(channel, text);
        if remainder.is_empty() {
            return None;
        }
        Some(match channel {
            Channel::Text => EventKind::TextDelta { text: remainder },
            Channel::Reasoning => EventKind::ReasoningDelta { text: remainder },
        })
    }

    fn start_activity(&mut self, block: &ContentBlock<'_>) -> Option<EventKind> {
        let call_id = block.id()?;
        let name = block.name()?;
        if self.is_open(call_id) {
            return None;
        }
        self.open_activities.push(call_id.to_owned());
        Some(EventKind::ActivityStarted {
            call_id: call_id.to_owned(),
            // Verbatim. `Read` is `Read`, and an MCP tool keeps its namespaced name: renaming
            // another company's tools in a host's interface would misattribute the work.
            activity: Activity::new(
                name,
                activity_kind(name),
                summarize_tool_input(block.input()),
            ),
        })
    }

    /// Tool results, which Claude reports as a `user` message.
    ///
    /// A denied tool lands here too, as a `tool_result` with `is_error: true` — closing the
    /// activity needs no special case for that. Its detail does: a held
    /// `system/permission_denied` is the vendor's own statement of why, and takes priority over
    /// whatever `tool_result.content` happens to carry, which is not guaranteed to say anything
    /// past "denied". One rendering either way — nothing else ever reports the same refusal.
    fn reduce_user(&mut self, record: &StreamRecord) -> Vec<EventKind> {
        let mut events = Vec::new();
        for block in record.content_blocks() {
            if block.kind() != Some("tool_result") {
                continue;
            }
            let Some(call_id) = block.tool_use_id() else {
                continue;
            };
            if !self.close(call_id) {
                continue;
            }
            self.nested_text.remove(call_id);
            let detail = self
                .denied_activities
                .remove(call_id)
                .map(Cow::Owned)
                .unwrap_or_else(|| block.result_text());
            events.push(EventKind::ActivityCompleted {
                call_id: call_id.to_owned(),
                result: ActivityResult::new(if block.is_error() {
                    ActivityStatus::Failed
                } else {
                    ActivityStatus::Completed
                })
                .with_optional_detail(detail_for(&detail)),
            });
        }
        events
    }

    /// The terminal record.
    ///
    /// A run that ends with tool calls still open closes them as cancelled: the process is gone,
    /// so nothing will ever report their outcome, and an activity left spinning in a reloaded
    /// transcript is a control that will never resolve.
    fn reduce_result(&mut self, record: &StreamRecord) -> Vec<EventKind> {
        let mut events = self.end_run();
        if let Some(usage) = record.result().usage() {
            events.push(EventKind::Usage { usage });
        }
        match result_error(record) {
            Some(error) => events.push(EventKind::Error { error }),
            None => events.push(EventKind::Completed),
        }
        events
    }

    /// Closes every call the run ended without reporting, as cancelled.
    ///
    /// A call the vendor already refused carries that refusal into its close. The `tool_result`
    /// that normally delivers the reason never arrived, so the held `system/permission_denied` is
    /// the only statement anyone made about why that call did not happen.
    fn close_open_activities(&mut self) -> Vec<EventKind> {
        let open = std::mem::take(&mut self.open_activities);
        let denials = std::mem::take(&mut self.denied_activities);
        open.into_iter()
            .map(|call_id| {
                let detail = denials
                    .get(&call_id)
                    .map(String::as_str)
                    .unwrap_or_default();
                EventKind::ActivityCompleted {
                    call_id,
                    result: ActivityResult::new(ActivityStatus::Cancelled)
                        .with_optional_detail(detail_for(detail)),
                }
            })
            .collect()
    }

    fn is_open(&self, call_id: &str) -> bool {
        self.open_activities.iter().any(|open| open == call_id)
    }

    /// Removes an open call, answering whether it was one.
    fn close(&mut self, call_id: &str) -> bool {
        let Some(position) = self.open_activities.iter().position(|open| open == call_id) else {
            return false;
        };
        self.open_activities.remove(position);
        true
    }

    /// Bounded concatenation of a subagent's forwarded blocks.
    ///
    /// Hands back the accumulated buffer itself. A copy would be made per forwarded block, of a
    /// string that is allowed to grow to twice a detail's bound, only for the caller to bound and
    /// copy it again on the way into the event.
    fn append_nested(&mut self, parent: &str, text: &str) -> &str {
        let entry = self.nested_text.entry(parent.to_owned()).or_default();
        if !entry.is_empty() {
            entry.push('\n');
        }
        entry.push_str(text);
        if let Some((boundary, _)) = entry.char_indices().nth(DETAIL_CARRY_MAX_CHARS) {
            entry.truncate(boundary);
        }
        entry
    }
}

/// Claude's tool names mapped onto the neutral icon buckets.
///
/// Icon selection only. The label is the vendor's own tool name, verbatim and untranslated.
///
/// The default is [`ActivityKind::Other`] rather than a guess: a tool this table has never heard
/// of is far more likely to be an MCP tool or a plugin's than a new built-in, and picking
/// [`ActivityKind::Command`] would put a shell icon on something that never touched a shell.
///
/// # Example
///
/// ```
/// use mango_agent_claude::reducer::activity_kind;
/// use mango_external_agents::ActivityKind;
///
/// assert_eq!(activity_kind("Bash"), ActivityKind::Command);
/// assert_eq!(activity_kind("mcp__playwright__browser_click"), ActivityKind::Mcp);
/// assert_eq!(activity_kind("SomethingNew"), ActivityKind::Other);
/// ```
pub fn activity_kind(tool_name: &str) -> ActivityKind {
    match tool_name {
        "Bash" | "BashOutput" | "KillShell" => ActivityKind::Command,
        "Edit" | "Write" | "NotebookEdit" => ActivityKind::FileChange,
        "Task" => ActivityKind::Subagent,
        "WebSearch" | "WebFetch" => ActivityKind::WebSearch,
        "TodoWrite" | "ExitPlanMode" => ActivityKind::Plan,
        // MCP tools are namespaced `mcp__<server>__<tool>` by the CLI, which is the one shape
        // worth recognising here: a protocol convention rather than a tool name, so it cannot
        // collide with a built-in.
        _ if tool_name.starts_with(commands::MCP_PREFIX) => ActivityKind::Mcp,
        _ => ActivityKind::Other,
    }
}

/// Whether an opening content block is a reasoning phase, redacted or not.
fn is_reasoning_block(block_type: Option<&str>) -> bool {
    matches!(block_type, Some("thinking" | "redacted_thinking"))
}

/// Which channel an opening content block will stream on, if either.
///
/// `redacted_thinking` streams no deltas at all — its text is encrypted — but it is still a
/// reasoning-channel block for the purpose of not colliding with a text buffer.
fn opening_channel(block_type: Option<&str>) -> Option<Channel> {
    match block_type {
        Some("text") => Some(Channel::Text),
        other if is_reasoning_block(other) => Some(Channel::Reasoning),
        _ => None,
    }
}

/// The renderable content of a `text` or `thinking` block, or nothing for neither.
fn renderable_block<'a>(block: &ContentBlock<'a>) -> Option<(Channel, &'a str)> {
    match block.kind()? {
        "text" => Some((Channel::Text, block.text()?)),
        "thinking" => Some((Channel::Reasoning, block.thinking()?)),
        _ => None,
    }
}

/// A one-line summary of a tool call's input, for the activity's title.
fn summarize_tool_input(input: Option<&Value>) -> String {
    let Some(Value::Object(fields)) = input else {
        return String::new();
    };
    TITLE_FIELDS
        .into_iter()
        .find_map(|key| fields.get(key).and_then(Value::as_str))
        .filter(|value| !value.is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// A detail field, bounded, or nothing for text that says nothing.
fn detail_for(detail: &str) -> Option<String> {
    let bounded = head(detail, DETAIL_CARRY_MAX_CHARS);
    (!bounded.is_empty()).then(|| bounded.to_owned())
}

/// The first `max` characters, never cutting one in half.
fn head(text: &str, max: usize) -> &str {
    text.char_indices()
        .nth(max)
        .map_or(text, |(boundary, _)| &text[..boundary])
}

/// Whether a `result` record is a failure, and what to call it.
///
/// A run whose only problem was a refused tool is **not** an error: `permission_denials` is
/// populated, `is_error` is false and the process exits zero — the agent asked, the host's
/// configuration said no, and the vendor reported that faithfully. Reporting it as a failed turn
/// would blame the user's own permission choice on the vendor.
fn result_error(record: &StreamRecord) -> Option<VendorError> {
    let result = record.result();
    if !result.is_error() {
        return None;
    }
    let subtype = record.subtype().unwrap_or("error");
    let terminal_reason = result.terminal_reason();
    let errors = result.error_texts().join("\n");

    let message = if !errors.is_empty() {
        errors
    } else if let Some(text) = result.result_text() {
        text.to_owned()
    } else if let Some(reason) = terminal_reason {
        format!("Claude Code ended the turn: {reason}.")
    } else if NAMED_RESULT_SUBTYPES.contains(&subtype) {
        format!("Claude Code ended the turn with \"{subtype}\".")
    } else {
        // The record explained nothing and its subtype is one this file does not name, so there is
        // nothing left that the library wrote. Quoting the subtype here would put it back into the
        // text a host shows, which is the same field the code was just kept out of.
        String::from("Claude Code ended the turn without explaining why.")
    };

    let error = VendorError::new(result_code(subtype), message);
    match (result.api_error_status(), terminal_reason) {
        (Some(status), _) => {
            Some(error.with_vendor_code(status.to_string(), retryable_status(status)))
        }
        (None, Some(reason)) => Some(error.with_vendor_code(reason, false)),
        (None, None) => Some(error),
    }
}

/// The result subtypes this harness will name, in a code and in a sentence it writes itself.
///
/// Every terminal `subtype` observed on a failed `--print` run — the four that carry their
/// explanation in `errors` rather than in `result`, as [`ResultRecord::error_texts`] documents —
/// plus the bare `error` a record with no subtype falls back to.
const NAMED_RESULT_SUBTYPES: &[&str] = &[
    "error",
    "error_during_execution",
    "error_max_turns",
    "error_max_budget_usd",
    "error_max_structured_output_retries",
];

/// The harness's code for a failed result, from a subtype it recognises.
///
/// `subtype` is a field the vendor fills in, and an [`ErrorCode`] is diagnostic text: `Display`
/// writes it, `resume_fallback_reason` puts it in a sentence a host shows. Bounding its shape
/// afterwards would still be printing whatever the vendor sent inside those bounds, so the list is
/// held here, where the code is made. A subtype nobody has documented becomes the generic
/// `claude-error`, and the vendor's own word stays on the message the record carried.
fn result_code(subtype: &str) -> ErrorCode {
    if NAMED_RESULT_SUBTYPES.contains(&subtype) {
        return ErrorCode::new(format!("claude-{subtype}"));
    }
    ErrorCode::from_static("claude-error")
}

/// Whether an identical retry of a failed vendor API call could plausibly succeed.
///
/// Only the two classes where the answer is a property of the response rather than of the request:
/// a rate limit, and a server-side failure. Everything else — a refused model, a prompt that is
/// too long — fails the same way every time, and marking it retryable would invite a host to spend
/// a second turn's tokens learning that.
fn retryable_status(status: i64) -> bool {
    status == 429 || (500..600).contains(&status)
}

#[cfg(test)]
mod tests {
    use super::{
        DETAIL_CARRY_MAX_CHARS, TurnReducer, activity_kind, detail_for, summarize_tool_input,
    };
    use crate::protocol::StreamRecord;
    use mango_external_agents::normalize::TextLimit;
    use mango_external_agents::{ActivityKind, ActivityUpdate, EventKind};
    use serde_json::json;

    /// The carry bound has to stay above the sink's, or a cut stops being reported.
    ///
    /// Lowering it to `TextLimit::Detail` looks like a saved allocation and is not: the sink then
    /// has nothing left to cut, `truncated` reads false, and a host renders a detail that was cut
    /// as one that was complete. This pins the invariant the constant's own doc argues for.
    #[test]
    fn keeps_a_cut_visible_to_the_sink_that_reports_it() {
        assert!(
            DETAIL_CARRY_MAX_CHARS > TextLimit::Detail.max_code_points(),
            "expected room for the sink to make the cut it flags, received {DETAIL_CARRY_MAX_CHARS} against {}",
            TextLimit::Detail.max_code_points()
        );

        let overlong = "m".repeat(DETAIL_CARRY_MAX_CHARS + 1);
        let update = ActivityUpdate::new()
            .with_optional_detail(detail_for(&overlong))
            .normalized();
        assert!(
            update.truncated,
            "expected the sink to report the cut it made, received {update:?}"
        );
    }

    fn reduce(reducer: &mut TurnReducer, line: &str) -> Vec<EventKind> {
        let record = StreamRecord::parse(line).expect("expected a parseable record");
        reducer.reduce(&record).events
    }

    #[test]
    fn ignores_an_unknown_top_level_record_type() {
        let mut reducer = TurnReducer::new();
        assert!(reduce(&mut reducer, r#"{"type":"rate_limit_event"}"#).is_empty());
        assert!(!reducer.finished());
    }

    #[test]
    fn ignores_a_system_subtype_with_no_neutral_event_behind_it() {
        let mut reducer = TurnReducer::new();
        assert!(reduce(&mut reducer, r#"{"type":"system","subtype":"api_retry"}"#).is_empty());
        assert!(
            reduce(
                &mut reducer,
                r#"{"type":"system","subtype":"thinking_tokens"}"#
            )
            .is_empty()
        );
    }

    /// The vendor's own session handle is session state, not a turn event — see [`RunInit`] and
    /// the module documentation for [`crate::session`]. It arrives through
    /// [`TurnReducer::reduce`]'s `init` half rather than as an
    /// [`EventKind`] the turn stream carries.
    #[test]
    fn folds_the_vendors_session_handle_into_run_init_rather_than_a_turn_event() {
        let mut reducer = TurnReducer::new();
        let record =
            StreamRecord::parse(r#"{"type":"system","subtype":"init","session_id":"sess_1"}"#)
                .expect("expected a parseable record");
        let reduction = reducer.reduce(&record);
        assert!(
            reduction.events.is_empty(),
            "expected no turn event for a session-scoped fact, received {:?}",
            reduction.events
        );
        assert_eq!(
            reduction
                .init
                .expect("expected the run to describe itself")
                .session_id,
            Some(String::from("sess_1"))
        );
    }

    /// Unlike the old `SessionStarted` event, which only fired once, a repeated `init` is reported
    /// every time it arrives: applying the same fact twice to
    /// [`SessionState`](mango_external_agents::SessionState) is a no-op, so there is nothing this
    /// type has to remember on the record's behalf any more.
    #[test]
    fn reports_a_repeated_init_again_rather_than_withholding_it() {
        let mut reducer = TurnReducer::new();
        let record =
            StreamRecord::parse(r#"{"type":"system","subtype":"init","session_id":"sess_1"}"#)
                .expect("expected a parseable record");
        assert!(reducer.reduce(&record).init.is_some());
        assert!(
            reducer.reduce(&record).init.is_some(),
            "expected the second init to still be reported"
        );
    }

    #[test]
    fn stops_reducing_after_the_result() {
        let mut reducer = TurnReducer::new();
        assert_eq!(
            reduce(&mut reducer, r#"{"type":"result","is_error":false}"#),
            vec![EventKind::Completed]
        );
        assert!(reduce(&mut reducer, r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"late"}}}"#).is_empty());
        assert!(
            reducer
                .abort(mango_external_agents::VendorError::new(
                    mango_external_agents::ErrorCode::from_static("claude-late"),
                    "too late"
                ))
                .is_empty()
        );
    }

    #[test]
    fn falls_back_to_other_for_a_tool_it_does_not_recognise() {
        assert_eq!(activity_kind("Read"), ActivityKind::Other);
        assert_eq!(activity_kind("Glob"), ActivityKind::Other);
        assert_eq!(activity_kind("Task"), ActivityKind::Subagent);
        assert_eq!(activity_kind("Edit"), ActivityKind::FileChange);
        assert_eq!(activity_kind("WebFetch"), ActivityKind::WebSearch);
        assert_eq!(activity_kind("ExitPlanMode"), ActivityKind::Plan);
    }

    #[test]
    fn titles_a_call_with_the_field_a_reader_is_looking_for() {
        let bash = json!({"command": "ls -la", "description": "List files"});
        assert_eq!(summarize_tool_input(Some(&bash)), "ls -la");

        let read = json!({"file_path": "/work/note.txt"});
        assert_eq!(summarize_tool_input(Some(&read)), "/work/note.txt");

        assert_eq!(summarize_tool_input(Some(&json!({"limit": 5}))), "");
        assert_eq!(summarize_tool_input(Some(&json!("scalar"))), "");
        assert_eq!(summarize_tool_input(None), "");
    }

    #[test]
    fn reports_a_failed_result_as_a_structured_error() {
        let mut reducer = TurnReducer::new();
        let events = reduce(
            &mut reducer,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"api_error_status":529}"#,
        );
        let EventKind::Error { error } = events.last().expect("expected a terminal") else {
            panic!("expected an error, received {events:?}");
        };
        assert_eq!(error.code.as_str(), "claude-error_during_execution");
        assert_eq!(error.vendor_code.as_deref(), Some("529"));
        assert!(
            error.retryable,
            "expected a 529 to be worth another attempt"
        );
    }

    /// A subtype is a field the vendor fills in, and a code is diagnostic text a host is shown.
    ///
    /// The documented subtypes become the harness's own label. Anything else becomes the generic
    /// one rather than a `claude-` prefix wrapped around whatever arrived — bounding its shape
    /// afterwards would still be printing the vendor's own string inside those bounds.
    #[test]
    fn a_result_subtype_nobody_documented_does_not_become_a_code() {
        let mut reducer = TurnReducer::new();
        let events = reduce(
            &mut reducer,
            r#"{"type":"result","subtype":"sk-live-subtype-canary","is_error":true}"#,
        );
        let EventKind::Error { error } = events.last().expect("expected a terminal") else {
            panic!("expected an error, received {events:?}");
        };
        assert_eq!(error.code.as_str(), "claude-error");
        assert_eq!(error.code.to_string(), "claude-error");
        // The fallback sentence is the library's own, so it must not put the subtype back.
        assert!(
            !error.message.contains("sk-live-subtype-canary"),
            "expected the subtype to stay out of the message, received {}",
            error.message
        );
    }

    /// The four subtypes that carry their explanation in `errors` are all named, not just two.
    #[test]
    fn every_documented_error_subtype_keeps_its_own_code() {
        for subtype in [
            "error_during_execution",
            "error_max_turns",
            "error_max_budget_usd",
            "error_max_structured_output_retries",
        ] {
            let mut reducer = TurnReducer::new();
            let line = format!(r#"{{"type":"result","subtype":"{subtype}","is_error":true}}"#);
            let events = reduce(&mut reducer, &line);
            let EventKind::Error { error } = events.last().expect("expected a terminal") else {
                panic!("expected an error, received {events:?}");
            };
            assert_eq!(
                error.code.as_str(),
                format!("claude-{subtype}"),
                "expected {subtype} to keep its own code"
            );
        }
    }

    #[test]
    fn surfaces_the_error_arms_own_text_instead_of_the_generic_fallback() {
        let mut reducer = TurnReducer::new();
        let events = reduce(
            &mut reducer,
            r#"{"type":"result","subtype":"error_max_turns","is_error":true,"errors":["Reached the turn limit.","Nothing was written."]}"#,
        );
        let EventKind::Error { error } = events.last().expect("expected a terminal") else {
            panic!("expected an error, received {events:?}");
        };
        assert_eq!(
            error.message,
            "Reached the turn limit.\nNothing was written."
        );
        assert!(!error.retryable);
    }

    #[test]
    fn names_the_vendors_own_terminal_reason_when_nothing_else_explains_the_failure() {
        let mut reducer = TurnReducer::new();
        let events = reduce(
            &mut reducer,
            r#"{"type":"result","subtype":"error","is_error":true,"terminal_reason":"budget_exhausted"}"#,
        );
        let EventKind::Error { error } = events.last().expect("expected a terminal") else {
            panic!("expected an error, received {events:?}");
        };
        assert_eq!(
            error.message,
            "Claude Code ended the turn: budget_exhausted."
        );
        assert_eq!(error.vendor_code.as_deref(), Some("budget_exhausted"));
    }

    #[test]
    fn names_the_subtype_when_the_vendor_explained_nothing_at_all() {
        let mut reducer = TurnReducer::new();
        let events = reduce(
            &mut reducer,
            r#"{"type":"result","subtype":"error","is_error":true}"#,
        );
        let EventKind::Error { error } = events.last().expect("expected a terminal") else {
            panic!("expected an error, received {events:?}");
        };
        assert_eq!(error.message, "Claude Code ended the turn with \"error\".");
        assert_eq!(error.vendor_code, None);
    }
}
