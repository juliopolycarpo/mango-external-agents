//! `session/update` in, [`EventKind`] events and session-scoped facts out.
//!
//! Pure: no IO, no clock, no channel. Everything the harness has to decide about an ACP frame is
//! decided here, against a value parsed from the wire, which is what makes each mapping a test with
//! a JSON literal in it rather than something only a live agent can exercise.
//!
//! Three rules run through the whole mapping:
//!
//! * **A tool call is an observation.** ACP calls them tool calls; they arrive as
//!   [`EventKind::ActivityStarted`] and friends, because nothing here may reach a host's tool
//!   executor. See the [`event`](mango_external_agents::event) module docs.
//! * **Our own text never comes back.** `user_message_chunk` is the agent echoing the prompt the
//!   host just sent; replaying it would duplicate the host's own message.
//! * **Session state is not transcript.** The mode, config-option, session-info and command-catalog
//!   updates describe the session rather than the turn. The command catalog is reported as a
//!   [`SessionFact`] rather than an [`EventKind`], because it is session state — see
//!   [`state::SessionState::set_commands`](mango_external_agents::state::SessionState::set_commands)
//!   — and the rest are dropped, because 0.1 has no session-state slot for them yet.
//!
//! ACP v1 reference: <https://agentclientprotocol.com/protocol/v1/prompt-turn>

use std::collections::{HashMap, HashSet};

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus,
    SessionConfigOption, SessionUpdate, StopReason, ToolCall, ToolCallContent, ToolCallLocation,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use mango_external_agents::event::{
    Activity, ActivityKind, ActivityResult, ActivityStatus, ActivityUpdate, Command, EventKind,
    ThreadUsage, Usage,
};
use mango_external_agents::{
    ActivityContent, ErrorCode, ExtensionValue, Extensions, FileChange, PlanStep, PlanStepPriority,
    PlanStepStatus, VendorError,
};

/// The call id every plan update shares.
///
/// ACP v1's `plan` update carries the whole plan and no identity of its own — the plan is a property
/// of the session, replaced wholesale each time it changes. The activity vocabulary needs a call id,
/// so the plan gets one constant one and its revisions arrive as updates to it. Prefixed to stay
/// clear of an agent's own tool call ids.
pub const PLAN_CALL_ID: &str = "acp:plan";

/// The title a tool call gets when its first frame named none.
///
/// Only reachable through a `tool_call_update` for a call this client never saw announced:
/// `tool_call` itself requires a title. An empty label is what a host would otherwise render.
pub const UNTITLED_TOOL_CALL: &str = "tool";

/// A session-scoped fact one frame carried, alongside whatever it said about the turn.
///
/// ACP re-announces its command catalog mid-session; that catalog is session state, not transcript
/// (see the module's own docs), so it is reported here rather than folded into
/// [`Reducer::update`]'s event vector. The harness applies it to
/// [`SessionState::set_commands`](mango_external_agents::state::SessionState::set_commands) instead
/// of the turn stream, which is what lets a *second* announcement reach a host that already read
/// the first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionFact {
    /// The agent announced its slash-command catalog.
    Commands(Vec<Command>),
    /// The agent replaced its complete live configuration catalog.
    ConfigurationOptions(Vec<SessionConfigOption>),
}

/// Turns one agent's frames into events, remembering only what a frame alone cannot say.
///
/// Three things: whether a reasoning block is open (ACP streams thought chunks with no start or end
/// marker, and [`EventKind::ReasoningStarted`]/[`EventKind::ReasoningEnded`] are a pair a host
/// relies on), whether the plan activity has been announced yet, and which tool calls the host has
/// been told about. The last one is what lets a call first reported through `tool_call_update`
/// still open a bracket, and a frame for a call that already ended stay out of the transcript.
#[derive(Debug, Default)]
pub struct Reducer {
    reasoning_open: bool,
    plan_started: bool,
    /// Tool calls the host saw start and has not yet seen end, by the agent's own call id.
    open_calls: HashMap<String, OpenCall>,
    /// Tool calls that already ended this turn, so a late frame cannot open a second bracket.
    finished_calls: HashSet<String>,
    /// Source of [`OpenCall::opened`], so the calls a turn ends owing are closed in the order the
    /// agent opened them rather than in a hash map's.
    next_call: u64,
}

/// One tool call a host is rendering as running.
#[derive(Debug)]
struct OpenCall {
    /// When it opened, relative to the turn's other calls.
    opened: u64,
}

impl Reducer {
    /// A reducer for one turn.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The events and session facts one `session/update` produces, in order.
    ///
    /// # Example
    ///
    /// ```
    /// use agent_client_protocol::schema::v1::SessionUpdate;
    /// use mango_agent_acp::reducer::Reducer;
    /// use mango_external_agents::EventKind;
    ///
    /// let update: SessionUpdate = serde_json::from_value(serde_json::json!({
    ///     "sessionUpdate": "agent_message_chunk",
    ///     "content": { "type": "text", "text": "hello" }
    /// }))
    /// .expect("a v1 agent message chunk");
    ///
    /// let mut reducer = Reducer::new();
    /// let (events, facts) = reducer.update(update);
    /// assert_eq!(events, vec![EventKind::TextDelta { text: String::from("hello") }]);
    /// assert!(facts.is_empty());
    /// ```
    pub fn update(&mut self, update: SessionUpdate) -> (Vec<EventKind>, Vec<SessionFact>) {
        // A transcript frame closes an open reasoning block: a host that never saw `ReasoningEnded`
        // would render a reasoning phase that stays open for the rest of the turn. A frame that
        // produces no transcript must *not*, or a usage report or a mode change arriving between two
        // thought chunks would split one continuous thought into two rendered blocks.
        let mut events = match transcript(&update) {
            true => self.close_reasoning(),
            false => Vec::new(),
        };
        let (body_events, facts) = self.body(update);
        events.extend(body_events);
        (events, facts)
    }

    /// The events that close out a turn that completed, before its terminal.
    ///
    /// [`Self::finish_with`] with [`ActivityStatus::Completed`]: see there for what a turn can end
    /// owing.
    pub fn finish(&mut self) -> Vec<EventKind> {
        self.finish_with(ActivityStatus::Completed)
    }

    /// The events that close out a turn, before its terminal, with the tool calls the agent left
    /// running ending as `calls`.
    ///
    /// Three debts a turn can end owing. A turn that stopped mid-thought owes the other half of the
    /// reasoning pair. A tool call the agent announced and never reported as ended owes its
    /// completion: ACP ends a turn with the `session/prompt` response rather than a frame per call,
    /// so without this the host renders that call as running forever. And a turn that opened the
    /// plan activity owes its completion, because ACP never sends one — the plan is session state
    /// that is replaced wholesale, so nothing on the wire marks it done.
    ///
    /// The open calls end as `calls`, in the order the agent opened them. The caller passes the
    /// status that agrees with the turn's terminal: a call the agent never reported on did not
    /// demonstrably succeed, so a turn that failed closes it as [`ActivityStatus::Failed`] and a
    /// cancelled one as [`ActivityStatus::Cancelled`].
    ///
    /// The plan completes as [`ActivityStatus::Completed`] whatever its entries say. The activity is
    /// the *display* of the plan, and the turn ending is what ends it; reporting `Failed` because some
    /// entry was still pending would claim the agent failed at something it merely did not finish.
    ///
    /// # Example
    ///
    /// ```
    /// use agent_client_protocol::schema::v1::SessionUpdate;
    /// use mango_agent_acp::reducer::Reducer;
    /// use mango_external_agents::event::{ActivityStatus, EventKind};
    ///
    /// let running: SessionUpdate = serde_json::from_value(serde_json::json!({
    ///     "sessionUpdate": "tool_call",
    ///     "toolCallId": "call_1",
    ///     "title": "Run `cargo build`",
    ///     "kind": "execute",
    ///     "status": "in_progress"
    /// }))
    /// .expect("a v1 tool call");
    ///
    /// let mut reducer = Reducer::new();
    /// let _ = reducer.update(running);
    /// let closing = reducer.finish_with(ActivityStatus::Failed);
    /// assert!(matches!(
    ///     closing.as_slice(),
    ///     [EventKind::ActivityCompleted { call_id, result }]
    ///         if call_id == "call_1" && result.status == ActivityStatus::Failed
    /// ));
    /// ```
    pub fn finish_with(&mut self, calls: ActivityStatus) -> Vec<EventKind> {
        let mut events = self.close_reasoning();
        let mut open: Vec<(String, OpenCall)> = self.open_calls.drain().collect();
        open.sort_by_key(|(_, call)| call.opened);
        for (call_id, _) in open {
            self.finished_calls.insert(call_id.clone());
            events.push(EventKind::ActivityCompleted {
                call_id,
                result: ActivityResult::new(calls),
            });
        }
        if std::mem::take(&mut self.plan_started) {
            events.push(EventKind::ActivityCompleted {
                call_id: String::from(PLAN_CALL_ID),
                result: ActivityResult::new(ActivityStatus::Completed),
            });
        }
        events
    }

