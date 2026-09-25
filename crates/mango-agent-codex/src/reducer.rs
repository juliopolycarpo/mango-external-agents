//! What the app-server announced, as what a host renders.
//!
//! One rule shapes the whole module: **`turn/completed` is the only terminal.** The server also
//! sends an `error` notification, which reads like an ending and is not one — it carries
//! `willRetry`, and the turn's own completion still follows. A reducer that ended the turn there
//! would end it twice, which is the one thing the core's contract says cannot happen.
//!
//! The second rule is that a connection carries more than one conversation. A subagent's thread
//! and a detached review's arrive on the same pipe, and anything not addressed to this session's
//! thread belongs to somebody else.

use mango_external_agents::error::{ErrorCode, VendorError};
use mango_external_agents::event::{EventKind, ThreadUsage, Usage};
use mango_external_agents::session::CancelReason;

use crate::activity;
use crate::protocol::items::ThreadItem;
use crate::protocol::notifications::{Notification, ThreadTokenUsage, TokenUsageBreakdown};
use crate::protocol::requests::TurnStatus;

/// What one announcement does to a turn.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// Emit these events, in order, and keep reading.
    Emit(Vec<EventKind>),
    /// The turn is over: emit these, then end it.
    ///
    /// The events come first and the ending after, so a completion's own usage reaches the host
    /// before the terminal that closes the stream.
    Finish {
        /// Whatever the ending itself reported.
        events: Vec<EventKind>,
        /// Why it ended, when it did not simply finish.
        cancelled: Option<CancelReason>,
        /// The failure, when it failed.
        failure: Option<VendorError>,
    },
    /// The terminal frame was malformed and could not name a conversation. The session must stop
    /// accepting work after failing its active stream because it cannot safely correlate later
    /// frames on this connection.
    Poison {
        /// The protocol failure to put on the active stream.
        failure: VendorError,
    },
    /// Nothing a host needs to know about.
    Ignore,
}

impl Outcome {
    /// One event and nothing else.
    fn one(kind: EventKind) -> Self {
        Self::Emit(vec![kind])
    }
}

/// Turns one announcement into what a host sees.
///
/// `thread_id` is the conversation this session subscribed to. An announcement addressed to
/// another one is [`Outcome::Ignore`]: a subagent's thread and a detached review's ride the same
/// connection, and replaying their events under this turn's id would attribute another
/// conversation's work to this one.
///
/// `now` is the host's clock, for the one event that carries a reading's own timestamp: a quota
/// snapshot renders as unknown rather than as zero once it is stale, which needs an instant the
/// host agrees with rather than one this module invented.
#[must_use]
pub fn reduce(notification: &Notification, thread_id: &str, now: std::time::SystemTime) -> Outcome {
    if notification
        .thread_id()
        .is_some_and(|addressed| addressed != thread_id)
    {
        return Outcome::Ignore;
    }

    if notification.is_malformed_terminal() {
        let failure = malformed_terminal_failure();
        return if notification.thread_id().is_some() {
            Outcome::Finish {
                events: Vec::new(),
                cancelled: None,
                failure: Some(failure),
            }
        } else {
            Outcome::Poison { failure }
        };
    }

    match notification {
        Notification::AgentMessageDelta(delta) if !delta.delta.is_empty() => {
            Outcome::one(EventKind::TextDelta {
                text: delta.delta.clone(),
            })
        }
        Notification::ReasoningDelta(delta) if !delta.delta.is_empty() => {
            Outcome::one(EventKind::ReasoningDelta {
                text: delta.delta.clone(),
            })
        }
        Notification::ItemStarted(started) => item_started(&started.item),
        Notification::ItemCompleted(completed) => item_completed(&completed.item),
        Notification::ThreadTokenUsage(usage) => usage_events(&usage.token_usage),
        // Quota belongs to the account rather than to a conversation, which is why it names no
        // thread and is not routed by one.
        Notification::RateLimits(update) => Outcome::one(EventKind::AccountLimits {
            limits: crate::rate_limits::to_account_limits(&update.rate_limits, now),
        }),
        Notification::TurnCompleted(completed) => finish(completed.turn.status, &completed.turn),
        // Streamed progress needs to know which activities this turn announced, which is
        // `TurnReducer`'s memory rather than this function's.
        Notification::CommandOutputDelta(_)
        | Notification::McpToolCallProgress(_)
        | Notification::FileChangePatchUpdated(_) => Outcome::Ignore,
        // A turn beginning is the same turn the host already started; announcing it again would
        // be a second session start. An error notification is a report, never an ending.
        Notification::TurnStarted(_)
        | Notification::ThreadStarted(_)
        | Notification::ServerRequestResolved(_)
        | Notification::Error(_)
        | Notification::AgentMessageDelta(_)
        | Notification::ReasoningDelta(_)
        | Notification::Malformed { .. }
        | Notification::Other { .. } => Outcome::Ignore,
    }
}

