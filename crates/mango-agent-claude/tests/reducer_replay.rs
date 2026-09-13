//! The reducer, replaying the captured transcripts byte for byte.
//!
//! Every case here was a TypeScript test against the same two `.jsonl` files in the mangostudio
//! runtime, and keeps its name so the two can be compared. A fixture is the only way to test a
//! dialect on a machine where the vendor's CLI is not installed — which is every machine, in CI —
//! and it is the only thing that can notice the vendor changing a shape.

mod support;

use mango_agent_claude::protocol::StreamRecord;
use mango_agent_claude::reducer::{RunInit, TurnReducer};
use mango_external_agents::{
    Activity, ActivityKind, ActivityResult, ActivityStatus, Command, ErrorCode, EventKind,
    VendorError,
};
use support::READ_TURN;

const DENIED_WRITE: &str =
    include_str!("../../../fixtures/claude/transcripts/denied-write-turn.jsonl");

/// One run's events, plus the last thing the run said about itself.
struct Replay {
    events: Vec<EventKind>,
    init: Option<RunInit>,
}

/// Replays a captured transcript through a reducer, exactly as the turn loop feeds it.
fn replay(transcript: &str, resumed: bool) -> Replay {
    let mut reducer = TurnReducer::new(resumed);
    let mut events = Vec::new();
    let mut init = None;
    for line in transcript.lines() {
        let Some(record) = StreamRecord::parse(line) else {
            continue;
        };
        let reduction = reducer.reduce(&record);
        events.extend(reduction.events);
        init = reduction.init.or(init);
    }
    Replay { events, init }
}

/// A synthetic run, for the shapes a captured transcript does not happen to contain.
fn reduce_lines(lines: &[&str]) -> Vec<EventKind> {
    let mut reducer = TurnReducer::new(false);
    lines
        .iter()
        .filter_map(|line| StreamRecord::parse(line))
        .flat_map(|record| reducer.reduce(&record).events)
        .collect()
}

/// Each event named by its kind, for asserting on a whole run's shape at once.
fn shape(events: &[EventKind]) -> Vec<String> {
    events.iter().map(name_of).collect()
}

/// The event's own `type` tag, read back through the same [`serde::Serialize`] impl the wire
/// format uses — rather than a hand-maintained copy of `EventKind`'s `#[serde(rename_all)]` table,
/// which would silently read a variant this match forgot as "unknown" instead of failing.
fn name_of(event: &EventKind) -> String {
    serde_json::to_value(event)
        .ok()
        .and_then(|value| value.get("type")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| String::from("unknown"))
}