    fn close_reasoning(&mut self) -> Vec<EventKind> {
        if !std::mem::take(&mut self.reasoning_open) {
            return Vec::new();
        }
        vec![EventKind::ReasoningEnded]
    }

    /// Every event and session fact one frame's own arm produces, with no reasoning bookkeeping.
    ///
    /// Every arm added here needs a matching decision in [`transcript`]: whether the frame is part of
    /// the turn's transcript, and so whether it closes an open reasoning block.
    fn body(&mut self, update: SessionUpdate) -> (Vec<EventKind>, Vec<SessionFact>) {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => (text_delta(chunk), Vec::new()),
            SessionUpdate::AgentThoughtChunk(chunk) => (self.reasoning_delta(chunk), Vec::new()),
            SessionUpdate::ToolCall(call) => (self.tool_call(call), Vec::new()),
            SessionUpdate::ToolCallUpdate(update) => (self.tool_call_update(update), Vec::new()),
            SessionUpdate::Plan(plan) => (self.plan(plan), Vec::new()),
            SessionUpdate::AvailableCommandsUpdate(catalog) => (
                Vec::new(),
                vec![SessionFact::Commands(
                    catalog
                        .available_commands
                        .into_iter()
                        .map(|command| Command {
                            name: command.name,
                            description: Some(command.description),
                        })
                        .collect(),
                )],
            ),
            SessionUpdate::UsageUpdate(usage) => (
                vec![EventKind::ThreadUsage {
                    usage: ThreadUsage {
                        last: None,
                        total: Some(Usage {
                            total_tokens: Some(usage.used),
                            ..Usage::default()
                        }),
                        // The one honest denominator: ACP reports the window alongside what was
                        // used, so a percentage does not have to be guessed from a default.
                        context_window_tokens: Some(usage.size),
                    },
                }],
                Vec::new(),
            ),
            SessionUpdate::ConfigOptionUpdate(update) => (
                Vec::new(),
                vec![SessionFact::ConfigurationOptions(update.config_options)],
            ),
            // Session state rather than transcript, and the `#[non_exhaustive]` tail: an agent that
            // sends an update from a draft feature this build did not opt into is not a failed turn.
            SessionUpdate::UserMessageChunk(_)
            | SessionUpdate::CurrentModeUpdate(_)
            | SessionUpdate::SessionInfoUpdate(_)
            | _ => (Vec::new(), Vec::new()),
        }
    }

    /// A `tool_call` frame: a new bracket, or a revision of one this turn already opened.
    ///
    /// ACP describes `tool_call` as the frame that announces a call, but nothing stops an agent from
    /// sending it again for a call that is still running. A second [`EventKind::ActivityStarted`]
    /// under the same id would give a host two rows for one call, so the repeat arrives as an update
    /// carrying everything the new frame said. A call that already ended stays ended.
    fn tool_call(&mut self, call: ToolCall) -> Vec<EventKind> {
        let call_id = call.tool_call_id.to_string();
        if self.finished_calls.contains(&call_id) {
            return Vec::new();
        }
        if self.open_calls.contains_key(&call_id) {
            let fields = ToolCallUpdateFields::new()
                .kind(call.kind)
                .status(call.status)
                .title(call.title)
                .content(call.content)
                .locations(call.locations);
            return self.tool_call_update(ToolCallUpdate::new(call.tool_call_id, fields));
        }
        let finished = finished(call.status).is_some();
        let events = tool_call(call);
        self.track(call_id, finished);
        events
    }

    /// A `tool_call_update` frame, opening the bracket first when this is the call's first frame.
    ///
    /// ACP permits a `tool_call_update` for a call this client never saw announced — a loaded
    /// session's in-flight call is one — and a host applies updates only to a call it saw start. So
    /// a first sighting through this channel is announced from whatever the update carries, with a
    /// generic title when it names none, and completed in the same breath when it is already over.
    fn tool_call_update(&mut self, update: ToolCallUpdate) -> Vec<EventKind> {
        let call_id = update.tool_call_id.to_string();
        if self.finished_calls.contains(&call_id) {
            return Vec::new();
        }
        if !self.open_calls.contains_key(&call_id) {
            let fields = update.fields;
            let title = fields
                .title
                .filter(|title| !title.trim().is_empty())
                .unwrap_or_else(|| String::from(UNTITLED_TOOL_CALL));
            let mut call = ToolCall::new(update.tool_call_id, title)
                .kind(fields.kind.unwrap_or_default())
                .status(fields.status.unwrap_or_default());
            if let Some(content) = fields.content {
                call = call.content(content);
            }
            if let Some(locations) = fields.locations {
                call = call.locations(locations);
            }
            return self.tool_call(call);
        }
        let events = tool_call_update(update);
        let finished = events
            .iter()
            .any(|event| matches!(event, EventKind::ActivityCompleted { .. }));
        if finished {
            self.open_calls.remove(&call_id);
            self.finished_calls.insert(call_id);
        }
        events
    }

    /// Records a call the host has just been told about, as running or as already over.
    fn track(&mut self, call_id: String, finished: bool) {
        if finished {
            self.finished_calls.insert(call_id);
            return;
        }
        let opened = self.next_call;
        self.next_call = self.next_call.wrapping_add(1);
        self.open_calls.insert(call_id, OpenCall { opened });
    }

    fn reasoning_delta(&mut self, chunk: ContentChunk) -> Vec<EventKind> {
        let Some(text) = plain_text(chunk.content) else {
            return Vec::new();
        };
        let mut events = Vec::with_capacity(2);
        if !self.reasoning_open {
            self.reasoning_open = true;
            events.push(EventKind::ReasoningStarted);
        }
        events.push(EventKind::ReasoningDelta { text });
        events
    }

    fn plan(&mut self, plan: Plan) -> Vec<EventKind> {
        let title = plan_title(&plan);
        let detail = plan_detail(&plan);
        let content = plan_content(&plan);
        if std::mem::replace(&mut self.plan_started, true) {
            return vec![EventKind::ActivityUpdated {
                call_id: String::from(PLAN_CALL_ID),
                update: ActivityUpdate::new()
                    .with_title(title)
                    .with_detail(detail)
                    .with_content(content),
            }];
        }
        vec![EventKind::ActivityStarted {
            call_id: String::from(PLAN_CALL_ID),
            // No `with_item_id`: `PLAN_CALL_ID` is already the event's own call id, minted by this
            // crate rather than sent by the agent, and repeating it as the item id would dress a
            // library-invented id up as something the vendor said.
            activity: Activity::new("plan", ActivityKind::Plan, title)
                .with_detail(detail)
                .with_content(content),
        }]
    }
}