/// Whether an announcement belongs to the turn this session is running right now.
///
/// The connection carries more than one conversation — a subagent's thread, a detached review's,
/// another turn of this thread's — and account-level quota belongs to no conversation at all. This
/// is the routing half of [`reduce_for_active_turn`] on its own, for the caller that has to decide
/// whether a frame is progress before knowing whether it renders: an announcement can be this
/// turn's and still reduce to [`Outcome::Ignore`], as `turn/started` and a retrying `error` both
/// do.
///
/// For example, `routes_to_active_turn(&notification, thread_id, Some(native_turn_id))` is false
/// for a `rateLimits` update, which names no thread.
#[must_use]
pub fn routes_to_active_turn(
    notification: &Notification,
    thread_id: &str,
    active_native_turn_id: Option<&str>,
) -> bool {
    let Some(active_native_turn_id) = active_native_turn_id else {
        return false;
    };
    notification.thread_id() == Some(thread_id)
        && matches_active_turn(notification, active_native_turn_id)
}

/// Whether an announcement that names a turn names *this* one.
///
/// An empty active id means the start response has not answered yet, so nothing can be compared;
/// a family excluded from [`Notification::requires_native_turn_match`] is never compared at all.
fn matches_active_turn(notification: &Notification, active_native_turn_id: &str) -> bool {
    active_native_turn_id.is_empty()
        || !notification.requires_native_turn_match()
        || notification
            .turn_id()
            .is_none_or(|turn_id| turn_id == active_native_turn_id)
}

/// Reduces an announcement after checking it against the current turn's native id.
///
/// The start response is the authoritative id. `turn/started` deliberately bypasses this check,
/// because native reviews announce a different id in that early notification than they use for
/// their items and completion.
#[must_use]
pub fn reduce_for_active_turn(
    notification: &Notification,
    thread_id: &str,
    active_native_turn_id: Option<&str>,
    now: std::time::SystemTime,
) -> Outcome {
    let Some(active_native_turn_id) = active_native_turn_id else {
        return Outcome::Ignore;
    };
    if !matches_active_turn(notification, active_native_turn_id) {
        return Outcome::Ignore;
    }
    reduce(notification, thread_id, now)
}

fn item_started(item: &ThreadItem) -> Outcome {
    // Reasoning opens a phase rather than an activity: on a default build the vendor withholds the
    // text, so without the marker a whole phase produces no events at all and the turn looks hung.
    if matches!(item, ThreadItem::Reasoning { .. }) {
        return Outcome::one(EventKind::ReasoningStarted);
    }
    let (Some(call_id), Some(activity)) = (item.id(), activity::started(item)) else {
        return Outcome::Ignore;
    };
    Outcome::one(EventKind::ActivityStarted {
        call_id: call_id.to_owned(),
        activity,
    })
}

fn item_completed(item: &ThreadItem) -> Outcome {
    if matches!(item, ThreadItem::Reasoning { .. }) {
        return Outcome::one(EventKind::ReasoningEnded);
    }
    let (Some(call_id), Some(result)) = (item.id(), activity::completed(item)) else {
        return Outcome::Ignore;
    };
    Outcome::one(EventKind::ActivityCompleted {
        call_id: call_id.to_owned(),
        result,
    })
}

