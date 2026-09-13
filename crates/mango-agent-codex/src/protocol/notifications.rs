//! What the app-server announces, and the envelope every announcement shares.
//!
//! The server sends far more notification families than a host needs. Rather than model each one
//! and grow a variant per release, [`Notification`] names the families this harness acts on and
//! collapses the rest into [`Notification::Other`], which the reducer drops. A family that starts
//! mattering becomes a variant here and a case in the reducer, together.

use serde::Deserialize;
use serde_json::Value;

use super::items::ThreadItem;
use super::requests::{ThreadSummary, TurnHandle};

/// The notification methods this harness acts on, exactly as the app-server spells them.
pub mod method {
    /// A conversation opened.
    pub const THREAD_STARTED: &str = "thread/started";
    /// A turn began running.
    pub const TURN_STARTED: &str = "turn/started";
    /// A turn ended, however it ended. The only terminal the reducer trusts.
    pub const TURN_COMPLETED: &str = "turn/completed";
    /// An item appeared.
    pub const ITEM_STARTED: &str = "item/started";
    /// An item finished.
    pub const ITEM_COMPLETED: &str = "item/completed";
    /// A piece of the answer.
    pub const AGENT_MESSAGE_DELTA: &str = "item/agentMessage/delta";
    /// A piece of the reasoning.
    pub const REASONING_TEXT_DELTA: &str = "item/reasoning/textDelta";
    /// A piece of the reasoning summary, which is what a default build streams.
    pub const REASONING_SUMMARY_TEXT_DELTA: &str = "item/reasoning/summaryTextDelta";
    /// Tokens this turn and this thread have used.
    pub const THREAD_TOKEN_USAGE_UPDATED: &str = "thread/tokenUsage/updated";
    /// The account's plan quota, as the server rolls it forward.
    pub const ACCOUNT_RATE_LIMITS_UPDATED: &str = "account/rateLimits/updated";
    /// A question the server had asked is no longer waiting for this client.
    pub const SERVER_REQUEST_RESOLVED: &str = "serverRequest/resolved";
    /// A failure the server reports without ending the turn.
    pub const ERROR: &str = "error";
}

/// One announcement, in the families this harness acts on.
#[derive(Clone, Debug, PartialEq)]
pub enum Notification {
    /// A conversation opened.
    ThreadStarted(ThreadStarted),
    /// A turn began running.
    TurnStarted(TurnNotification),
    /// A turn ended.
    TurnCompleted(TurnNotification),
    /// An item appeared.
    ItemStarted(ItemNotification),
    /// An item finished.
    ItemCompleted(ItemNotification),
    /// A piece of the answer.
    AgentMessageDelta(AgentMessageDelta),
    /// A piece of the reasoning, whether streamed as text or as a summary.
    ReasoningDelta(ReasoningDelta),
    /// Tokens used.
    ThreadTokenUsage(TurnTokenUsage),
    /// The account's plan quota.
    RateLimits(RateLimitsUpdated),
    /// A question the server is no longer waiting on.
    ServerRequestResolved(ServerRequestResolved),
    /// A failure that does not end the turn.
    Error(ErrorNotification),
    /// A notification family this harness recognises, whose parameters did not decode.
    ///
    /// The routing values are recovered independently of the typed payload. A malformed terminal
    /// still has to end the active stream when the server gave enough information to identify it.
    Malformed {
        /// The method the server used.
        method: String,
        /// The conversation, when the raw params named one as a string.
        thread_id: Option<String>,
        /// The native turn, when the raw params named one as a string.
        turn_id: Option<String>,
    },
    /// A family this harness does not act on.
    ///
    /// Kept as its method name rather than dropped at the parse, so a reducer test can assert that
    /// an unknown family was seen and ignored rather than mistaken for something else.
    Other {
        /// The method the server used.
        method: String,
    },
}