/// Whether this frame is part of the turn's transcript rather than of the session's own state.
///
/// The thought chunk is transcript and is excluded anyway: it is the frame that *opens* a reasoning
/// block, so closing one for it would end every block on its second chunk. Everything listed as
/// `false` is what [`Reducer::body`] drops or reports as a [`SessionFact`] — the `#[non_exhaustive]`
/// tail with it, because a frame this build cannot read is not evidence that the agent stopped
/// thinking.
fn transcript(update: &SessionUpdate) -> bool {
    match update {
        SessionUpdate::AgentThoughtChunk(_)
        | SessionUpdate::UserMessageChunk(_)
        | SessionUpdate::UsageUpdate(_)
        | SessionUpdate::CurrentModeUpdate(_)
        | SessionUpdate::ConfigOptionUpdate(_)
        | SessionUpdate::SessionInfoUpdate(_)
        // The command catalog is session state, like the mode and config updates above: see
        // `SessionFact::Commands`'s own doc comment. It still produces a fact in `body`, it just
        // must not close a reasoning block on its way through.
        | SessionUpdate::AvailableCommandsUpdate(_) => false,
        SessionUpdate::AgentMessageChunk(_)
        | SessionUpdate::ToolCall(_)
        | SessionUpdate::ToolCallUpdate(_)
        | SessionUpdate::Plan(_) => true,
        _ => false,
    }
}

fn text_delta(chunk: ContentChunk) -> Vec<EventKind> {
    match plain_text(chunk.content) {
        Some(text) => vec![EventKind::TextDelta { text }],
        None => Vec::new(),
    }
}

/// The text of a content block, or nothing when it is not text.
///
/// Deliberately narrow. An image, audio block or embedded resource in an *agent message* would have
/// to be rendered into words to become a [`EventKind::TextDelta`], and words the agent did not write
/// are words a host would show as the agent's. 0.1 drops them; when a vendor is seen to use one, it
/// gets an activity of its own rather than invented prose.
fn plain_text(content: ContentBlock) -> Option<String> {
    match content {
        ContentBlock::Text(text) => Some(text.text),
        _ => None,
    }
}

fn tool_call(call: ToolCall) -> Vec<EventKind> {
    let call_id = call.tool_call_id.to_string();
    // The item id and the call id are the same string here — the tension with
    // `Activity::item_id`'s own doc, "distinct from the call", is real: ACP names no id for a tool
    // call's transcript item apart from `tool_call_id`. Carried anyway, so a host can route an
    // update by item id the same way across harnesses, even on the one where the two ids coincide.
    let activity = Activity::new(
        tool_name(&call),
        activity_kind(call.kind),
        call.title.clone(),
    )
    .with_item_id(call_id.clone());
    let activity = match content_detail(&call.content) {
        Some(detail) => activity.with_detail(detail),
        None => activity,
    };
    let activity = match tool_call_content(&call.content) {
        Some(content) => activity.with_content(content),
        None => activity,
    };
    let activity = match locations_extension(&call.locations) {
        Some(extensions) => activity.with_extensions(extensions),
        None => activity,
    };
    let mut events = vec![EventKind::ActivityStarted {
        call_id: call_id.clone(),
        activity,
    }];
    // An agent may report a call that already finished — a cached read, a refusal — in one frame.
    // Announcing it without completing it would leave a spinner running for something that is over.
    if let Some(result) = finished(call.status) {
        events.push(EventKind::ActivityCompleted { call_id, result });
    }
    events
}

fn tool_call_update(update: ToolCallUpdate) -> Vec<EventKind> {
    let call_id = update.tool_call_id.to_string();
    let fields = update.fields;
    // ACP replaces this complete collection. Its absence must therefore leave the activity alone,
    // while an explicit empty (or one carrying no shape this crate renders) clears the content a
    // host retained from the earlier call. `detail` is derived from that same collection, so it is
    // explicitly empty too instead of leaving an old diff path or output line visible.
    let detail = fields
        .content
        .as_deref()
        .map(|content| content_detail(content).unwrap_or_default());
    let content = fields
        .content
        .as_deref()
        .map(|content| tool_call_content(content).unwrap_or(ActivityContent::Empty));
    // `locations` on an update is left uncarried: unlike `Activity`, `ActivityUpdate` has no
    // extensions slot to put a count in, and `raw_input`/`raw_output` never go anywhere — both are
    // unbounded vendor payloads this library forbids carrying.
    if let Some(result) = fields.status.and_then(finished) {
        let result = result.with_optional_detail(detail);
        let result = match content {
            Some(content) => result.with_content(content),
            None => result,
        };
        return vec![EventKind::ActivityCompleted { call_id, result }];
    }
    let update = ActivityUpdate::new()
        .with_optional_title(fields.title)
        .with_optional_detail(detail)
        .with_optional_content(content);
    if update.is_empty() {
        // A status-only move to `in_progress`, or a `locations`/`raw_input` refinement: nothing a
        // host would render differently, and an empty update is noise in a transcript.
        return Vec::new();
    }
    vec![EventKind::ActivityUpdated { call_id, update }]
}

/// The outcome of a terminal tool-call status, or nothing while it is still running.
fn finished(status: ToolCallStatus) -> Option<ActivityResult> {
    let status = match status {
        ToolCallStatus::Completed => ActivityStatus::Completed,
        ToolCallStatus::Failed => ActivityStatus::Failed,
        ToolCallStatus::Pending | ToolCallStatus::InProgress => return None,
        // `#[non_exhaustive]`: a status this build does not know is not a completion, because
        // guessing "completed" would close an activity that is still running.
        _ => return None,
    };
    Some(ActivityResult::new(status))
}

/// The agent's own tool name when it sent one, its title otherwise.
///
/// `ToolCall::name` is behind the crate's `unstable_tool_call_name` feature, which this crate does
/// not enable — so on the stable v1 wire the title is all there is, and
/// [`Activity::name`](mango_external_agents::Activity) carries it rather than being left blank.
fn tool_name(call: &ToolCall) -> String {
    call.title.clone()
}

/// Which neutral bucket an ACP tool kind falls in.
///
/// ACP's `read`, `search`, `think` and `switch_mode` have no counterpart in the neutral set and fall
/// to [`ActivityKind::Other`] rather than being forced into one that picks the wrong icon: a file
/// read is not a file *change*, and mapping it to one would show an edit badge on a read.
#[must_use]
pub fn activity_kind(kind: ToolKind) -> ActivityKind {
    match kind {
        ToolKind::Execute => ActivityKind::Command,
        ToolKind::Edit | ToolKind::Delete | ToolKind::Move => ActivityKind::FileChange,
        ToolKind::Fetch => ActivityKind::WebSearch,
        ToolKind::Read | ToolKind::Search | ToolKind::Think | ToolKind::SwitchMode => {
            ActivityKind::Other
        }
        _ => ActivityKind::Other,
    }
}

/// One line of specifics from a tool call's content, when it carries any.
///
/// A text block wins first, whichever position it is in: the bug this fixed was a diff block ahead
/// of a text block in the list eating the text, because the old `find_map` took the first block of
/// any kind. Failing that, a diff reports its path — not its body, which is one line, bounded by the
/// core on the way through, and a diff is not one line; the body itself survives as a
/// [`FileChange`] row in this crate's own `tool_call_content` instead. Failing that, a terminal
/// contributes its id, same as before.
#[must_use]
pub fn content_detail(content: &[ToolCallContent]) -> Option<String> {
    content_text(content)
        .or_else(|| {
            content_diffs(content)
                .into_iter()
                .next()
                .map(|file| file.path)
        })
        .or_else(|| {
            // A terminal is the host's to own, and this harness declines the capability — so an
            // agent that embeds one has nothing here a host could open. Its id is what it said.
            content.iter().find_map(|block| match block {
                ToolCallContent::Terminal(terminal) => Some(terminal.terminal_id.to_string()),
                _ => None,
            })
        })
}

/// The first text block's own words, when the call carries one.
fn content_text(content: &[ToolCallContent]) -> Option<String> {
    content.iter().find_map(|block| match block {
        ToolCallContent::Content(inner) => plain_text(inner.content.clone()),
        _ => None,
    })
}