fn text_of(events: &[EventKind]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            EventKind::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn reasoning_of(events: &[EventKind]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            EventKind::ReasoningDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn commands_of(events: &[EventKind]) -> Vec<Command> {
    events
        .iter()
        .find_map(|event| match event {
            EventKind::CommandsAvailable { commands } => Some(commands.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// The first call this run opened, and what it opened it with.
fn first_activity_started(events: &[EventKind]) -> (&str, &Activity) {
    events
        .iter()
        .find_map(|event| match event {
            EventKind::ActivityStarted { call_id, activity } => Some((call_id.as_str(), activity)),
            _ => None,
        })
        .expect("expected an ActivityStarted event")
}

/// The first call this run closed, and what it closed it with.
fn first_activity_completed(events: &[EventKind]) -> (&str, &ActivityResult) {
    events
        .iter()
        .find_map(|event| match event {
            EventKind::ActivityCompleted { call_id, result } => Some((call_id.as_str(), result)),
            _ => None,
        })
        .expect("expected an ActivityCompleted event")
}

fn names(commands: &[Command]) -> Vec<&str> {
    commands
        .iter()
        .map(|command| command.name.as_str())
        .collect()
}

mod on_a_recorded_read_a_file_turn {
    use super::*;

    #[test]
    fn opens_the_session_from_the_init_record() {
        let run = replay(READ_TURN, false);
        assert_eq!(
            run.events.first(),
            Some(&EventKind::SessionStarted {
                native_session_id: String::from("b01414e7-4b4b-43a2-9109-a33e21664340"),
                resumed: false,
            })
        );
        assert_eq!(
            run.init.expect("expected the run to describe itself").model,
            Some(String::from("claude-sonnet-5"))
        );
    }

    #[test]
    fn publishes_the_names_whose_origin_the_record_states_when_the_exclusion_list_is_unreadable() {
        let commands = commands_of(&replay(READ_TURN, false).events);
        let published = names(&commands);

        assert!(
            published.contains(&"dataviz"),
            "expected a skill to publish"
        );
        assert!(
            published.contains(&"code-review:code-review"),
            "expected a plugin's own command to publish"
        );
        assert!(
            published.contains(&"mcp__claude_design__design"),
            "expected an MCP server's command to publish"
        );
        assert!(
            published.contains(&"doctor"),
            "expected a skill named like a builtin to publish, because the record says it is a skill"
        );
        for withheld in [
            "clear",
            "compact",
            "agents",
            "heapdump",
            "__remote-workflow",
        ] {
            assert!(
                !published.contains(&withheld),
                "expected {withheld:?} to be withheld, received {published:?}"
            );
        }
        assert!(
            commands.iter().all(|command| command.description.is_none()),
            "expected Claude Code to send no help text with its names"
        );
    }

    #[test]
    fn reports_the_resume_state_it_was_opened_with_rather_than_inferring_one() {
        let run = replay(READ_TURN, true);
        assert_eq!(
            run.events.first(),
            Some(&EventKind::SessionStarted {
                native_session_id: String::from("b01414e7-4b4b-43a2-9109-a33e21664340"),
                resumed: true,
            })
        );
    }

    #[test]
    fn delivers_assistant_text_once_from_the_deltas_only() {
        let events = replay(READ_TURN, false).events;
        assert_eq!(text_of(&events), "mango");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, EventKind::TextDelta { .. }))
                .count(),
            2,
            "expected the two deltas and no replay of the completed block"
        );
    }

    #[test]
    fn emits_no_reasoning_when_the_vendor_withholds_the_thinking_text() {
        let events = replay(READ_TURN, false).events;
        assert_eq!(
            reasoning_of(&events),
            "",
            "expected the withheld thinking deltas to carry nothing"
        );
    }

    #[test]
    fn announces_the_reasoning_phase_even_when_the_vendor_withholds_every_delta() {
        let events = replay(READ_TURN, false).events;
        assert_eq!(
            events
                .iter()
                .filter(|event| **event == EventKind::ReasoningStarted)
                .count(),
            1,
            "expected the phase to be announced by its opening block alone"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| **event == EventKind::ReasoningEnded)
                .count(),
            1
        );
    }

    #[test]
    fn labels_the_activity_with_claudes_own_tool_name_verbatim() {
        let events = replay(READ_TURN, false).events;
        let (call_id, activity) = first_activity_started(&events);
        assert_eq!(call_id, "toolu_01LZJqPzShDSj9cPgL7PeD1v");
        assert_eq!(activity.name, "Read");
        assert_eq!(activity.kind, ActivityKind::Other);
        assert_eq!(activity.title, "/work/repo/note.txt");
    }

    #[test]
    fn closes_the_activity_when_its_tool_result_arrives() {
        let events = replay(READ_TURN, false).events;
        let (call_id, result) = first_activity_completed(&events);
        assert_eq!(call_id, "toolu_01LZJqPzShDSj9cPgL7PeD1v");
        assert_eq!(result.status, ActivityStatus::Completed);
        assert_eq!(result.detail.as_deref(), Some("1\tmango\n2\t"));
    }

    #[test]
    fn ends_with_usage_and_a_completion() {
        let events = replay(READ_TURN, false).events;
        assert_eq!(
            shape(&events),
            vec![
                "session_started",
                "commands_available",
                "reasoning_started",
                "reasoning_ended",
                "activity_started",
                "activity_completed",
                "text_delta",
                "text_delta",
                "usage",
                "completed",
            ]
        );

        let EventKind::Usage { usage } = &events[events.len() - 2] else {
            panic!("expected usage before the completion, received {events:?}");
        };
        assert_eq!(usage.input_tokens, Some(4));
        assert_eq!(usage.output_tokens, Some(775));
        assert_eq!(usage.cache_read_tokens, Some(30_122));
        assert_eq!(usage.cache_write_tokens, Some(31_209));
    }

    #[test]
    fn emits_no_approval_because_claude_never_offers_one_to_answer() {
        for transcript in [READ_TURN, DENIED_WRITE] {
            let events = replay(transcript, false).events;
            assert!(
                !events.iter().any(|event| matches!(
                    event,
                    EventKind::ApprovalRequested { .. } | EventKind::ApprovalResolved { .. }
                )),
                "expected no approval event, received {:?}",
                shape(&events)
            );
        }
    }
}

mod on_a_recorded_denied_write {
    use super::*;

    #[test]
    fn reports_the_denial_as_a_failed_activity_not_as_a_pending_approval() {
        let events = replay(DENIED_WRITE, false).events;
        assert_eq!(
            shape(&events),
            vec![
                "session_started",
                "commands_available",
                "reasoning_delta",
                "activity_started",
                "activity_completed",
                "reasoning_delta",
                "text_delta",
                "usage",
                "completed",
            ]
        );
    }

    #[test]
    fn completes_the_turn_rather_than_failing_it() {
        let events = replay(DENIED_WRITE, false).events;
        assert_eq!(
            events.last(),
            Some(&EventKind::Completed),
            "expected a refused tool to leave the turn successful"
        );
    }

    #[test]
    fn reports_the_denial_once_through_the_activity_that_was_refused() {
        let events = replay(DENIED_WRITE, false).events;
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, EventKind::ActivityCompleted { .. }))
                .count(),
            1,
            "expected the refusal to be rendered exactly once"
        );
    }

    #[test]
    fn names_the_refused_tool_and_the_reason_in_the_activity_claude_reports() {
        let events = replay(DENIED_WRITE, false).events;
        let (_, activity) = first_activity_started(&events);
        assert_eq!(activity.name, "Write");
        assert_eq!(activity.kind, ActivityKind::FileChange);
        assert_eq!(activity.title, "/work/repo/denied.txt");

        let (_, result) = first_activity_completed(&events);
        assert_eq!(result.status, ActivityStatus::Failed);
        assert_eq!(
            result.detail.as_deref(),
            Some(
                "Claude requested permissions to write to /work/repo/denied.txt, but you haven't granted it yet."
            )
        );
    }

    #[test]
    fn delivers_the_assistant_text_even_though_the_run_carried_no_partial_messages() {
        let events = replay(DENIED_WRITE, false).events;
        assert_eq!(
            text_of(&events),
            "I need permission to write the file. Please approve the request to write to `denied.txt` so I can create the file with the content \"hello\"."
        );
    }

    #[test]
    fn delivers_the_assistant_reasoning_even_though_the_run_carried_no_partial_messages() {
        let events = replay(DENIED_WRITE, false).events;
        let reasoning = reasoning_of(&events);
        assert!(
            reasoning.starts_with("The user is asking me to create a file named denied.txt"),
            "expected the whole completed thinking block, received {reasoning:?}"
        );
        assert!(reasoning.contains("I need to use the Write tool with:"));
    }
}

