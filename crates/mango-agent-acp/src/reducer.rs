//! `session/update` in, [`EventKind`] out.
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
//! * **Session state is not transcript.** The mode, config-option and session-info updates describe
//!   the session rather than the turn, and 0.1 drops them rather than inventing transcript events
//!   for them.
//!
//! ACP v1 reference: <https://agentclientprotocol.com/protocol/v1/prompt-turn>

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, Plan, PlanEntryStatus, SessionUpdate, StopReason, ToolCall,
    ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolKind,
};
use mango_external_agents::event::{
    Activity, ActivityKind, ActivityResult, ActivityStatus, ActivityUpdate, Command, EventKind,
    ThreadUsage, Usage,
};

/// The call id every plan update shares.
///
/// ACP v1's `plan` update carries the whole plan and no identity of its own — the plan is a property
/// of the session, replaced wholesale each time it changes. The activity vocabulary needs a call id,
/// so the plan gets one constant one and its revisions arrive as updates to it. Prefixed to stay
/// clear of an agent's own tool call ids.
pub const PLAN_CALL_ID: &str = "acp:plan";

/// Turns one agent's frames into events, remembering only what a frame alone cannot say.
///
/// Two things: whether a reasoning block is open (ACP streams thought chunks with no start or end
/// marker, and [`EventKind::ReasoningStarted`]/[`EventKind::ReasoningEnded`] are a pair a host
/// relies on), and whether the plan activity has been announced yet.
#[derive(Debug, Default)]
pub struct Reducer {
    reasoning_open: bool,
    plan_started: bool,
}

impl Reducer {
    /// A reducer for one turn.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The events one `session/update` produces, in order.
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
    /// assert_eq!(
    ///     reducer.update(update),
    ///     vec![EventKind::TextDelta { text: String::from("hello") }]
    /// );
    /// ```
    pub fn update(&mut self, update: SessionUpdate) -> Vec<EventKind> {
        // A transcript frame closes an open reasoning block: a host that never saw `ReasoningEnded`
        // would render a reasoning phase that stays open for the rest of the turn. A frame that
        // produces no transcript must *not*, or a usage report or a mode change arriving between two
        // thought chunks would split one continuous thought into two rendered blocks.
        let mut events = match transcript(&update) {
            true => self.close_reasoning(),
            false => Vec::new(),
        };
        events.extend(self.body(update));
        events
    }