/// Every diff block as its own row, in the vendor's own order.
///
/// Never a unified diff synthesised from the two texts: the core forbids computing one
/// representation of a change from another, so `old_text`/`new_text` are carried as ACP sent them
/// and nothing here builds a patch out of them.
///
/// The `kind` falls out of [`FileChange::with_texts`], which reads an absent `oldText` as a new
/// file — ACP's own words for that field are "the original content (None for new files)". It is the
/// one kind in this crate that is read rather than stated, and the caveat is that the schema crate
/// marks `oldText` `DefaultOnError`: an agent sending a malformed one produces the same `None` an
/// omitted one does, and that file is reported as created. Following the protocol's own definition
/// is the documented behaviour; the alternative is dropping the signal for every honest agent to
/// guard against a broken one.
fn content_diffs(content: &[ToolCallContent]) -> Vec<FileChange> {
    content
        .iter()
        .filter_map(|block| match block {
            ToolCallContent::Diff(diff) => Some(
                FileChange::new(diff.path.display().to_string())
                    .with_texts(diff.old_text.clone(), diff.new_text.clone()),
            ),
            _ => None,
        })
        .collect()
}

/// The structured thing a tool call's content blocks describe, when they describe one this crate
/// carries.
///
/// Diff blocks win the slot when the call has any: they are the call's real output, and a text block
/// alongside them is treated as commentary that `detail` already carries. A call with text and no
/// diff gets that text as [`ActivityContent::Output`] too, its own thing rather than only the one
/// bounded line `detail` carries. A call with only a terminal id produces no content — a terminal is
/// the host's to own, and this harness has nothing else to say about it.
#[must_use]
fn tool_call_content(content: &[ToolCallContent]) -> Option<ActivityContent> {
    let files = content_diffs(content);
    if !files.is_empty() {
        return Some(ActivityContent::Diff { files });
    }
    content_text(content).map(|text| ActivityContent::Output { text })
}

/// A bounded, observational count of the files a call names, when it names any.
///
/// `locations` is a list of paths the call touched or will touch, not itself a diff — carrying it
/// whole would duplicate what `tool_call_content` already carries for a call that sends diffs, and
/// invent structure for one that does not. A count is the one honest, bounded fact left.
///
/// Keyed `locationCount` rather than by the vendor's own field name, which is the one place this
/// crate departs from "key it as the vendor spelled it". A key named `locations` holding a number
/// tells a host the paths are in there; the extension channel is scalar-only, so they never can be,
/// and a name that promises a list nothing will ever deliver is worse than a name the vendor did
/// not write.
fn locations_extension(locations: &[ToolCallLocation]) -> Option<Extensions> {
    if locations.is_empty() {
        return None;
    }
    let count = i64::try_from(locations.len()).unwrap_or(i64::MAX);
    Some(Extensions::new().with("locationCount", ExtensionValue::Integer(count)))
}

fn plan_title(plan: &Plan) -> String {
    let total = plan.entries.len();
    let done = plan
        .entries
        .iter()
        .filter(|entry| matches!(entry.status, PlanEntryStatus::Completed))
        .count();
    format!("Plan: {done}/{total} done")
}

/// The plan's current step, or its first, as the one line a host shows next to it.
fn plan_detail(plan: &Plan) -> String {
    plan.entries
        .iter()
        .find(|entry| matches!(entry.status, PlanEntryStatus::InProgress))
        .or_else(|| plan.entries.first())
        .map(|entry| entry.content.clone())
        .unwrap_or_default()
}

/// Every entry as a step, in the vendor's own order.
///
/// ACP's plan carries no id of its own for an entry — the whole plan is replaced wholesale each time
/// it changes, which is why [`PLAN_CALL_ID`] exists — so
/// [`PlanStep::id`](mango_external_agents::PlanStep) is left absent rather than invented from an
/// entry's position, which would silently reassign a step's identity the moment the agent reordered
/// its own list.
fn plan_content(plan: &Plan) -> ActivityContent {
    ActivityContent::Plan {
        steps: plan.entries.iter().map(plan_step).collect(),
    }
}

fn plan_step(entry: &PlanEntry) -> PlanStep {
    let step = PlanStep::new(entry.content.clone()).with_status(plan_step_status(&entry.status));
    match plan_step_priority(&entry.priority) {
        Some(priority) => step.with_priority(priority),
        None => step,
    }
}

fn plan_step_status(status: &PlanEntryStatus) -> PlanStepStatus {
    match status {
        PlanEntryStatus::Pending => PlanStepStatus::Pending,
        PlanEntryStatus::InProgress => PlanStepStatus::InProgress,
        PlanEntryStatus::Completed => PlanStepStatus::Completed,
        // `#[non_exhaustive]`: unreachable from the wire today — `Plan::entries` skips an entry
        // whose status this build cannot parse rather than handing one through — but a future ACP
        // status is not evidence the step was dropped, so it falls back to `PlanStepStatus`'s own
        // `#[default]` rather than to a guess this crate has no basis for.
        _ => PlanStepStatus::Pending,
    }
}

/// How the vendor ranked one entry, or nothing when this build does not know the rank it sent.
///
/// `#[non_exhaustive]`: also unreachable from the wire today, for the same reason as
/// [`plan_step_status`]. A future ACP priority becomes "no ranking" rather than a guessed one —
/// [`PlanStep::priority`](mango_external_agents::PlanStep) is already an [`Option`] for exactly
/// this, an unranked step read as one nobody ranked rather than one ranked low.
fn plan_step_priority(priority: &PlanEntryPriority) -> Option<PlanStepPriority> {
    match priority {
        PlanEntryPriority::High => Some(PlanStepPriority::High),
        PlanEntryPriority::Medium => Some(PlanStepPriority::Medium),
        PlanEntryPriority::Low => Some(PlanStepPriority::Low),
        _ => None,
    }
}

/// Whether a stop reason means somebody stopped the turn rather than the agent finishing it.
///
/// The other four — `end_turn`, `max_tokens`, `max_turn_requests`, `refusal` — are the agent ending
/// its own turn. Only `end_turn` completes it; the other three end it short, which
/// [`stop_failure`] reports.
#[must_use]
pub fn was_cancelled(stop_reason: StopReason) -> bool {
    matches!(stop_reason, StopReason::Cancelled)
}

/// The code a turn ends with when the agent stopped it short of finishing.
pub const TURN_INCOMPLETE_CODE: &str = "vendor-turn-incomplete";