fn usage_events(usage: &ThreadTokenUsage) -> Outcome {
    let mut events = Vec::with_capacity(2);
    // Per-turn first: a display reading the thread total for this turn would grow monotonically
    // and mislead, so the two are emitted as the distinct things they are.
    if let Some(last) = usage.last {
        events.push(EventKind::Usage {
            usage: to_usage(&last),
        });
    }
    if usage.total.is_some() || usage.model_context_window.is_some() {
        events.push(EventKind::ThreadUsage {
            usage: ThreadUsage {
                last: usage.last.as_ref().map(to_usage),
                total: usage.total.as_ref().map(to_usage),
                context_window_tokens: usage.model_context_window,
            },
        });
    }
    if events.is_empty() {
        return Outcome::Ignore;
    }
    Outcome::Emit(events)
}

fn to_usage(breakdown: &TokenUsageBreakdown) -> Usage {
    Usage {
        input_tokens: breakdown.input_tokens,
        output_tokens: breakdown.output_tokens,
        cache_read_tokens: breakdown.cached_input_tokens,
        cache_write_tokens: breakdown.cache_write_input_tokens,
        reasoning_tokens: breakdown.reasoning_output_tokens,
        total_tokens: breakdown.total_tokens,
    }
}

/// The code every Codex turn failure is reported under.
pub const TURN_FAILED: ErrorCode = ErrorCode::from_static("codex-turn-failed");

/// The app-server named a terminal notification but did not provide the shape this harness needs.
pub const PROTOCOL_ERROR: ErrorCode = ErrorCode::from_static("codex-protocol-error");

fn malformed_terminal_failure() -> VendorError {
    VendorError::new(
        PROTOCOL_ERROR,
        "expected turn/completed params with a string threadId and turn.id, received malformed params",
    )
    .with_vendor_code("malformed-turn-completed", false)
}