impl Notification {
    /// Reads one announcement, or `Other` when it is a family this harness does not act on.
    #[must_use]
    pub fn parse(method: &str, params: Value) -> Self {
        fn read<T: serde::de::DeserializeOwned>(
            params: Value,
            method: &str,
            wrap: impl FnOnce(T) -> Notification,
        ) -> Notification {
            serde_json::from_value(params.clone()).map_or_else(
                |_| Notification::Malformed {
                    method: method.to_owned(),
                    thread_id: routing_string(&params, "threadId"),
                    turn_id: routing_string(&params, "turnId").or_else(|| {
                        params
                            .pointer("/turn/id")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    }),
                },
                wrap,
            )
        }

        match method {
            method::THREAD_STARTED => read(params, method, Self::ThreadStarted),
            method::TURN_STARTED => read(params, method, Self::TurnStarted),
            method::TURN_COMPLETED => read(params, method, Self::TurnCompleted),
            method::ITEM_STARTED => read(params, method, Self::ItemStarted),
            method::ITEM_COMPLETED => read(params, method, Self::ItemCompleted),
            method::AGENT_MESSAGE_DELTA => read(params, method, Self::AgentMessageDelta),
            method::REASONING_TEXT_DELTA | method::REASONING_SUMMARY_TEXT_DELTA => {
                read(params, method, Self::ReasoningDelta)
            }
            method::THREAD_TOKEN_USAGE_UPDATED => read(params, method, Self::ThreadTokenUsage),
            method::ACCOUNT_RATE_LIMITS_UPDATED => read(params, method, Self::RateLimits),
            method::SERVER_REQUEST_RESOLVED => read(params, method, Self::ServerRequestResolved),
            method::ERROR => read(params, method, Self::Error),
            other => Self::Other {
                method: other.to_owned(),
            },
        }
    }

    /// Which conversation this belongs to, when the server said.
    ///
    /// A session subscribes to one thread and the server writes on several: a subagent's thread
    /// and a detached review's both arrive on the same connection. Anything that does not name
    /// this session's thread is another conversation's, and the reducer drops it.
    #[must_use]
    pub fn thread_id(&self) -> Option<&str> {
        match self {
            Self::ThreadStarted(notification) => Some(notification.thread.id.as_str()),
            Self::TurnStarted(notification) | Self::TurnCompleted(notification) => {
                Some(notification.thread_id.as_str())
            }
            Self::ItemStarted(notification) | Self::ItemCompleted(notification) => {
                Some(notification.thread_id.as_str())
            }
            Self::AgentMessageDelta(delta) => Some(delta.thread_id.as_str()),
            Self::ReasoningDelta(delta) => Some(delta.thread_id.as_str()),
            Self::ThreadTokenUsage(usage) => Some(usage.thread_id.as_str()),
            Self::ServerRequestResolved(resolved) => Some(resolved.thread_id.as_str()),
            Self::Error(error) => Some(error.thread_id.as_str()),
            // Account quota is the account's, not a conversation's.
            Self::Malformed { thread_id, .. } => thread_id.as_deref(),
            Self::RateLimits(_) | Self::Other { .. } => None,
        }
    }

    /// The native turn this belongs to, when this notification family names one.
    /// For example, use `notification.turn_id()` to reject a delayed completion for another turn.
    #[must_use]
    pub fn turn_id(&self) -> Option<&str> {
        match self {
            Self::TurnStarted(notification) | Self::TurnCompleted(notification) => {
                Some(notification.turn.id.as_str())
            }
            Self::ItemStarted(notification) | Self::ItemCompleted(notification) => {
                Some(notification.turn_id.as_str())
            }
            Self::AgentMessageDelta(delta) => Some(delta.turn_id.as_str()),
            Self::ReasoningDelta(delta) => Some(delta.turn_id.as_str()),
            Self::ThreadTokenUsage(usage) => Some(usage.turn_id.as_str()),
            Self::Error(error) => Some(error.turn_id.as_str()),
            Self::Malformed { turn_id, .. } => turn_id.as_deref(),
            Self::ThreadStarted(_)
            | Self::RateLimits(_)
            | Self::ServerRequestResolved(_)
            | Self::Other { .. } => None,
        }
    }

    /// Whether this notification must match the active native turn before it can render or end.
    ///
    /// `turn/started` is excluded. Native review captures show that its id can differ from the
    /// `review/start` response and from every later item and completion for the same review.
    /// For example, gate an id comparison on `notification.requires_native_turn_match()`.
    #[must_use]
    pub fn requires_native_turn_match(&self) -> bool {
        matches!(
            self,
            Self::TurnCompleted(_)
                | Self::ItemStarted(_)
                | Self::ItemCompleted(_)
                | Self::AgentMessageDelta(_)
                | Self::ReasoningDelta(_)
                | Self::ThreadTokenUsage(_)
                | Self::Error(_)
        ) || self.is_malformed_terminal()
    }

    /// Whether malformed terminal parameters can strand an active stream.
    /// For example, `notification.is_malformed_terminal()` selects the fail-closed path.
    #[must_use]
    pub fn is_malformed_terminal(&self) -> bool {
        matches!(
            self,
            Self::Malformed { method, .. } if method == method::TURN_COMPLETED
        )
    }
}