/// The failure a turn ends with when the agent stopped it short, or nothing when it did not.
///
/// ACP's `max_tokens`, `max_turn_requests` and `refusal` are the agent ending its own turn without
/// finishing it: the answer is truncated, or absent. Completing the turn would render that as a
/// success, so it ends as [`TURN_INCOMPLETE_CODE`] with the wire's own spelling of the stop reason as
/// the vendor code. It is not retryable: an identical prompt meets the same limit or the same
/// refusal. `end_turn` and `cancelled` produce nothing here — the first completes the turn and the
/// second is a cancellation (see [`was_cancelled`]).
///
/// # Example
///
/// ```
/// use agent_client_protocol::schema::v1::StopReason;
/// use mango_agent_acp::reducer::stop_failure;
///
/// let failure = stop_failure(StopReason::MaxTokens).expect("a truncated turn is a failure");
/// assert_eq!(failure.code.as_str(), "vendor-turn-incomplete");
/// assert_eq!(failure.vendor_code.as_deref(), Some("max_tokens"));
/// assert!(stop_failure(StopReason::EndTurn).is_none());
/// ```
#[must_use]
pub fn stop_failure(stop_reason: StopReason) -> Option<VendorError> {
    let message = match stop_reason {
        StopReason::EndTurn | StopReason::Cancelled => return None,
        StopReason::MaxTokens => "the agent stopped the turn at its token limit",
        StopReason::MaxTurnRequests => "the agent stopped the turn at its request limit",
        StopReason::Refusal => "the agent refused to continue the turn",
        // `#[non_exhaustive]`: a stop reason this build does not know is still not `end_turn`, and
        // reading it as one would render an unfinished turn as a finished one.
        _ => "the agent ended the turn before finishing it",
    };
    let wire = serde_json::to_value(stop_reason)
        .ok()
        .and_then(|value| value.as_str().map(String::from))
        .unwrap_or_else(|| String::from("unknown"));
    Some(
        VendorError::new(ErrorCode::from_static(TURN_INCOMPLETE_CODE), message)
            .with_vendor_code(wire, false),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        PLAN_CALL_ID, Reducer, SessionFact, TURN_INCOMPLETE_CODE, activity_kind, stop_failure,
        was_cancelled,
    };
    use agent_client_protocol::schema::v1::{SessionUpdate, StopReason, ToolKind};
    use mango_external_agents::event::{
        ActivityKind, ActivityStatus, Command, EventKind, ThreadUsage, Usage,
    };
    use mango_external_agents::{
        ActivityContent, ExtensionValue, FileChangeKind, PlanStepPriority, PlanStepStatus,
    };
    use serde_json::json;

    /// Every case parses the wire rather than building a Rust value, so a rename or a reshape in the
    /// schema crate fails here instead of compiling into a mapping that no longer matches the JSON.
    fn update(value: serde_json::Value) -> SessionUpdate {
        serde_json::from_value(value).expect("expected a v1 session update")
    }

    fn reduce(values: Vec<serde_json::Value>) -> Vec<EventKind> {
        reduce_with_facts(values).0
    }

    /// The `tool_call` frame that announces `call_id`, so a test about updates reads updates to a
    /// call the host already saw start.
    fn announced(call_id: &str) -> serde_json::Value {
        json!({
            "sessionUpdate": "tool_call",
            "toolCallId": call_id,
            "title": "Tool",
            "kind": "other",
            "status": "in_progress"
        })
    }

    fn reduce_with_facts(values: Vec<serde_json::Value>) -> (Vec<EventKind>, Vec<SessionFact>) {
        let mut reducer = Reducer::new();
        let mut events = Vec::new();
        let mut facts = Vec::new();
        for value in values {
            let (frame_events, frame_facts) = reducer.update(update(value));
            events.extend(frame_events);
            facts.extend(frame_facts);
        }
        events.extend(reducer.finish());
        (events, facts)
    }

    #[test]
    fn an_agent_message_chunk_is_assistant_text() {
        assert_eq!(
            reduce(vec![json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "ship it" }
            })]),
            vec![EventKind::TextDelta {
                text: String::from("ship it")
            }]
        );
    }

    /// The host's own prompt comes back as a `user_message_chunk`. Emitting it would duplicate the
    /// message the host already has.
    #[test]
    fn the_agents_echo_of_our_own_prompt_produces_nothing() {
        assert_eq!(
            reduce(vec![json!({
                "sessionUpdate": "user_message_chunk",
                "content": { "type": "text", "text": "say hello" }
            })]),
            Vec::<EventKind>::new()
        );
    }

    /// ACP streams thought chunks with no start or end marker, and the core's pair is what tells a
    /// host "this reasoning phase is empty because it is still running" from "because it is over".
    #[test]
    fn thought_chunks_are_bracketed_by_the_reasoning_pair() {
        let thought = json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": { "type": "text", "text": "weighing it" }
        });
        assert_eq!(
            reduce(vec![
                thought.clone(),
                thought,
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "done" }
                }),
            ]),
            vec![
                EventKind::ReasoningStarted,
                EventKind::ReasoningDelta {
                    text: String::from("weighing it")
                },
                EventKind::ReasoningDelta {
                    text: String::from("weighing it")
                },
                EventKind::ReasoningEnded,
                EventKind::TextDelta {
                    text: String::from("done")
                },
            ]
        );
    }

    /// A frame that produces no transcript must not break a thought in half.
    ///
    /// Agents report usage and mode changes whenever they like, including between two thought
    /// chunks. Closing the reasoning block for one of them would hand a host two collapsed reasoning
    /// panels for a single continuous thought — and `usage_update` in particular arrives on most
    /// turns, so this was not a rare shape.
    #[test]
    fn a_session_state_frame_between_thoughts_does_not_split_the_reasoning_block() {
        let thought = json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": { "type": "text", "text": "weighing it" }
        });
        for interruption in [
            json!({ "sessionUpdate": "usage_update", "used": 1200, "size": 200_000 }),
            json!({ "sessionUpdate": "current_mode_update", "currentModeId": "default" }),
            json!({
                "sessionUpdate": "user_message_chunk",
                "content": { "type": "text", "text": "our own prompt" }
            }),
            json!({
                "sessionUpdate": "available_commands_update",
                "availableCommands": [{ "name": "review", "description": "Review the diff" }]
            }),
        ] {
            let events = reduce(vec![thought.clone(), interruption.clone(), thought.clone()]);
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, EventKind::ReasoningStarted))
                    .count(),
                1,
                "expected one reasoning block across {interruption}, received {events:?}"
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, EventKind::ReasoningEnded))
                    .count(),
                1,
                "expected one reasoning block across {interruption}, received {events:?}"
            );
        }
    }

    /// A turn that ends mid-thought still owes the host the closing half.
    #[test]
    fn a_turn_that_ends_while_reasoning_still_closes_the_block() {
        assert_eq!(
            reduce(vec![json!({
                "sessionUpdate": "agent_thought_chunk",
                "content": { "type": "text", "text": "hmm" }
            })]),
            vec![
                EventKind::ReasoningStarted,
                EventKind::ReasoningDelta {
                    text: String::from("hmm")
                },
                EventKind::ReasoningEnded,
            ]
        );
    }

    #[test]
    fn a_tool_call_is_an_activity_with_the_agents_own_call_id() {
        let events = reduce(vec![json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_1",
            "title": "Run `cargo test`",
            "kind": "execute",
            "status": "in_progress"
        })]);
        let [
            EventKind::ActivityStarted { call_id, activity },
            EventKind::ActivityCompleted { .. },
        ] = events.as_slice()
        else {
            panic!("expected one started activity, received {events:?}");
        };
        assert_eq!(call_id, "call_1");
        assert_eq!(activity.kind, ActivityKind::Command);
        assert_eq!(activity.title, "Run `cargo test`");
        assert_eq!(
            activity.item_id.as_deref(),
            Some("call_1"),
            "expected the call id carried as the item id too"
        );
    }

    /// A call that arrives already finished has to be completed in the same breath, or a host shows
    /// a spinner for something that is over.
    #[test]
    fn a_tool_call_that_arrives_completed_is_started_and_completed_at_once() {
        let events = reduce(vec![json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_2",
            "title": "Read src/lib.rs",
            "kind": "read",
            "status": "completed"
        })]);
        assert_eq!(events.len(), 2, "received {events:?}");
        assert!(matches!(events[0], EventKind::ActivityStarted { .. }));
        let EventKind::ActivityCompleted { call_id, result } = &events[1] else {
            panic!("expected a completion, received {events:?}");
        };
        assert_eq!(call_id, "call_2");
        assert_eq!(result.status, ActivityStatus::Completed);
    }

    #[test]
    fn a_tool_call_update_completes_on_a_terminal_status_and_updates_otherwise() {
        let events = reduce(vec![
            announced("call_3"),
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_3",
                "title": "Editing src/main.rs"
            }),
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_3",
                "status": "failed",
                "content": [{ "type": "content", "content": { "type": "text", "text": "no such file" }}]
            }),
        ]);
        assert!(
            matches!(&events[1], EventKind::ActivityUpdated { call_id, .. } if call_id == "call_3"),
            "received {events:?}"
        );
        let EventKind::ActivityCompleted { result, .. } = &events[2] else {
            panic!("expected a completion, received {events:?}");
        };
        assert_eq!(result.status, ActivityStatus::Failed);
        assert_eq!(result.detail.as_deref(), Some("no such file"));
    }

    /// A move to `in_progress` with nothing else in it changes nothing a host renders, and an empty
    /// update is noise in a transcript.
    ///
    /// `rawInput` rides along on the wire, as an agent that already reported its raw frame on the
    /// original `tool_call` will keep doing: it must not turn a no-op status move into a visible
    /// update, because nothing here ever carries a raw vendor frame.
    #[test]
    fn a_status_only_move_to_in_progress_produces_nothing() {
        let mut reducer = Reducer::new();
        let _ = reducer.update(update(announced("call_4")));
        assert_eq!(
            reducer
                .update(update(json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "call_4",
                    "status": "in_progress",
                    "rawInput": { "command": "cargo test" }
                })))
                .0,
            Vec::<EventKind>::new()
        );
    }

    /// A diff's path still lands in `detail` — a lone diff block is the "as today" case — but its
    /// body no longer does: it survives as a [`FileChange`] row instead, and nothing here synthesises
    /// a unified diff out of the two texts ACP sent.
    #[test]
    fn a_diff_reports_its_path_in_detail_and_its_body_as_a_file_change() {
        let events = reduce(vec![json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_5",
            "title": "Edit",
            "kind": "edit",
            "status": "pending",
            "content": [{
                "type": "diff",
                "path": "/repo/src/lib.rs",
                "oldText": "fn main() {}",
                "newText": "fn main() { println!(\"hi\"); }"
            }]
        })]);
        let [
            EventKind::ActivityStarted { activity, .. },
            EventKind::ActivityCompleted { .. },
        ] = events.as_slice()
        else {
            panic!("expected one activity, received {events:?}");
        };
        assert_eq!(activity.detail.as_deref(), Some("/repo/src/lib.rs"));
        assert_eq!(activity.kind, ActivityKind::FileChange);
        let Some(ActivityContent::Diff { files }) = &activity.content else {
            panic!("expected diff content, received {:?}", activity.content);
        };
        assert_eq!(files.len(), 1, "received {files:?}");
        assert_eq!(files[0].path, "/repo/src/lib.rs");
        assert_eq!(files[0].kind, Some(FileChangeKind::Modified));
        assert_eq!(files[0].old_text.as_deref(), Some("fn main() {}"));
        assert_eq!(
            files[0].new_text.as_deref(),
            Some("fn main() { println!(\"hi\"); }")
        );
        assert_eq!(
            files[0].unified_diff, None,
            "expected no synthesised unified diff"
        );
    }

    /// A diff with no `oldText` is ACP's own way of saying the file is new.
    #[test]
    fn a_diff_with_no_old_text_is_a_created_file() {
        let events = reduce(vec![json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_new",
            "title": "Create",
            "kind": "edit",
            "status": "pending",
            "content": [{ "type": "diff", "path": "/repo/src/new.rs", "newText": "fn new() {}" }]
        })]);
        let [
            EventKind::ActivityStarted { activity, .. },
            EventKind::ActivityCompleted { .. },
        ] = events.as_slice()
        else {
            panic!("expected one activity, received {events:?}");
        };
        let Some(ActivityContent::Diff { files }) = &activity.content else {
            panic!("expected diff content, received {:?}", activity.content);
        };
        assert_eq!(files[0].kind, Some(FileChangeKind::Created));
        assert_eq!(files[0].old_text, None);
    }

    /// The bug this whole mapping exists to fix: a text block and a diff block used to cost a host
    /// one of the two, because the old lookup took only the first content block. Now the text stays
    /// in `detail` and the diff's files survive as their own content, whichever order ACP sent them.
    #[test]
    fn a_tool_call_with_text_and_a_diff_keeps_both() {
        let events = reduce(vec![json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_6",
            "title": "Edit",
            "kind": "edit",
            "status": "pending",
            "content": [
                { "type": "diff", "path": "/repo/src/lib.rs", "newText": "fn main() {}" },
                { "type": "content", "content": { "type": "text", "text": "rewrote the entry point" } }
            ]
        })]);
        let [
            EventKind::ActivityStarted { activity, .. },
            EventKind::ActivityCompleted { .. },
        ] = events.as_slice()
        else {
            panic!("expected one activity, received {events:?}");
        };
        assert_eq!(
            activity.detail.as_deref(),
            Some("rewrote the entry point"),
            "expected the text, not the diff's path, once a text block is present"
        );
        let Some(ActivityContent::Diff { files }) = &activity.content else {
            panic!(
                "expected the diff to survive as content, received {:?}",
                activity.content
            );
        };
        assert_eq!(files.len(), 1, "received {files:?}");
        assert_eq!(files[0].path, "/repo/src/lib.rs");
    }

    /// A call with text and no diff gets that text as `Output` content too, not only as the one
    /// bounded line `detail` carries.
    #[test]
    fn a_text_only_tool_call_carries_its_body_as_output_content() {
        let events = reduce(vec![json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_7",
            "title": "Run",
            "kind": "execute",
            "status": "pending",
            "content": [{ "type": "content", "content": { "type": "text", "text": "2 tests passed" } }]
        })]);
        let [
            EventKind::ActivityStarted { activity, .. },
            EventKind::ActivityCompleted { .. },
        ] = events.as_slice()
        else {
            panic!("expected one activity, received {events:?}");
        };
        assert_eq!(activity.detail.as_deref(), Some("2 tests passed"));
        assert_eq!(
            activity.content,
            Some(ActivityContent::Output {
                text: String::from("2 tests passed")
            })
        );
    }

    /// A call that names files it touches, without a diff, gets that as a bounded count rather than
    /// the raw path list — `locations` is not itself a diff, and the paths belong in a `FileChange`
    /// row, not a scalar map.
    #[test]
    fn a_tool_calls_locations_become_a_bounded_count() {
        let events = reduce(vec![json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_8",
            "title": "Search",
            "kind": "search",
            "status": "pending",
            "locations": [{ "path": "/repo/src/a.rs" }, { "path": "/repo/src/b.rs" }]
        })]);
        let [
            EventKind::ActivityStarted { activity, .. },
            EventKind::ActivityCompleted { .. },
        ] = events.as_slice()
        else {
            panic!("expected one activity, received {events:?}");
        };
        assert_eq!(
            activity.extensions.get("locationCount"),
            Some(&ExtensionValue::Integer(2)),
            "received {:?}",
            activity.extensions
        );
    }

    /// A tool call that names no locations must not invent a `locations` extension entry for zero.
    #[test]
    fn a_tool_call_with_no_locations_carries_no_locations_extension() {
        let events = reduce(vec![json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_8b",
            "title": "Search",
            "kind": "search",
            "status": "pending"
        })]);
        let [
            EventKind::ActivityStarted { activity, .. },
            EventKind::ActivityCompleted { .. },
        ] = events.as_slice()
        else {
            panic!("expected one activity, received {events:?}");
        };
        assert!(
            activity.extensions.is_empty(),
            "received {:?}",
            activity.extensions
        );
    }

    /// A `tool_call_update` that only changes content — no title, no terminal status — still has to
    /// reach a host: before this, only `title`/`detail` were checked for "is this update empty",
    /// which would have dropped a content-only revision on the floor.
    #[test]
    fn a_tool_call_update_that_only_changes_content_still_produces_an_update() {
        let events = reduce(vec![
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call_9",
                "title": "Edit",
                "kind": "edit",
                "status": "in_progress"
            }),
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_9",
                "content": [{ "type": "diff", "path": "/repo/src/lib.rs", "newText": "fn main() {}" }]
            }),
        ]);
        let update_event = events.iter().find(
            |event| matches!(event, EventKind::ActivityUpdated { call_id, .. } if call_id == "call_9"),
        );
        let Some(EventKind::ActivityUpdated { update, .. }) = update_event else {
            panic!("expected a content-only update, received {events:?}");
        };
        assert!(update.title.is_none());
        let Some(ActivityContent::Diff { files }) = &update.content else {
            panic!(
                "expected diff content on the update, received {:?}",
                update.content
            );
        };
        assert_eq!(files[0].path, "/repo/src/lib.rs");
    }

    /// ACP replaces a tool call's complete content collection. An empty replacement must therefore
    /// reach the host instead of being mistaken for an omitted `content` field, which retains what
    /// the host already rendered.
    #[test]
    fn an_empty_tool_call_content_replacement_clears_the_activity() {
        let events = reduce(vec![
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call_clear",
                "title": "Edit",
                "kind": "edit",
                "status": "in_progress",
                "content": [{
                    "type": "diff",
                    "path": "/repo/src/lib.rs",
                    "newText": "fn main() {}"
                }]
            }),
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_clear",
                "content": []
            }),
        ]);
        let Some(EventKind::ActivityUpdated { update, .. }) = events.get(1) else {
            panic!("expected an activity update that clears content, received {events:?}");
        };
        assert!(
            update.content == Some(ActivityContent::Empty),
            "expected an explicit content clear, received {update:?}"
        );
        assert_eq!(update.detail.as_deref(), Some(""));
    }

    /// `content` being absent is ACP leaving that collection unchanged. A later title update must
    /// therefore carry neither an empty marker nor an empty detail that a host would read as a
    /// replacement of the earlier diff.
    #[test]
    fn an_omitted_tool_call_content_update_retains_the_earlier_content() {
        let events = reduce(vec![
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call_retain",
                "title": "Edit",
                "kind": "edit",
                "status": "in_progress",
                "content": [{
                    "type": "diff",
                    "path": "/repo/src/lib.rs",
                    "newText": "fn main() {}"
                }]
            }),
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_retain",
                "title": "Editing src/lib.rs"
            }),
        ]);
        let Some(EventKind::ActivityUpdated { update, .. }) = events.get(1) else {
            panic!("expected a title update that retains content, received {events:?}");
        };
        assert_eq!(update.title.as_deref(), Some("Editing src/lib.rs"));
        assert_eq!(update.content, None);
        assert_eq!(update.detail, None);
    }

    /// A terminal ACP update replaces its content collection just like a running one. The result
    /// keeps the explicit empty marker so a host can distinguish it from a terminal update that
    /// supplied no content field at all.
    #[test]
    fn an_empty_terminal_tool_call_content_replacement_carries_empty_result_content() {
        let events = reduce(vec![
            announced("call_complete_empty"),
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_complete_empty",
                "status": "completed",
                "content": []
            }),
        ]);
        let [_, EventKind::ActivityCompleted { result, .. }] = events.as_slice() else {
            panic!("expected one completion, received {events:?}");
        };
        assert_eq!(result.content, Some(ActivityContent::Empty));
        assert_eq!(result.detail.as_deref(), Some(""));
    }

    /// The `ActivityCompleted` a terminal status produces carries the same content the update did,
    /// not just its detail.
    #[test]
    fn a_tool_call_update_that_completes_with_content_carries_it_on_the_result() {
        let events = reduce(vec![
            announced("call_10"),
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_10",
                "status": "completed",
                "content": [{ "type": "diff", "path": "/repo/src/lib.rs", "newText": "fn main() {}" }]
            }),
        ]);
        let [_, EventKind::ActivityCompleted { result, .. }] = events.as_slice() else {
            panic!("expected one completion, received {events:?}");
        };
        assert_eq!(result.status, ActivityStatus::Completed);
        let Some(ActivityContent::Diff { files }) = &result.content else {
            panic!(
                "expected diff content on the result, received {:?}",
                result.content
            );
        };
        assert_eq!(files[0].path, "/repo/src/lib.rs");
    }

    /// ACP lets an agent report a call through `tool_call_update` alone — a loaded session's
    /// in-flight call, or an agent that skips the opening frame. A host applies an update only to a
    /// call it saw start, so without a synthesised start the whole call would be invisible.
    #[test]
    fn a_tool_call_first_seen_through_an_update_opens_its_own_activity() {
        let events = reduce(vec![
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_late",
                "title": "Read src/lib.rs",
                "kind": "execute",
                "status": "in_progress",
                "content": [{ "type": "content", "content": { "type": "text", "text": "reading" } }]
            }),
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_late",
                "status": "completed"
            }),
        ]);
        let [
            EventKind::ActivityStarted { call_id, activity },
            EventKind::ActivityCompleted {
                call_id: completed,
                result,
            },
        ] = events.as_slice()
        else {
            panic!(
                "expected a start and a completion for the update-first call, received {events:?}"
            );
        };
        assert_eq!(call_id, "call_late");
        assert_eq!(completed, "call_late");
        assert_eq!(activity.title, "Read src/lib.rs");
        assert_eq!(activity.kind, ActivityKind::Command);
        assert_eq!(activity.detail.as_deref(), Some("reading"));
        assert_eq!(activity.item_id.as_deref(), Some("call_late"));
        assert_eq!(result.status, ActivityStatus::Completed);
    }

    /// A terminal update for a call nobody saw start still needs both halves of the bracket, and
    /// an update that names no title falls back to a generic one rather than an empty label.
    #[test]
    fn an_untitled_update_first_call_that_arrives_finished_is_started_and_completed() {
        let events = reduce(vec![json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_blind",
            "status": "failed"
        })]);
        let [
            EventKind::ActivityStarted { activity, .. },
            EventKind::ActivityCompleted { result, .. },
        ] = events.as_slice()
        else {
            panic!("expected a start and a completion, received {events:?}");
        };
        assert_eq!(activity.title, "tool");
        assert_eq!(activity.kind, ActivityKind::Other);
        assert_eq!(result.status, ActivityStatus::Failed);
    }

    /// A second `tool_call` for a call that is still open is a revision of it. Starting it again
    /// would hand a host two rows for one call, which the core's conformance suite refuses.
    #[test]
    fn a_repeated_tool_call_for_an_open_call_updates_it_instead_of_starting_it_twice() {
        let events = reduce(vec![
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call_twice",
                "title": "Run",
                "kind": "execute",
                "status": "pending"
            }),
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call_twice",
                "title": "Run `cargo test`",
                "kind": "execute",
                "status": "in_progress"
            }),
        ]);
        let starts = events
            .iter()
            .filter(|event| matches!(event, EventKind::ActivityStarted { .. }))
            .count();
        assert_eq!(
            starts, 1,
            "expected one start for one call, received {events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                EventKind::ActivityUpdated { update, .. }
                    if update.title.as_deref() == Some("Run `cargo test`")
            )),
            "expected the repeated frame to arrive as an update, received {events:?}"
        );
    }

    /// A frame for a call that already ended cannot reopen it: the host closed that row, and a
    /// second bracket under the same id would render one call twice.
    #[test]
    fn a_late_update_for_a_finished_call_is_dropped() {
        let events = reduce(vec![
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call_done",
                "title": "Read",
                "kind": "read",
                "status": "completed"
            }),
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_done",
                "title": "Read again"
            }),
        ]);
        assert_eq!(
            events.len(),
            2,
            "expected only the original bracket, received {events:?}"
        );
    }

    /// ACP ends a turn with the `session/prompt` response, not with a frame per call, so a call the
    /// agent never reported as ended would otherwise stay running in the host's transcript for
    /// good. The turn's end closes it, in the order the agent opened its calls.
    #[test]
    fn a_call_the_agent_never_ended_is_completed_when_the_turn_completes() {
        let events = reduce(vec![
            announced("call_first"),
            announced("call_second"),
            announced("call_ended"),
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_ended",
                "status": "completed"
            }),
        ]);
        let closed: Vec<(&str, ActivityStatus)> = events
            .iter()
            .skip(4)
            .filter_map(|event| match event {
                EventKind::ActivityCompleted { call_id, result } => {
                    Some((call_id.as_str(), result.status))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            closed,
            vec![
                ("call_first", ActivityStatus::Completed),
                ("call_second", ActivityStatus::Completed),
            ],
            "expected the two unended calls closed at the turn's end, received {events:?}"
        );
    }

    /// A turn that did not complete does not get to claim its unended calls succeeded: they close
    /// with the status the caller passes, while the plan still completes because the turn ending is
    /// what ends its display.
    #[test]
    fn unended_calls_take_the_turns_status_and_the_plan_still_completes() {
        for status in [ActivityStatus::Failed, ActivityStatus::Cancelled] {
            let mut reducer = Reducer::new();
            let _ = reducer.update(update(announced("call_open")));
            let _ = reducer.update(update(json!({
                "sessionUpdate": "plan",
                "entries": [{ "content": "build", "priority": "high", "status": "pending" }]
            })));
            let closing = reducer.finish_with(status);
            let statuses: Vec<(&str, ActivityStatus)> = closing
                .iter()
                .filter_map(|event| match event {
                    EventKind::ActivityCompleted { call_id, result } => {
                        Some((call_id.as_str(), result.status))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                statuses,
                vec![
                    ("call_open", status),
                    (PLAN_CALL_ID, ActivityStatus::Completed)
                ],
                "expected the call to take {status:?} and the plan to complete, received {closing:?}"
            );
            assert!(
                reducer.finish_with(status).is_empty(),
                "expected a second finish to owe nothing"
            );
        }
    }

    /// ACP v1's plan is replaced wholesale and carries no id, so the first one opens an activity and
    /// every revision updates that same one.
    #[test]
    fn the_plan_opens_once_and_revises_under_one_call_id() {
        let first = json!({
            "sessionUpdate": "plan",
            "entries": [
                { "content": "read the code", "priority": "high", "status": "in_progress" },
                { "content": "write the test", "priority": "medium", "status": "pending" }
            ]
        });
        let second = json!({
            "sessionUpdate": "plan",
            "entries": [
                { "content": "read the code", "priority": "high", "status": "completed" },
                { "content": "write the test", "priority": "medium", "status": "in_progress" }
            ]
        });
        let events = reduce(vec![first, second]);

        let [
            EventKind::ActivityStarted { call_id, activity },
            EventKind::ActivityUpdated {
                call_id: revised,
                update,
            },
            // ACP never marks a plan done — it replaces it — so the turn's end is what completes the
            // activity. Without it a host renders a plan that runs forever.
            EventKind::ActivityCompleted {
                call_id: completed,
                result,
            },
        ] = events.as_slice()
        else {
            panic!("expected a start, an update and a completion, received {events:?}");
        };
        assert_eq!(call_id, PLAN_CALL_ID);
        assert_eq!(revised, PLAN_CALL_ID);
        assert_eq!(activity.kind, ActivityKind::Plan);
        assert_eq!(activity.title, "Plan: 0/2 done");
        assert_eq!(activity.detail.as_deref(), Some("read the code"));
        assert_eq!(
            activity.item_id, None,
            "expected no invented item id for a plan the agent gave no id of its own"
        );
        assert_eq!(update.title.as_deref(), Some("Plan: 1/2 done"));
        assert_eq!(update.detail.as_deref(), Some("write the test"));
        assert_eq!(completed, PLAN_CALL_ID);
        assert_eq!(result.status, ActivityStatus::Completed);

        // The steps, not just the title and detail a host already rendered: this is what makes a
        // revision a plan again instead of only a new sentence.
        let Some(ActivityContent::Plan { steps }) = &activity.content else {
            panic!(
                "expected plan content on the start, received {:?}",
                activity.content
            );
        };
        assert_eq!(steps.len(), 2, "received {steps:?}");
        assert_eq!(steps[0].id, None, "expected no id ACP never sent");
        assert_eq!(steps[0].title, "read the code");
        assert_eq!(steps[0].status, PlanStepStatus::InProgress);
        assert_eq!(steps[0].priority, Some(PlanStepPriority::High));
        assert_eq!(steps[1].status, PlanStepStatus::Pending);
        assert_eq!(steps[1].priority, Some(PlanStepPriority::Medium));

        let Some(ActivityContent::Plan {
            steps: revised_steps,
        }) = &update.content
        else {
            panic!(
                "expected plan content on the revision, received {:?}",
                update.content
            );
        };
        assert_eq!(revised_steps[0].status, PlanStepStatus::Completed);
        assert_eq!(revised_steps[1].status, PlanStepStatus::InProgress);
    }

    /// The command catalog is session state now, not a turn event: it is reported as a
    /// [`SessionFact`] rather than folded into the event vector.
    ///
    /// The name reaches the host bare. Invocation is `/` plus the name, so the sigil belongs to
    /// whoever renders it — and a name with a `/` inside it, like a scoped plugin command, would not
    /// survive being re-slugged.
    #[test]
    fn the_command_catalog_keeps_the_agents_own_spelling_and_is_a_session_fact() {
        let (events, facts) = reduce_with_facts(vec![json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": [
                { "name": "create_plan", "description": "Draft a plan" },
                { "name": "my-plugin:review", "description": "Review the diff" }
            ]
        })]);
        assert_eq!(events, Vec::<EventKind>::new(), "expected no turn event");
        assert_eq!(
            facts,
            vec![SessionFact::Commands(vec![
                Command {
                    name: String::from("create_plan"),
                    description: Some(String::from("Draft a plan")),
                },
                Command {
                    name: String::from("my-plugin:review"),
                    description: Some(String::from("Review the diff")),
                },
            ])]
        );
    }

    /// `size` is the only honest denominator for a context percentage, so it is carried rather than
    /// left for a host to guess at.
    #[test]
    fn a_usage_update_carries_the_window_it_was_measured_against() {
        assert_eq!(
            reduce(vec![json!({
                "sessionUpdate": "usage_update",
                "used": 12_000,
                "size": 200_000
            })]),
            vec![EventKind::ThreadUsage {
                usage: ThreadUsage {
                    last: None,
                    total: Some(Usage {
                        total_tokens: Some(12_000),
                        ..Usage::default()
                    }),
                    context_window_tokens: Some(200_000),
                }
            }]
        );
    }

    #[test]
    fn session_state_updates_are_not_transcript_events() {
        assert_eq!(
            reduce(vec![
                json!({ "sessionUpdate": "current_mode_update", "currentModeId": "plan" }),
                json!({ "sessionUpdate": "session_info_update", "title": "Refactor" }),
            ]),
            Vec::<EventKind>::new()
        );
    }

    /// An image block in an *agent message* is dropped rather than rendered into words: prose the
    /// agent did not write is prose a host would attribute to it. Pinned so the limit is a decision
    /// with a test on it rather than an oversight.
    #[test]
    fn a_non_text_block_in_an_agent_message_is_dropped_rather_than_described() {
        assert_eq!(
            reduce(vec![json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "image", "data": "aGk=", "mimeType": "image/png" }
            })]),
            Vec::<EventKind>::new()
        );
    }

    #[test]
    fn every_tool_kind_picks_an_icon_without_claiming_a_change_that_did_not_happen() {
        let cases = [
            (ToolKind::Execute, ActivityKind::Command),
            (ToolKind::Edit, ActivityKind::FileChange),
            (ToolKind::Delete, ActivityKind::FileChange),
            (ToolKind::Move, ActivityKind::FileChange),
            (ToolKind::Fetch, ActivityKind::WebSearch),
            (ToolKind::Read, ActivityKind::Other),
            (ToolKind::Search, ActivityKind::Other),
            (ToolKind::Think, ActivityKind::Other),
            (ToolKind::SwitchMode, ActivityKind::Other),
            (ToolKind::Other, ActivityKind::Other),
        ];
        for (acp, expected) in cases {
            assert_eq!(
                activity_kind(acp),
                expected,
                "expected {expected:?} for {acp:?}"
            );
        }
    }

    /// Only a cancellation is somebody stopping the turn. A refusal or a limit is the agent ending
    /// its own — short, which [`stop_failure`] reports, but not a cancellation.
    #[test]
    fn only_a_cancelled_stop_reason_counts_as_a_cancellation() {
        assert!(was_cancelled(StopReason::Cancelled));
        for reason in [
            StopReason::EndTurn,
            StopReason::MaxTokens,
            StopReason::MaxTurnRequests,
            StopReason::Refusal,
        ] {
            assert!(!was_cancelled(reason), "received {reason:?}");
        }
    }

    /// A refused, token-limited or request-limited turn is not a success: its answer is truncated or
    /// absent. Each ends as the incomplete-turn failure carrying the wire's own stop reason, while a
    /// finished or cancelled turn produces no failure at all.
    #[test]
    fn a_stop_short_of_the_end_is_an_incomplete_turn_naming_its_reason() {
        for (reason, wire) in [
            (StopReason::MaxTokens, "max_tokens"),
            (StopReason::MaxTurnRequests, "max_turn_requests"),
            (StopReason::Refusal, "refusal"),
        ] {
            let failure = stop_failure(reason)
                .unwrap_or_else(|| panic!("expected a failure for {reason:?}, received none"));
            assert_eq!(failure.code.as_str(), TURN_INCOMPLETE_CODE);
            assert_eq!(
                failure.vendor_code.as_deref(),
                Some(wire),
                "expected the wire spelling for {reason:?}, received {failure:?}"
            );
            assert!(
                !failure.retryable,
                "expected {reason:?} not to invite a retry"
            );
        }
        for reason in [StopReason::EndTurn, StopReason::Cancelled] {
            assert!(
                stop_failure(reason).is_none(),
                "expected no failure for {reason:?}"
            );
        }
    }
}