fn finish(status: Option<TurnStatus>, turn: &crate::protocol::requests::TurnHandle) -> Outcome {
    match status {
        // Still running, or spelled in a way this build does not know, but the server says the
        // turn is over. Nothing sensible follows, so the turn ends rather than waiting for a
        // completion that has already been sent. An unknown ending claims neither a cancellation
        // nor a failure, because neither is evidenced.
        Some(TurnStatus::Completed)
        | Some(TurnStatus::InProgress)
        | Some(TurnStatus::Unknown)
        | None => Outcome::Finish {
            events: Vec::new(),
            cancelled: None,
            failure: None,
        },
        Some(TurnStatus::Interrupted) => Outcome::Finish {
            events: Vec::new(),
            // The vendor says only that the turn was interrupted. Who asked for it is the
            // session's knowledge, not the server's, so the session substitutes the reason it
            // cancelled under and this is the fallback when nobody here did.
            cancelled: Some(CancelReason::Requested),
            failure: None,
        },
        Some(TurnStatus::Failed) => {
            let error = turn.error.clone().unwrap_or_default();
            let message = match error.additional_details {
                Some(details) if !details.is_empty() => format!("{}: {details}", error.message),
                _ => error.message,
            };
            Outcome::Finish {
                events: Vec::new(),
                cancelled: None,
                failure: Some(VendorError::new(
                    TURN_FAILED,
                    if message.is_empty() {
                        String::from("the turn failed without saying why")
                    } else {
                        message
                    },
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Outcome, reduce, routes_to_active_turn};
    use crate::protocol::notifications::{Notification, method};
    use mango_external_agents::content::ActivityContent;
    use mango_external_agents::event::{ActivityKind, ActivityStatus, EventKind};
    use mango_external_agents::session::CancelReason;
    use serde_json::json;

    const THREAD: &str = "01a09999-7858";

    /// A clock that does not move, so a quota snapshot's own timestamp is a value to assert on.
    fn now() -> std::time::SystemTime {
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_789_283_381)
    }

    fn notification(family: &str, params: serde_json::Value) -> Notification {
        Notification::parse(family, params)
    }

    /// Idle accounting asks a narrower question than rendering does: not "what does this say",
    /// but "is this turn still being worked on". Everything else on the connection has to answer
    /// no, or a silent turn never reaches its deadline.
    #[test]
    fn another_conversations_announcement_is_not_this_turns_progress() {
        assert!(
            !routes_to_active_turn(
                &notification(
                    method::ITEM_STARTED,
                    json!({"threadId": "someone-elses", "turnId": "u",
                           "item": {"id": "i", "item_type": "agent_message"}}),
                ),
                THREAD,
                Some("t"),
            ),
            "expected another thread's item to leave this turn's deadline alone"
        );
    }

    #[test]
    fn another_turn_of_this_conversation_is_not_this_turns_progress() {
        assert!(
            !routes_to_active_turn(
                &notification(
                    method::AGENT_MESSAGE_DELTA,
                    json!({"threadId": THREAD, "turnId": "an-older-turn", "itemId": "i",
                           "delta": "late"}),
                ),
                THREAD,
                Some("t"),
            ),
            "expected a delayed frame for another turn to leave this turn's deadline alone"
        );
    }

    /// Quota belongs to the account, so it names no conversation and can arrive while every
    /// conversation on the connection is silent.
    #[test]
    fn an_account_quota_update_is_not_this_turns_progress() {
        assert!(
            !routes_to_active_turn(
                &notification(
                    method::ACCOUNT_RATE_LIMITS_UPDATED,
                    json!({"rate_limits": {}}),
                ),
                THREAD,
                Some("t"),
            ),
            "expected an account-level update to leave this turn's deadline alone"
        );
    }

    /// The other half: a frame this turn owns counts even when it renders nothing. `turn/started`
    /// and a retrying `error` both reduce to `Ignore`, and withholding the reset for them would
    /// time out a turn that is working.
    #[test]
    fn a_frame_this_turn_owns_is_progress_even_when_it_renders_nothing() {
        assert!(
            routes_to_active_turn(
                &notification(
                    method::ERROR,
                    json!({"threadId": THREAD, "turnId": "t", "message": "retrying",
                           "willRetry": true}),
                ),
                THREAD,
                Some("t"),
            ),
            "expected this turn's own retry report to count as progress"
        );
        assert!(
            routes_to_active_turn(
                &notification(
                    method::TURN_STARTED,
                    json!({"threadId": THREAD, "turn": {"id": "a-review-announces-another-id"}}),
                ),
                THREAD,
                Some("t"),
            ),
            "expected turn/started, which is never matched on id, to count as progress"
        );
    }

    /// Before the start response answers there is no id to compare, and a turn with no active
    /// route has no deadline to reset.
    #[test]
    fn progress_needs_an_active_route_and_survives_an_unanswered_start() {
        let started = notification(
            method::ITEM_STARTED,
            json!({"threadId": THREAD, "turnId": "whatever-the-server-chose",
                   "item": {"id": "i", "item_type": "agent_message"}}),
        );
        assert!(
            routes_to_active_turn(&started, THREAD, Some("")),
            "expected an unanswered start to accept its own thread's frames"
        );
        assert!(
            !routes_to_active_turn(&started, THREAD, None),
            "expected no active route to have no deadline to reset"
        );
    }

    /// A subagent's thread and a detached review's ride the same connection. Replaying their
    /// events under this turn would attribute another conversation's work to this one.
    #[test]
    fn an_announcement_for_another_conversation_is_not_this_turns_to_render() {
        let outcome = reduce(
            &notification(
                method::AGENT_MESSAGE_DELTA,
                json!({"threadId": "someone-elses", "turnId": "u", "itemId": "i",
                       "delta": "not ours"}),
            ),
            THREAD,
            now(),
        );
        assert_eq!(outcome, Outcome::Ignore);
    }

    #[test]
    fn the_answer_and_the_reasoning_arrive_as_their_own_deltas() {
        let text = reduce(
            &notification(
                method::AGENT_MESSAGE_DELTA,
                json!({"threadId": THREAD, "turnId": "u", "itemId": "i", "delta": "mango"}),
            ),
            THREAD,
            now(),
        );
        assert_eq!(
            text,
            Outcome::Emit(vec![EventKind::TextDelta {
                text: String::from("mango")
            }])
        );

        let reasoning = reduce(
            &notification(
                method::REASONING_SUMMARY_TEXT_DELTA,
                json!({"threadId": THREAD, "turnId": "u", "itemId": "i", "delta": "hmm"}),
            ),
            THREAD,
            now(),
        );
        assert_eq!(
            reasoning,
            Outcome::Emit(vec![EventKind::ReasoningDelta {
                text: String::from("hmm")
            }])
        );
    }

    /// An empty delta is a frame with nothing in it. Emitting it spends a channel slot to say
    /// nothing, and a host rendering deltas would show a flicker per empty frame.
    #[test]
    fn a_delta_with_no_text_in_it_is_not_an_event() {
        let outcome = reduce(
            &notification(
                method::AGENT_MESSAGE_DELTA,
                json!({"threadId": THREAD, "turnId": "u", "itemId": "i", "delta": ""}),
            ),
            THREAD,
            now(),
        );
        assert_eq!(outcome, Outcome::Ignore);
    }

    /// On a default build the vendor withholds the reasoning text entirely, so without the pair of
    /// markers a whole reasoning phase produces no events and the turn looks hung.
    #[test]
    fn a_reasoning_block_opens_and_closes_even_when_the_vendor_sends_no_text() {
        let opened = reduce(
            &notification(
                method::ITEM_STARTED,
                json!({"threadId": THREAD, "turnId": "u",
                       "item": {"type": "reasoning", "id": "r-1", "summary": [], "content": []}}),
            ),
            THREAD,
            now(),
        );
        assert_eq!(opened, Outcome::Emit(vec![EventKind::ReasoningStarted]));

        let closed = reduce(
            &notification(
                method::ITEM_COMPLETED,
                json!({"threadId": THREAD, "turnId": "u",
                       "item": {"type": "reasoning", "id": "r-1", "summary": [], "content": []}}),
            ),
            THREAD,
            now(),
        );
        assert_eq!(closed, Outcome::Emit(vec![EventKind::ReasoningEnded]));
    }

    #[test]
    fn a_command_starts_and_completes_under_one_call_id() {
        let started = reduce(
            &notification(
                method::ITEM_STARTED,
                json!({"threadId": THREAD, "turnId": "u", "item": {
                    "type": "commandExecution", "id": "exec-1", "command": "echo mango",
                    "cwd": "/workspace", "status": "inProgress"
                }}),
            ),
            THREAD,
            now(),
        );
        let Outcome::Emit(events) = started else {
            panic!("expected an activity, received {started:?}");
        };
        let EventKind::ActivityStarted { call_id, activity } = &events[0] else {
            panic!("expected an activity start, received {events:?}");
        };
        assert_eq!(call_id, "exec-1");
        assert_eq!(activity.kind, ActivityKind::Command);

        let completed = reduce(
            &notification(
                method::ITEM_COMPLETED,
                json!({"threadId": THREAD, "turnId": "u", "item": {
                    "type": "commandExecution", "id": "exec-1", "command": "echo mango",
                    "status": "completed", "aggregatedOutput": "mango\n", "exitCode": 0
                }}),
            ),
            THREAD,
            now(),
        );
        let Outcome::Emit(events) = completed else {
            panic!("expected a completion, received {completed:?}");
        };
        let EventKind::ActivityCompleted { call_id, result } = &events[0] else {
            panic!("expected an activity completion, received {events:?}");
        };
        assert_eq!(call_id, "exec-1");
        assert_eq!(result.status, ActivityStatus::Completed);
    }

    /// `call_id` on the envelope and `item_id` on the `Activity` are the same value, but only the
    /// one on the `Activity` survives if a host keeps activities without their envelope.
    #[test]
    fn a_started_activitys_item_id_matches_its_call_id() {
        let started = reduce(
            &notification(
                method::ITEM_STARTED,
                json!({"threadId": THREAD, "turnId": "u", "item": {
                    "type": "commandExecution", "id": "exec-9", "command": "echo mango",
                    "status": "inProgress"
                }}),
            ),
            THREAD,
            now(),
        );
        let Outcome::Emit(events) = started else {
            panic!("expected an activity, received {started:?}");
        };
        let EventKind::ActivityStarted { call_id, activity } = &events[0] else {
            panic!("expected an activity start, received {events:?}");
        };
        assert_eq!(activity.item_id.as_deref(), Some(call_id.as_str()));
    }

    /// A patch's diff reaches the reducer as structure, not only as the joined path list the
    /// command-completion test above already covers.
    #[test]
    fn a_file_changes_diff_reaches_the_reducer_as_structured_content() {
        let diff = "@@ -1 +1 @@\n-old\n+new\n";
        let started = reduce(
            &notification(
                method::ITEM_STARTED,
                json!({"threadId": THREAD, "turnId": "u", "item": {
                    "type": "fileChange", "id": "patch-1", "status": "inProgress",
                    "changes": [{"path": "src/lib.rs", "diff": diff}]
                }}),
            ),
            THREAD,
            now(),
        );
        let Outcome::Emit(events) = started else {
            panic!("expected an activity, received {started:?}");
        };
        let EventKind::ActivityStarted { activity, .. } = &events[0] else {
            panic!("expected an activity start, received {events:?}");
        };
        let Some(ActivityContent::Diff { files }) = &activity.content else {
            panic!("expected diff content, received {:?}", activity.content);
        };
        assert_eq!(files[0].unified_diff.as_deref(), Some(diff));
    }

    /// The turn's own text arrives twice — as deltas, then as a finished item. Rendering the item
    /// as an activity would put every sentence on screen a second time.
    #[test]
    fn a_finished_answer_item_is_not_replayed_as_an_activity() {
        for family in [method::ITEM_STARTED, method::ITEM_COMPLETED] {
            let outcome = reduce(
                &notification(
                    family,
                    json!({"threadId": THREAD, "turnId": "u",
                           "item": {"type": "agentMessage", "id": "m-1", "text": "mango"}}),
                ),
                THREAD,
                now(),
            );
            assert_eq!(outcome, Outcome::Ignore, "expected {family} to be ignored");
        }
    }

    /// Per-turn and whole-thread counts must stay apart: a per-turn display reading the total
    /// would grow monotonically across a conversation and mislead.
    #[test]
    fn usage_reports_this_turn_and_the_whole_thread_as_two_different_things() {
        let outcome = reduce(
            &notification(
                method::THREAD_TOKEN_USAGE_UPDATED,
                json!({"threadId": THREAD, "turnId": "u", "tokenUsage": {
                    "total": {"totalTokens": 38167, "inputTokens": 38006,
                              "cachedInputTokens": 29824, "outputTokens": 161},
                    "last": {"totalTokens": 20019, "inputTokens": 19943,
                             "cachedInputTokens": 17920, "outputTokens": 76},
                    "modelContextWindow": 272000
                }}),
            ),
            THREAD,
            now(),
        );

        let Outcome::Emit(events) = outcome else {
            panic!("expected usage events, received {outcome:?}");
        };
        assert_eq!(events.len(), 2);
        let EventKind::Usage { usage } = &events[0] else {
            panic!("expected this turn's usage first, received {events:?}");
        };
        assert_eq!(usage.total_tokens, Some(20019));
        assert_eq!(usage.cache_read_tokens, Some(17920));

        let EventKind::ThreadUsage { usage } = &events[1] else {
            panic!("expected the thread's usage second, received {events:?}");
        };
        assert_eq!(
            usage.total.and_then(|total| total.total_tokens),
            Some(38167)
        );
        assert_eq!(usage.context_window_tokens, Some(272_000));
    }

    /// The captured ending from a real turn.
    #[test]
    fn a_completed_turn_ends_without_a_cancellation_or_a_failure() {
        let outcome = reduce(
            &notification(
                method::TURN_COMPLETED,
                json!({"threadId": THREAD, "turn": {"id": "u", "status": "completed",
                                                    "error": null}}),
            ),
            THREAD,
            now(),
        );
        assert_eq!(
            outcome,
            Outcome::Finish {
                events: Vec::new(),
                cancelled: None,
                failure: None
            }
        );
    }

    /// A terminal the server cannot route is unsafe to ignore because no later completion can be
    /// tied back to this connection's active stream.
    #[test]
    fn an_unroutable_malformed_terminal_poisons_the_connection() {
        let outcome = reduce(
            &notification(method::TURN_COMPLETED, json!({"unexpected": true})),
            THREAD,
            now(),
        );
        let Outcome::Poison { failure } = outcome else {
            panic!("expected malformed terminal to poison the connection, received {outcome:?}");
        };
        assert_eq!(failure.code.as_str(), "codex-protocol-error");
    }

    /// The one frame whose unknown spelling must not cost the turn. A strict enum would fail the
    /// whole notification here, and the host would hold a stream that never terminates on a
    /// session that refuses every later turn as one already running.
    #[test]
    fn an_ending_this_build_cannot_name_is_still_an_ending() {
        let outcome = reduce(
            &notification(
                method::TURN_COMPLETED,
                json!({"threadId": THREAD, "turn": {"id": "u",
                                                    "status": "somethingTheNextReleaseAdded"}}),
            ),
            THREAD,
            now(),
        );
        assert_eq!(
            outcome,
            Outcome::Finish {
                events: Vec::new(),
                cancelled: None,
                failure: None
            },
            "expected an unknown status to end the turn, received {outcome:?}"
        );
    }

    #[test]
    fn an_interrupted_turn_ends_as_a_cancellation() {
        let outcome = reduce(
            &notification(
                method::TURN_COMPLETED,
                json!({"threadId": THREAD, "turn": {"id": "u", "status": "interrupted"}}),
            ),
            THREAD,
            now(),
        );
        let Outcome::Finish { cancelled, .. } = outcome else {
            panic!("expected the turn to end, received {outcome:?}");
        };
        assert_eq!(cancelled, Some(CancelReason::Requested));
    }

    #[test]
    fn a_failed_turn_carries_what_the_server_said_about_it() {
        let outcome = reduce(
            &notification(
                method::TURN_COMPLETED,
                json!({"threadId": THREAD, "turn": {"id": "u", "status": "failed", "error": {
                    "message": "upstream refused", "additionalDetails": "429"
                }}}),
            ),
            THREAD,
            now(),
        );
        let Outcome::Finish { failure, .. } = outcome else {
            panic!("expected the turn to end, received {outcome:?}");
        };
        let failure = failure.expect("expected a failure");
        assert_eq!(failure.message, "upstream refused: 429");
        assert_eq!(failure.code.as_str(), "codex-turn-failed");
    }

    /// The failure this module exists to prevent. `error` reads like an ending and is not one —
    /// `turn/completed` still follows, and ending here would end the host's turn twice.
    #[test]
    fn an_error_notification_does_not_end_the_turn_the_server_is_still_running() {
        for will_retry in [true, false] {
            let outcome = reduce(
                &notification(
                    method::ERROR,
                    json!({"threadId": THREAD, "turnId": "u", "willRetry": will_retry,
                           "error": {"message": "upstream timed out"}}),
                ),
                THREAD,
                now(),
            );
            assert_eq!(
                outcome,
                Outcome::Ignore,
                "expected willRetry={will_retry} not to end the turn"
            );
        }
    }

    /// The turn the host already started. Announcing it again would be a second beginning.
    #[test]
    fn a_turn_beginning_is_not_an_event_the_host_needs() {
        let outcome = reduce(
            &notification(
                method::TURN_STARTED,
                json!({"threadId": THREAD, "turn": {"id": "u", "status": "inProgress"}}),
            ),
            THREAD,
            now(),
        );
        assert_eq!(outcome, Outcome::Ignore);
    }

    /// Quota belongs to the account, not to a conversation, so it is not routed by thread.
    #[test]
    fn account_quota_reaches_the_turn_even_though_it_names_no_conversation() {
        let outcome = reduce(
            &notification(
                method::ACCOUNT_RATE_LIMITS_UPDATED,
                json!({"rateLimits": {"primary": {"usedPercent": 4.0}, "planType": "plus"}}),
            ),
            THREAD,
            now(),
        );
        let Outcome::Emit(events) = outcome else {
            panic!("expected a quota event, received {outcome:?}");
        };
        let EventKind::AccountLimits { limits } = &events[0] else {
            panic!("expected account limits, received {events:?}");
        };
        assert_eq!(limits.plan_type.as_deref(), Some("plus"));
    }

    #[test]
    fn a_family_this_harness_does_not_act_on_produces_nothing() {
        let outcome = reduce(
            &notification("mcpServer/startupStatus/updated", json!({"name": "exa"})),
            THREAD,
            now(),
        );
        assert_eq!(outcome, Outcome::Ignore);
    }
}