fn routing_string(params: &Value, key: &str) -> Option<String> {
    params.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// A conversation opened.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStarted {
    /// The conversation.
    pub thread: ThreadSummary,
}

/// A turn began or ended.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnNotification {
    /// Which conversation.
    pub thread_id: String,
    /// The turn.
    pub turn: TurnHandle,
}

/// An item appeared or finished.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemNotification {
    /// Which conversation.
    pub thread_id: String,
    /// Which turn.
    pub turn_id: String,
    /// The item itself.
    pub item: ThreadItem,
}

/// A piece of the answer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessageDelta {
    /// Which conversation.
    pub thread_id: String,
    /// Which turn.
    pub turn_id: String,
    /// The text.
    #[serde(default)]
    pub delta: String,
}

/// A piece of the reasoning.
///
/// One type for both families the server streams. A default build withholds the raw reasoning and
/// sends only the summary, so a harness that read one method and not the other would show an empty
/// reasoning phase on every ordinary turn.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningDelta {
    /// Which conversation.
    pub thread_id: String,
    /// Which turn.
    pub turn_id: String,
    /// Which item the reasoning belongs to.
    #[serde(default)]
    pub item_id: String,
    /// The text.
    #[serde(default)]
    pub delta: String,
}

/// Tokens, as the server counts them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsageBreakdown {
    /// Tokens sent.
    #[serde(default)]
    pub input_tokens: Option<u64>,
    /// Tokens served from the vendor's cache.
    #[serde(default)]
    pub cached_input_tokens: Option<u64>,
    /// Tokens written to the vendor's cache.
    #[serde(default)]
    pub cache_write_input_tokens: Option<u64>,
    /// Tokens produced.
    #[serde(default)]
    pub output_tokens: Option<u64>,
    /// Tokens spent reasoning.
    #[serde(default)]
    pub reasoning_output_tokens: Option<u64>,
    /// The vendor's own total.
    #[serde(default)]
    pub total_tokens: Option<u64>,
}

/// This turn and the whole thread, kept apart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTokenUsage {
    /// The whole thread.
    #[serde(default)]
    pub total: Option<TokenUsageBreakdown>,
    /// This turn.
    #[serde(default)]
    pub last: Option<TokenUsageBreakdown>,
    /// The window the active model has.
    #[serde(default)]
    pub model_context_window: Option<u64>,
}

/// A token-usage announcement, with the conversation it belongs to.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnTokenUsage {
    /// Which conversation.
    pub thread_id: String,
    /// Which turn.
    #[serde(default)]
    pub turn_id: String,
    /// The counts.
    pub token_usage: ThreadTokenUsage,
}

/// One metered window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitWindow {
    /// How much of it is used, as a percentage the server computed.
    #[serde(default)]
    pub used_percent: f64,
    /// How long the window is.
    #[serde(default)]
    pub window_duration_mins: Option<u32>,
    /// Unix seconds when it resets.
    #[serde(default)]
    pub resets_at: Option<i64>,
}

/// The account's plan quota, as one snapshot.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitSnapshot {
    /// The shorter window, usually hours.
    #[serde(default)]
    pub primary: Option<RateLimitWindow>,
    /// The longer one, usually a week.
    #[serde(default)]
    pub secondary: Option<RateLimitWindow>,
    /// The plan the vendor named.
    #[serde(default)]
    pub plan_type: Option<String>,
}

/// A quota announcement.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitsUpdated {
    /// The snapshot.
    pub rate_limits: RateLimitSnapshot,
}

/// A question the server is no longer waiting on.
///
/// The server resolves its own approvals when a turn is interrupted or a policy answers first. A
/// harness that only ever removed a pending question on receiving an answer would hold the waiter
/// — and the task composing the reply — for the rest of the session.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerRequestResolved {
    /// Which conversation.
    pub thread_id: String,
    /// Which question, in whatever JSON shape its id had.
    pub request_id: Value,
}

/// A failure the server reports without ending the turn.
///
/// Not a terminal: `willRetry` says the server intends to try again, and `turn/completed` is still
/// coming either way. Emitting a turn-ending event here would end the host's turn twice.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorNotification {
    /// Which conversation.
    pub thread_id: String,
    /// Which turn.
    #[serde(default)]
    pub turn_id: String,
    /// What went wrong.
    pub error: super::requests::TurnError,
    /// Whether the server means to try again.
    #[serde(default)]
    pub will_retry: bool,
}