mod merging_a_permission_denial_into_its_activity {
    use super::*;

    const OPEN_WRITE: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Write","input":{"file_path":"/work/x.txt"}}]}}"#;

    #[test]
    fn prefers_the_vendors_own_denial_reason_over_a_tool_result_that_does_not_explain_itself() {
        let events = reduce_lines(&[
            OPEN_WRITE,
            r#"{"type":"system","subtype":"permission_denied","tool_use_id":"toolu_1","message":"Claude asked to write /work/x.txt and nobody granted it."}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","is_error":true,"content":"denied"}]}}"#,
        ]);
        let EventKind::ActivityCompleted { result, .. } = &events[1] else {
            panic!("expected the call to close, received {events:?}");
        };
        assert_eq!(result.status, ActivityStatus::Failed);
        assert_eq!(
            result.detail.as_deref(),
            Some("Claude asked to write /work/x.txt and nobody granted it.")
        );
    }

    #[test]
    fn carries_the_held_denial_into_a_call_the_run_ended_without_closing() {
        let events = reduce_lines(&[
            OPEN_WRITE,
            r#"{"type":"system","subtype":"permission_denied","tool_use_id":"toolu_1","message":"Nobody granted it."}"#,
            r#"{"type":"result","is_error":false}"#,
        ]);
        let EventKind::ActivityCompleted { result, .. } = &events[1] else {
            panic!("expected the open call to be closed by the result, received {events:?}");
        };
        assert_eq!(result.status, ActivityStatus::Cancelled);
        assert_eq!(result.detail.as_deref(), Some("Nobody granted it."));
    }