    /// The events that close out a turn, before its terminal.
    ///
    /// Two debts a turn can end owing. A turn that stopped mid-thought owes the other half of the
    /// reasoning pair; and a turn that opened the plan activity owes its completion, because ACP never
    /// sends one — the plan is session state that is replaced wholesale, so nothing on the wire marks
    /// it done. Without this, every turn that wrote a plan leaves a host rendering an activity that
    /// runs forever.
    ///
    /// The plan completes as [`ActivityStatus::Completed`] whatever its entries say. The activity is
    /// the *display* of the plan, and the turn ending is what ends it; reporting `Failed` because some
    /// entry was still pending would claim the agent failed at something it merely did not finish.
    pub fn finish(&mut self) -> Vec<EventKind> {
        let mut events = self.close_reasoning();
        if std::mem::take(&mut self.plan_started) {
            events.push(EventKind::ActivityCompleted {
                call_id: String::from(PLAN_CALL_ID),
                result: ActivityResult {
                    status: ActivityStatus::Completed,
                    detail: None,
                    truncated: false,
                },
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

    /// The events one frame's own arm produces, with no reasoning bookkeeping.
    ///
    /// Every arm added here needs a matching decision in [`transcript`]: whether the frame is part of
    /// the turn's transcript, and so whether it closes an open reasoning block.
    fn body(&mut self, update: SessionUpdate) -> Vec<EventKind> {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => text_delta(chunk),
            SessionUpdate::AgentThoughtChunk(chunk) => self.reasoning_delta(chunk),
            SessionUpdate::ToolCall(call) => tool_call(call),
            SessionUpdate::ToolCallUpdate(update) => tool_call_update(update),
            SessionUpdate::Plan(plan) => self.plan(plan),
            SessionUpdate::AvailableCommandsUpdate(catalog) => {
                vec![EventKind::CommandsAvailable {
                    commands: catalog
                        .available_commands
                        .into_iter()
                        .map(|command| Command {
                            name: command.name,
                            description: Some(command.description),
                        })
                        .collect(),
                }]
            }
            SessionUpdate::UsageUpdate(usage) => vec![EventKind::ThreadUsage {
                usage: ThreadUsage {
                    last: None,
                    total: Some(Usage {
                        total_tokens: Some(usage.used),
                        ..Usage::default()
                    }),
                    // The one honest denominator: ACP reports the window alongside what was used,
                    // so a percentage does not have to be guessed from a default.
                    context_window_tokens: Some(usage.size),
                },
            }],
            // Session state rather than transcript, and the `#[non_exhaustive]` tail: an agent that
            // sends an update from a draft feature this build did not opt into is not a failed turn.
            SessionUpdate::UserMessageChunk(_)
            | SessionUpdate::CurrentModeUpdate(_)
            | SessionUpdate::ConfigOptionUpdate(_)
            | SessionUpdate::SessionInfoUpdate(_)
            | _ => Vec::new(),
        }
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
        if std::mem::replace(&mut self.plan_started, true) {
            return vec![EventKind::ActivityUpdated {
                call_id: String::from(PLAN_CALL_ID),
                update: ActivityUpdate {
                    title: Some(title),
                    detail: Some(detail),
                    truncated: false,
                },
            }];
        }
        vec![EventKind::ActivityStarted {
            call_id: String::from(PLAN_CALL_ID),
            activity: Activity {
                name: String::from("plan"),
                kind: ActivityKind::Plan,
                title,
                detail: Some(detail),
                truncated: false,
            },
        }]
    }
}

/// Whether this frame is part of the turn's transcript rather than of the session's own state.
///
/// The thought chunk is transcript and is excluded anyway: it is the frame that *opens* a reasoning
/// block, so closing one for it would end every block on its second chunk. Everything listed as
/// `false` is what [`Reducer::body`] drops — the `#[non_exhaustive]` tail with it, because a frame
/// this build cannot read is not evidence that the agent stopped thinking.
fn transcript(update: &SessionUpdate) -> bool {
    match update {
        SessionUpdate::AgentThoughtChunk(_)
        | SessionUpdate::UserMessageChunk(_)
        | SessionUpdate::UsageUpdate(_)
        | SessionUpdate::CurrentModeUpdate(_)
        | SessionUpdate::ConfigOptionUpdate(_)
        | SessionUpdate::SessionInfoUpdate(_)
        // The command catalog is session state, like the mode and config updates above: see
        // `EventKind::CommandsAvailable`'s own doc comment. It still produces an event in `body`,
        // it just must not close a reasoning block on its way through.
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
    let mut events = vec![EventKind::ActivityStarted {
        call_id: call_id.clone(),
        activity: Activity {
            name: tool_name(&call),
            kind: activity_kind(call.kind),
            title: call.title.clone(),
            detail: content_detail(&call.content),
            truncated: false,
        },
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
    let detail = fields.content.as_deref().and_then(content_detail);
    if let Some(result) = fields.status.and_then(finished) {
        return vec![EventKind::ActivityCompleted {
            call_id,
            result: ActivityResult { detail, ..result },
        }];
    }
    if fields.title.is_none() && detail.is_none() {
        // A status-only move to `in_progress`, or a `locations`/`raw_input` refinement: nothing a
        // host would render differently, and an empty update is noise in a transcript.
        return Vec::new();
    }
    vec![EventKind::ActivityUpdated {
        call_id,
        update: ActivityUpdate {
            title: fields.title,
            detail,
            truncated: false,
        },
    }]
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
    Some(ActivityResult {
        status,
        detail: None,
        truncated: false,
    })
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
/// The first block only: the content list is the whole output of a tool and the neutral
/// [`Activity::detail`](mango_external_agents::Activity) is one line, bounded by the core on the way
/// through. A diff reports its path rather than its body for the same reason.
#[must_use]
pub fn content_detail(content: &[ToolCallContent]) -> Option<String> {
    content.iter().find_map(|block| match block {
        ToolCallContent::Content(inner) => plain_text(inner.content.clone()),
        ToolCallContent::Diff(diff) => Some(diff.path.display().to_string()),
        // A terminal is the host's to own, and this harness declines the capability — so an agent
        // that embeds one has nothing here a host could open. Its id is what it said.
        ToolCallContent::Terminal(terminal) => Some(terminal.terminal_id.to_string()),
        _ => None,
    })
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

/// Whether a stop reason means somebody stopped the turn rather than the agent finishing it.
///
/// The other four — `end_turn`, `max_tokens`, `max_turn_requests`, `refusal` — are the agent ending
/// its own turn, so they complete it. A refusal in particular is not a failure of the link, and
/// reporting one as [`EventKind::Error`] would tell a host to retry something the agent decided.
#[must_use]
pub fn was_cancelled(stop_reason: StopReason) -> bool {
    matches!(stop_reason, StopReason::Cancelled)
}

#[cfg(test)]
mod tests {
    use super::{PLAN_CALL_ID, Reducer, activity_kind, was_cancelled};
    use agent_client_protocol::schema::v1::{SessionUpdate, StopReason, ToolKind};
    use mango_external_agents::event::{
        ActivityKind, ActivityStatus, Command, EventKind, ThreadUsage, Usage,
    };
    use serde_json::json;

    /// Every case parses the wire rather than building a Rust value, so a rename or a reshape in the
    /// schema crate fails here instead of compiling into a mapping that no longer matches the JSON.
    fn update(value: serde_json::Value) -> SessionUpdate {
        serde_json::from_value(value).expect("expected a v1 session update")
    }

    fn reduce(values: Vec<serde_json::Value>) -> Vec<EventKind> {
        let mut reducer = Reducer::new();
        let mut events: Vec<EventKind> = values
            .into_iter()
            .flat_map(|value| reducer.update(update(value)))
            .collect();
        events.extend(reducer.finish());
        events
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
        let [EventKind::ActivityStarted { call_id, activity }] = events.as_slice() else {
            panic!("expected one started activity, received {events:?}");
        };
        assert_eq!(call_id, "call_1");
        assert_eq!(activity.kind, ActivityKind::Command);
        assert_eq!(activity.title, "Run `cargo test`");
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
            matches!(&events[0], EventKind::ActivityUpdated { call_id, .. } if call_id == "call_3"),
            "received {events:?}"
        );
        let EventKind::ActivityCompleted { result, .. } = &events[1] else {
            panic!("expected a completion, received {events:?}");
        };
        assert_eq!(result.status, ActivityStatus::Failed);
        assert_eq!(result.detail.as_deref(), Some("no such file"));
    }

    /// A move to `in_progress` with nothing else in it changes nothing a host renders, and an empty
    /// update is noise in a transcript.
    #[test]
    fn a_status_only_move_to_in_progress_produces_nothing() {
        assert_eq!(
            reduce(vec![json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_4",
                "status": "in_progress"
            })]),
            Vec::<EventKind>::new()
        );
    }

    #[test]
    fn a_diff_reports_its_path_rather_than_its_body() {
        let events = reduce(vec![json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_5",
            "title": "Edit",
            "kind": "edit",
            "status": "pending",
            "content": [{ "type": "diff", "path": "/repo/src/lib.rs", "newText": "fn main() {}" }]
        })]);
        let [EventKind::ActivityStarted { activity, .. }] = events.as_slice() else {
            panic!("expected one activity, received {events:?}");
        };
        assert_eq!(activity.detail.as_deref(), Some("/repo/src/lib.rs"));
        assert_eq!(activity.kind, ActivityKind::FileChange);
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
        assert_eq!(update.title.as_deref(), Some("Plan: 1/2 done"));
        assert_eq!(update.detail.as_deref(), Some("write the test"));
        assert_eq!(completed, PLAN_CALL_ID);
        assert_eq!(result.status, ActivityStatus::Completed);
    }

    /// The name reaches the host bare. Invocation is `/` plus the name, so the sigil belongs to
    /// whoever renders it — and a name with a `/` inside it, like a scoped plugin command, would not
    /// survive being re-slugged.
    #[test]
    fn the_command_catalog_keeps_the_agents_own_spelling() {
        assert_eq!(
            reduce(vec![json!({
                "sessionUpdate": "available_commands_update",
                "availableCommands": [
                    { "name": "create_plan", "description": "Draft a plan" },
                    { "name": "my-plugin:review", "description": "Review the diff" }
                ]
            })]),
            vec![EventKind::CommandsAvailable {
                commands: vec![
                    Command {
                        name: String::from("create_plan"),
                        description: Some(String::from("Draft a plan")),
                    },
                    Command {
                        name: String::from("my-plugin:review"),
                        description: Some(String::from("Review the diff")),
                    },
                ]
            }]
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

    /// Only a cancellation is somebody stopping the turn. A refusal is the agent ending its own, and
    /// reporting it as an error would tell a host to retry a decision.
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
}