#[cfg(test)]
mod tests {
    use super::{Notification, method};
    use serde_json::json;

    #[test]
    fn a_family_this_harness_does_not_act_on_is_kept_as_its_name() {
        let notification = Notification::parse("mcpServer/startupStatus/updated", json!({}));
        assert_eq!(
            notification,
            Notification::Other {
                method: String::from("mcpServer/startupStatus/updated")
            }
        );
        assert_eq!(notification.thread_id(), None);
    }

    /// A malformed terminal keeps the routing values needed to end its addressed stream.
    #[test]
    fn a_malformed_terminal_keeps_its_method_and_any_routing_values() {
        let notification = Notification::parse(
            method::TURN_COMPLETED,
            json!({"threadId": "thread-1", "turn": {"id": "turn-1", "status": 7}}),
        );
        assert_eq!(
            notification,
            Notification::Malformed {
                method: String::from(method::TURN_COMPLETED),
                thread_id: Some(String::from("thread-1")),
                turn_id: Some(String::from("turn-1")),
            }
        );
    }

    /// Both reasoning families land on one variant: a default build streams only the summary.
    #[test]
    fn reasoning_text_and_reasoning_summary_are_the_same_event() {
        let params = json!({"threadId": "t", "turnId": "u", "itemId": "i", "delta": "thinking"});
        for family in [
            method::REASONING_TEXT_DELTA,
            method::REASONING_SUMMARY_TEXT_DELTA,
        ] {
            let Notification::ReasoningDelta(delta) = Notification::parse(family, params.clone())
            else {
                panic!("expected a reasoning delta from {family}");
            };
            assert_eq!(delta.delta, "thinking");
        }
    }

    /// A session subscribes to one thread; the connection carries several.
    #[test]
    fn every_conversation_scoped_family_names_its_thread() {
        let cases = [
            (
                method::TURN_STARTED,
                json!({"threadId": "t", "turn": {"id": "u"}}),
            ),
            (
                method::ITEM_STARTED,
                json!({"threadId": "t", "turnId": "u",
                       "item": {"type": "agentMessage", "id": "i", "text": "hi"}}),
            ),
            (
                method::AGENT_MESSAGE_DELTA,
                json!({"threadId": "t", "turnId": "u", "itemId": "i", "delta": "x"}),
            ),
            (
                method::SERVER_REQUEST_RESOLVED,
                json!({"threadId": "t", "requestId": 0}),
            ),
        ];
        for (family, params) in cases {
            let notification = Notification::parse(family, params);
            assert_eq!(
                notification.thread_id(),
                Some("t"),
                "expected {family} to name its thread, received {notification:?}"
            );
        }
    }

    /// A request id may be a number. Keeping it as JSON is what lets it be echoed back as one.
    #[test]
    fn a_resolved_question_keeps_its_id_in_whatever_shape_it_arrived() {
        let Notification::ServerRequestResolved(resolved) = Notification::parse(
            method::SERVER_REQUEST_RESOLVED,
            json!({"threadId": "t", "requestId": 0}),
        ) else {
            panic!("expected a resolution");
        };
        assert_eq!(resolved.request_id, json!(0));
    }

    #[test]
    fn a_quota_snapshot_reads_both_windows_and_the_plan() {
        let Notification::RateLimits(update) = Notification::parse(
            method::ACCOUNT_RATE_LIMITS_UPDATED,
            json!({"rateLimits": {
                "primary": {"usedPercent": 4.0, "windowDurationMins": 300, "resetsAt": 1789301053},
                "secondary": {"usedPercent": 6.0, "windowDurationMins": 10080, "resetsAt": null},
                "planType": "plus"
            }}),
        ) else {
            panic!("expected a quota snapshot");
        };
        let windows = update.rate_limits;
        assert_eq!(windows.primary.map(|window| window.used_percent), Some(4.0));
        assert_eq!(windows.secondary.and_then(|window| window.resets_at), None);
        assert_eq!(windows.plan_type.as_deref(), Some("plus"));
    }

    /// An error notification is a report, not a terminal: the server still sends `turn/completed`.
    #[test]
    fn an_error_notification_says_whether_the_server_means_to_try_again() {
        let Notification::Error(error) = Notification::parse(
            method::ERROR,
            json!({"threadId": "t", "turnId": "u",
                   "error": {"message": "upstream timed out"}, "willRetry": true}),
        ) else {
            panic!("expected an error notification");
        };
        assert!(error.will_retry);
        assert_eq!(error.error.message, "upstream timed out");
    }
}