    #[test]
    fn falls_back_to_the_tool_result_content_when_no_denial_was_held_for_the_call() {
        let events = reduce_lines(&[
            OPEN_WRITE,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"wrote 5 bytes"}]}}"#,
        ]);
        let EventKind::ActivityCompleted { result, .. } = &events[1] else {
            panic!("expected the call to close, received {events:?}");
        };
        assert_eq!(result.status, ActivityStatus::Completed);
        assert_eq!(result.detail.as_deref(), Some("wrote 5 bytes"));
    }

    #[test]
    fn ignores_a_tool_result_for_a_call_this_run_never_opened() {
        let events = reduce_lines(&[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_ghost","content":"x"}]}}"#,
        ]);
        assert!(events.is_empty(), "received {events:?}");
    }
}

mod reconciling_a_completed_block_with_its_deltas {
    use super::*;

    fn open_text(index: u64) -> String {
        format!(
            r#"{{"type":"stream_event","event":{{"type":"content_block_start","index":{index},"content_block":{{"type":"text"}}}}}}"#
        )
    }

    fn text_delta(index: u64, text: &str) -> String {
        format!(
            r#"{{"type":"stream_event","event":{{"type":"content_block_delta","index":{index},"delta":{{"type":"text_delta","text":"{text}"}}}}}}"#
        )
    }

    fn completed_text(text: &str) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}}}}"#
        )
    }

    fn run(lines: &[String]) -> Vec<EventKind> {
        let borrowed: Vec<&str> = lines.iter().map(String::as_str).collect();
        reduce_lines(&borrowed)
    }

    #[test]
    fn emits_nothing_for_a_block_its_deltas_already_delivered_in_full() {
        let events = run(&[
            open_text(0),
            text_delta(0, "mango"),
            completed_text("mango"),
        ]);
        assert_eq!(text_of(&events), "mango", "expected exactly one delivery");
        assert_eq!(events.len(), 1, "received {events:?}");
    }

    #[test]
    fn emits_only_the_tail_the_deltas_stopped_short_of() {
        let events = run(&[
            open_text(0),
            text_delta(0, "mango"),
            completed_text("mango juice"),
        ]);
        assert_eq!(text_of(&events), "mango juice");
        assert_eq!(events.len(), 2, "received {events:?}");
        assert_eq!(
            events[1],
            EventKind::TextDelta {
                text: String::from(" juice")
            }
        );
    }

    #[test]
    fn emits_the_whole_block_when_nothing_streamed_for_it() {
        let events = run(&[completed_text("mango")]);
        assert_eq!(
            events,
            vec![EventKind::TextDelta {
                text: String::from("mango")
            }]
        );
    }

    #[test]
    fn emits_nothing_for_a_reasoning_phase_the_vendor_withheld() {
        let events = reduce_lines(&[
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":""}]}}"#,
        ]);
        assert_eq!(events, vec![EventKind::ReasoningStarted]);
    }

    /// The stale buffer this guards against is one the *next* message never overwrites.
    ///
    /// A completed block consumes the buffer it matched, and a `content_block_start` replaces the
    /// entry at its own index — so a second message that reuses index 0 would look fine either
    /// way. The buffer that survives is the one at an index the next message does not reach:
    /// here the first message streams two blocks and only completes the first, so index 1 is left
    /// holding `beta` when the second message opens index 0 alone.
    #[test]
    fn does_not_credit_a_new_message_with_the_previous_ones_delivery() {
        let events = run(&[
            open_text(0),
            text_delta(0, "alpha"),
            open_text(1),
            text_delta(1, "beta"),
            completed_text("alpha"),
            String::from(r#"{"type":"stream_event","event":{"type":"message_stop"}}"#),
            String::from(r#"{"type":"stream_event","event":{"type":"message_start"}}"#),
            open_text(0),
            text_delta(0, "x"),
            completed_text("beta gamma"),
        ]);
        assert_eq!(
            text_of(&events),
            "alphabetaxbeta gamma",
            "expected the second message's block to be delivered whole, not credited to the first message's leftover buffer"
        );
    }

    #[test]
    fn does_not_let_a_delivered_reasoning_buffer_stand_in_for_an_echoing_text_block() {
        // Claude restates the plan it just reasoned through, so the two blocks share a prefix.
        let events = reduce_lines(&[
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"I will read the file"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_stop","index":0}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I will read the file, then summarise it."}]}}"#,
        ]);
        assert_eq!(
            text_of(&events),
            "I will read the file, then summarise it.",
            "expected the whole text block, not the tail past the thinking buffer"
        );
    }
}

mod subagent_handling {
    use super::*;

    const OPEN_TASK: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_task","name":"Task","input":{"description":"Find the bug"}}]}}"#;

    fn nested_text(text: &str) -> String {
        format!(
            r#"{{"type":"assistant","parent_tool_use_id":"toolu_task","message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}]}}}}"#
        )
    }

    #[test]
    fn gives_task_the_subagent_icon_while_keeping_its_vendor_name() {
        let events = reduce_lines(&[OPEN_TASK]);
        let EventKind::ActivityStarted { activity, .. } = &events[0] else {
            panic!("expected the Task call, received {events:?}");
        };
        assert_eq!(activity.name, "Task");
        assert_eq!(activity.kind, ActivityKind::Subagent);
        assert_eq!(activity.title, "Find the bug");
    }

    #[test]
    fn nests_a_subagents_text_under_the_task_activity_that_spawned_it() {
        let first = nested_text("found one");
        let second = nested_text("and another");
        let events = reduce_lines(&[OPEN_TASK, &first, &second]);

        let EventKind::ActivityUpdated { call_id, update } = &events[2] else {
            panic!("expected two nested updates, received {events:?}");
        };
        assert_eq!(call_id, "toolu_task");
        assert_eq!(
            update.detail.as_deref(),
            Some("found one\nand another"),
            "expected the blocks to accumulate rather than replace one another"
        );
    }

    #[test]
    fn never_promotes_a_subagents_text_into_the_main_transcript() {
        let nested = nested_text("a second agent talking");
        let events = reduce_lines(&[OPEN_TASK, &nested]);
        assert_eq!(text_of(&events), "", "received {events:?}");
    }

    #[test]
    fn never_announces_a_subagents_reasoning_phase_into_the_main_transcript() {
        let events = reduce_lines(&[
            OPEN_TASK,
            r#"{"type":"stream_event","parent_tool_use_id":"toolu_task","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}}"#,
        ]);
        assert_eq!(shape(&events), vec!["activity_started"]);
    }

    /// A subagent's own tool calls are the subagent's, exactly as its text is.
    ///
    /// Promoting one puts a second agent's `Read` beside the `Task` that spawned it, as though the
    /// assistant had run it — and the `tool_result` that would close it arrives under the same
    /// parent, so the activity spins for the rest of the turn and then closes as cancelled.
    #[test]
    fn never_promotes_a_subagents_own_tool_call_into_the_main_transcript() {
        let nested_call = r#"{"type":"assistant","parent_tool_use_id":"toolu_task","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_nested","name":"Read","input":{"file_path":"/work/note.txt"}}]}}"#;
        let nested_result = r#"{"type":"user","parent_tool_use_id":"toolu_task","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_nested","content":"1\tmango"}]}}"#;
        let events = reduce_lines(&[
            OPEN_TASK,
            nested_call,
            nested_result,
            r#"{"type":"result","is_error":false}"#,
        ]);
        assert_eq!(
            shape(&events),
            vec!["activity_started", "activity_completed", "completed"],
            "expected only the Task's own pair, received {events:?}"
        );
        let started = events
            .iter()
            .filter_map(|event| match event {
                EventKind::ActivityStarted { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(started, vec!["toolu_task"]);
    }

    #[test]
    fn ignores_nested_text_for_a_call_that_has_already_closed() {
        let nested = nested_text("late");
        let events = reduce_lines(&[
            OPEN_TASK,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_task","content":"done"}]}}"#,
            &nested,
        ]);
        assert_eq!(
            shape(&events),
            vec!["activity_started", "activity_completed"]
        );
    }
}

mod the_reasoning_phase {
    use super::*;

    fn open_block(index: u64, block_type: &str) -> String {
        format!(
            r#"{{"type":"stream_event","event":{{"type":"content_block_start","index":{index},"content_block":{{"type":"{block_type}"}}}}}}"#
        )
    }

    fn stop_block(index: u64) -> String {
        format!(
            r#"{{"type":"stream_event","event":{{"type":"content_block_stop","index":{index}}}}}"#
        )
    }

    #[test]
    fn announces_a_reasoning_phase_when_the_vendor_opens_a_thinking_block() {
        assert_eq!(
            reduce_lines(&[&open_block(0, "thinking")]),
            vec![EventKind::ReasoningStarted]
        );
    }

    #[test]
    fn announces_a_reasoning_phase_for_a_redacted_thinking_block_too() {
        assert_eq!(
            reduce_lines(&[&open_block(0, "redacted_thinking")]),
            vec![EventKind::ReasoningStarted]
        );
    }

    #[test]
    fn does_not_announce_a_reasoning_phase_for_a_tool_use_block_opening() {
        assert!(reduce_lines(&[&open_block(0, "tool_use")]).is_empty());
        assert!(reduce_lines(&[&open_block(0, "text")]).is_empty());
    }

    #[test]
    fn ends_the_reasoning_phase_when_the_block_that_opened_it_closes() {
        assert_eq!(
            reduce_lines(&[&open_block(2, "thinking"), &stop_block(2)]),
            vec![EventKind::ReasoningStarted, EventKind::ReasoningEnded]
        );
    }

    #[test]
    fn does_not_end_a_reasoning_phase_twice() {
        assert_eq!(
            reduce_lines(&[&open_block(0, "thinking"), &stop_block(0), &stop_block(0)]),
            vec![EventKind::ReasoningStarted, EventKind::ReasoningEnded]
        );
    }

    #[test]
    fn ends_a_reasoning_phase_the_message_boundary_closed_for_it() {
        assert_eq!(
            reduce_lines(&[
                &open_block(0, "thinking"),
                r#"{"type":"stream_event","event":{"type":"message_stop"}}"#,
            ]),
            vec![EventKind::ReasoningStarted, EventKind::ReasoningEnded]
        );
    }

    #[test]
    fn streams_thinking_as_reasoning_not_as_text_when_the_vendor_sends_it() {
        let events = reduce_lines(&[
            &open_block(0, "thinking"),
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"weighing it up"}}}"#,
        ]);
        assert_eq!(reasoning_of(&events), "weighing it up");
        assert_eq!(text_of(&events), "");
    }
}

mod termination {
    use super::*;

    #[test]
    fn cancels_activities_still_open_when_the_result_arrives() {
        let events = reduce_lines(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"sleep 600"}}]}}"#,
            r#"{"type":"result","is_error":false}"#,
        ]);
        assert_eq!(
            shape(&events),
            vec!["activity_started", "activity_completed", "completed"]
        );
        let EventKind::ActivityCompleted { result, .. } = &events[1] else {
            unreachable!("asserted above")
        };
        assert_eq!(result.status, ActivityStatus::Cancelled);
        assert_eq!(result.detail, None);
    }

    #[test]
    fn closes_a_run_whose_process_died_without_a_result() {
        let mut reducer = TurnReducer::new(false);
        let opening = StreamRecord::parse(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"sleep 600"}}]}}"#,
        )
        .expect("expected a parseable record");
        let mut events = reducer.reduce(&opening).events;
        events.extend(reducer.abort(VendorError::new(
            ErrorCode::from_static("claude-no-result"),
            "Claude Code ended without a result (exit code 1).",
        )));

        assert_eq!(
            shape(&events),
            vec!["activity_started", "activity_completed", "error"]
        );
        assert!(reducer.finished(), "expected the abort to end the run");
    }
}
