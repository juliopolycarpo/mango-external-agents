//! The neutral event contract every harness normalises onto.
//!
//! One invariant runs through the whole vocabulary:
//!
//! > External agents never own the host's tools. They use their own, and the host only surfaces
//! > them in its interface.
//!
//! That is why the tool-shaped events are called *activity*: they are observational. Nothing here
//! can be handed to a tool executor, because nothing here names a host tool.
//!
//! Every event carries the session and turn it belongs to and the instant it was stamped, so a
//! host can route, log or persist one without matching on its kind first.

use std::fmt;
use std::time::SystemTime;

use crate::error::{Result, VendorError};
use crate::normalize::{self, COMMAND_CATALOG_MAX_ITEMS, TextLimit};
use crate::permission::{ApprovalDecision, PermissionRequest};
use crate::session::CancelReason;

/// The host's own id for a session.
///
/// Distinct from the vendor's `native_session_id`: this one the host minted and can rely on, and
/// the other is an opaque handle the vendor may recycle, reshape or forget.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    /// Names a session.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The host's own id for a turn, which is also its idempotency key.
///
/// A retry that means "the same turn" reuses it; a retry that means "a new turn" mints a new one.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct TurnId(String);

impl TurnId {
    /// Names a turn.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TurnId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One normalised event, stamped with where and when it happened.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEvent {
    /// The session this belongs to.
    pub session_id: SessionId,
    /// The turn this belongs to.
    pub turn_id: TurnId,
    /// When the library stamped it, from the host's clock.
    pub at: SystemTime,
    /// What happened.
    pub kind: EventKind,
}

impl AgentEvent {
    /// Whether this event ends its turn.
    ///
    /// [`EventKind::Cancelled`] is a marker rather than a terminal: it is emitted immediately
    /// before [`EventKind::Completed`] and never instead of it, so a host that does not recognise
    /// it still sees its turn end.
    pub fn is_terminal(&self) -> bool {
        matches!(self.kind, EventKind::Completed | EventKind::Error { .. })
    }
}

/// What happened, in the vocabulary every harness normalises onto.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum EventKind {
    /// The vendor opened or resumed its own session.
    SessionStarted {
        /// The vendor's own session handle.
        native_session_id: String,
        /// Whether the vendor resumed an existing conversation rather than starting one.
        resumed: bool,
    },
    /// The session's slash-command catalog, as the vendor announced it.
    ///
    /// Session state rather than transcript: it describes what a user may type next, so it is
    /// never persisted as a message, and the last one received wins.
    CommandsAvailable {
        /// The commands this session will expand.
        commands: Vec<Command>,
    },
    /// A piece of the answer.
    TextDelta {
        /// The text, sanitised but not cut to a label's length.
        text: String,
    },
    /// A reasoning block opened.
    ///
    /// Payload-free on purpose. On current models the vendor default withholds reasoning text, so
    /// a whole reasoning phase can otherwise produce no events at all while the turn stays open.
    ReasoningStarted,
    /// A piece of the reasoning.
    ReasoningDelta {
        /// The text, sanitised but not cut to a label's length.
        text: String,
    },
    /// The reasoning block opened by the last [`EventKind::ReasoningStarted`] closed.
    ///
    /// The other half of the pair, and what turns "is this phase empty because it is still
    /// running, or because the vendor withheld it?" from a guess into a statement.
    ReasoningEnded,
    /// The vendor started doing something of its own.
    ActivityStarted {
        /// The vendor's id for this activity, echoed back when it is approved or resolved.
        call_id: String,
        /// What it is doing.
        activity: Activity,
    },
    /// An activity's title or detail changed.
    ActivityUpdated {
        /// Which activity.
        call_id: String,
        /// What changed.
        update: ActivityUpdate,
    },
    /// An activity finished.
    ActivityCompleted {
        /// Which activity.
        call_id: String,
        /// How it ended.
        result: ActivityResult,
    },
    /// The vendor is asking whether it may do something, and the turn is waiting.
    ///
    /// Answered through [`Session::respond`](crate::Session::respond). Nothing in the library
    /// answers one on the vendor's behalf; a host with a policy implements a
    /// [`PermissionBroker`](crate::PermissionBroker) and the answer is still the host's.
    ApprovalRequested {
        /// What is being asked, and the choices the vendor offered.
        request: PermissionRequest,
    },
    /// An approval was answered, however it was reached.
    ApprovalResolved {
        /// Which question.
        request_id: String,
        /// What was decided.
        decision: ApprovalDecision,
    },
    /// Tokens this turn used, as the vendor reported them.
    Usage {
        /// What it reported.
        usage: Usage,
    },
    /// Tokens the whole thread has used.
    ThreadUsage {
        /// What it reported.
        usage: ThreadUsage,
    },
    /// Account-level plan quota the vendor reported.
    AccountLimits {
        /// What it reported.
        limits: AccountLimits,
    },
    /// The turn stopped before it finished.
    ///
    /// A marker, not a terminal: [`EventKind::Completed`] still follows.
    Cancelled {
        /// Why it stopped.
        reason: CancelReason,
    },
    /// The turn is over.
    Completed,
    /// The turn failed.
    Error {
        /// What went wrong, with the vendor's own structure intact.
        error: VendorError,
    },
}

impl EventKind {
    /// This event with every vendor-supplied value bounded.
    ///
    /// Applied by [`EventSink`](crate::EventSink) so a harness cannot emit an unbounded event by
    /// accident. Exposed because a harness's own reducer tests want to assert on the result.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidVendorValue`](crate::Error::InvalidVendorValue) when an opaque id does not
    /// survive bounding. Such an id is refused rather than shortened: a truncated id echoed back
    /// to the vendor would name a different object.
    pub fn normalized(self) -> Result<Self> {
        Ok(match self {
            Self::SessionStarted {
                native_session_id,
                resumed,
            } => Self::SessionStarted {
                native_session_id: normalize::opaque_id(&native_session_id, "native session id")?,
                resumed,
            },
            Self::CommandsAvailable { commands } => Self::CommandsAvailable {
                commands: normalize_commands(commands),
            },
            Self::TextDelta { text } => Self::TextDelta {
                text: normalize::sanitize_field(&text).text,
            },
            Self::ReasoningDelta { text } => Self::ReasoningDelta {
                text: normalize::sanitize_field(&text).text,
            },
            Self::ActivityStarted { call_id, activity } => Self::ActivityStarted {
                call_id: normalize::opaque_id(&call_id, "activity call id")?,
                activity: activity.normalized(),
            },
            Self::ActivityUpdated { call_id, update } => Self::ActivityUpdated {
                call_id: normalize::opaque_id(&call_id, "activity call id")?,
                update: update.normalized(),
            },
            Self::ActivityCompleted { call_id, result } => Self::ActivityCompleted {
                call_id: normalize::opaque_id(&call_id, "activity call id")?,
                result: result.normalized(),
            },
            Self::ApprovalRequested { request } => Self::ApprovalRequested {
                request: request.normalized()?,
            },
            Self::ApprovalResolved {
                request_id,
                decision,
            } => Self::ApprovalResolved {
                request_id: normalize::opaque_id(&request_id, "approval request id")?,
                decision: ApprovalDecision {
                    option_id: normalize::opaque_id(&decision.option_id, "approval option id")?,
                    source: decision.source,
                },
            },
            Self::AccountLimits { limits } => Self::AccountLimits {
                limits: limits.normalized(),
            },
            Self::Error { error } => Self::Error {
                error: normalize_error(error),
            },
            // Nothing a vendor wrote: the whole event is its kind, or numbers with their own bounds.
            unchanged @ (Self::ReasoningStarted
            | Self::ReasoningEnded
            | Self::Usage { .. }
            | Self::ThreadUsage { .. }
            | Self::Cancelled { .. }
            | Self::Completed) => unchanged,
        })
    }
}

/// One slash command a session can expand, as the vendor announced it.
///
/// The catalog is the vendor's rather than the host's, because the CLI is the only thing that
/// knows what it actually loaded: user commands from disk, plugin commands a marketplace added,
/// and the skills a build exposes under the same prefix. A list rebuilt from a directory scan
/// would confidently offer commands the running binary never read.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Command {
    /// Invoked as `/name`. The vendor's own spelling, never re-slugged.
    pub name: String,
    /// One line of help, when the vendor wrote one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A neutral bucket for what a vendor is doing, small enough that every vendor maps onto it.
///
/// It picks an icon and nothing else. Assistant text and reasoning are *not* activity — they are
/// [`EventKind::TextDelta`] and [`EventKind::ReasoningDelta`].
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ActivityKind {
    /// A shell command.
    Command,
    /// A file was written, edited or deleted.
    FileChange,
    /// A call to an MCP server.
    Mcp,
    /// A subagent the vendor started.
    Subagent,
    /// A web search.
    WebSearch,
    /// An image the vendor produced or read.
    Image,
    /// A plan the vendor wrote.
    Plan,
    /// A review the vendor ran.
    Review,
    /// The vendor compacted its own context.
    Compaction,
    /// Anything else the vendor does.
    Other,
}

/// What the vendor is doing, as something to render.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    /// The vendor's own tool name, verbatim.
    ///
    /// The label, so it is never prettified or translated — and it is rendered as plain text,
    /// never as markdown or HTML.
    pub name: String,
    /// Which bucket it falls in.
    pub kind: ActivityKind,
    /// A one-line summary.
    pub title: String,
    /// More, when the vendor said more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// True when any field above was cut to fit its bound.
    #[serde(default)]
    pub truncated: bool,
}

impl Activity {
    /// This activity with every field bounded.
    #[must_use]
    pub fn normalized(self) -> Self {
        let name = normalize::bound_text(&self.name, TextLimit::ActivityName);
        let title = normalize::bound_text(&self.title, TextLimit::Title);
        let detail = self
            .detail
            .map(|detail| normalize::bound_text(&detail, TextLimit::Detail));
        let truncated = self.truncated
            || name.truncated
            || title.truncated
            || detail.as_ref().is_some_and(|detail| detail.truncated);
        Self {
            name: name.text,
            kind: self.kind,
            title: title.text,
            detail: detail.map(|detail| detail.text),
            truncated,
        }
    }
}

/// What changed about a running activity.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityUpdate {
    /// A new summary, when it changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// New detail, when it changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// True when any field above was cut to fit its bound.
    #[serde(default)]
    pub truncated: bool,
}

impl ActivityUpdate {
    /// This update with every field bounded.
    #[must_use]
    pub fn normalized(self) -> Self {
        let title = self
            .title
            .map(|title| normalize::bound_text(&title, TextLimit::Title));
        let detail = self
            .detail
            .map(|detail| normalize::bound_text(&detail, TextLimit::Detail));
        let truncated = self.truncated
            || title.as_ref().is_some_and(|title| title.truncated)
            || detail.as_ref().is_some_and(|detail| detail.truncated);
        Self {
            title: title.map(|title| title.text),
            detail: detail.map(|detail| detail.text),
            truncated,
        }
    }
}

/// How an activity ended.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ActivityStatus {
    /// It finished.
    Completed,
    /// It failed.
    Failed,
    /// It was cancelled.
    Cancelled,
}

/// An activity's outcome.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityResult {
    /// How it ended.
    pub status: ActivityStatus,
    /// What the vendor said about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// True when the detail was cut to fit its bound.
    #[serde(default)]
    pub truncated: bool,
}

impl ActivityResult {
    /// This result with its detail bounded.
    #[must_use]
    pub fn normalized(self) -> Self {
        let detail = self
            .detail
            .map(|detail| normalize::bound_text(&detail, TextLimit::Detail));
        let truncated = self.truncated || detail.as_ref().is_some_and(|detail| detail.truncated);
        Self {
            status: self.status,
            detail: detail.map(|detail| detail.text),
            truncated,
        }
    }
}

/// Tokens, as reported.
///
/// Every field optional: a harness reports only what its vendor reports, and absence is unknown
/// rather than zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    /// Tokens sent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Tokens produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    /// Tokens served from the vendor's cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// Tokens written to the vendor's cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    /// Tokens spent reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// The vendor's own total, when it gives one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
}

/// Cumulative thread usage, with this turn kept separate from the whole thread.
///
/// The two must never be collapsed: a per-turn display reading the thread total would grow
/// monotonically and mislead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadUsage {
    /// This turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<Usage>,
    /// The whole thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<Usage>,
    /// The window the vendor says the active model has, when it says so.
    ///
    /// The only denominator that makes a percentage honest, so a harness that cannot report one
    /// omits the field rather than guessing a default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<u64>,
}

/// One metered window, as the vendor models it.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitWindow {
    /// The vendor's own label for this window. Passed through, never translated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// How much of it is used.
    pub used_percent: f64,
    /// How long the window is, when the vendor says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_duration_minutes: Option<u32>,
    /// When it resets, when the vendor says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<SystemTime>,
}

/// Account-level plan quota.
///
/// A stale snapshot renders as unknown, never as zero, which is why `observed_at` travels with it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountLimits {
    /// Every metered window the vendor reported, in its own order.
    pub windows: Vec<RateLimitWindow>,
    /// The plan the vendor named, when it named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_type: Option<String>,
    /// When this snapshot was read.
    pub observed_at: SystemTime,
}

impl AccountLimits {
    /// A snapshot with no windows, read now.
    pub fn unknown(observed_at: SystemTime) -> Self {
        Self {
            windows: Vec::new(),
            plan_type: None,
            observed_at,
        }
    }

    /// This snapshot with its vendor labels bounded and its percentages made renderable.
    ///
    /// `used_percent` is a number a harness derives rather than one a vendor spells out, and the
    /// derivation divides: a window the vendor reports with no denominator yields `NaN`, which is
    /// not a percentage and is not even JSON — `serde_json` refuses to write it, so a host that
    /// persists or forwards its events would lose the whole event over one field. Clamped rather
    /// than refused, because a quota reading is not worth ending a turn for.
    #[must_use]
    pub fn normalized(self) -> Self {
        Self {
            windows: self
                .windows
                .into_iter()
                .map(|window| RateLimitWindow {
                    label: window
                        .label
                        .map(|label| normalize::bound_text(&label, TextLimit::Title).text),
                    used_percent: renderable_percent(window.used_percent),
                    ..window
                })
                .collect(),
            plan_type: self
                .plan_type
                .map(|plan| normalize::bound_text(&plan, TextLimit::AccountLabel).text),
            observed_at: self.observed_at,
        }
    }
}

/// A percentage that can be written and rendered: finite, and inside the scale it names.
///
/// `NaN` reads as unknown here, which is what a window with no denominator is, and zero is how an
/// unknown window renders in every quota display there is.
fn renderable_percent(used_percent: f64) -> f64 {
    if used_percent.is_nan() {
        return 0.0;
    }
    used_percent.clamp(0.0, 100.0)
}

/// Bounds a catalog, dropping rows that cannot be offered.
///
/// A name is dropped rather than truncated, on the same terms as an opaque id: a palette inserts
/// it into a composer for the vendor to expand, so a shortened name is a command the CLI does not
/// have. A name with whitespace in it is dropped for the same reason — a completion that splits on
/// the first space would offer half a command and turn the rest into an argument.
fn normalize_commands(commands: Vec<Command>) -> Vec<Command> {
    let mut kept: Vec<Command> = Vec::new();
    for command in commands {
        if kept.len() >= COMMAND_CATALOG_MAX_ITEMS {
            break;
        }
        let name = normalize::bound_text(&command.name, TextLimit::CommandName);
        if name.truncated
            || name.text.is_empty()
            || name.text.chars().any(char::is_whitespace)
            || kept.iter().any(|seen| seen.name == name.text)
        {
            continue;
        }
        let description = command
            .description
            .map(|description| {
                normalize::bound_text(&description, TextLimit::CommandDescription).text
            })
            .filter(|description| !description.is_empty());
        kept.push(Command {
            name: name.text,
            description,
        });
    }
    kept
}

/// Bounds a vendor failure without refusing it.
///
/// An error is the one place a refusal would be self-defeating: the turn is already failing, and
/// dropping the report would leave the host with nothing to show. So the ids here are cut like
/// text rather than refused like the ids that get echoed back.
fn normalize_error(error: VendorError) -> VendorError {
    VendorError {
        code: error.code,
        message: normalize::bound_text(&error.message, TextLimit::ErrorMessage).text,
        request_id: error
            .request_id
            .map(|id| normalize::bound_text(&id, TextLimit::VendorId).text),
        vendor_code: error
            .vendor_code
            .map(|code| normalize::bound_text(&code, TextLimit::VendorId).text),
        retryable: error.retryable,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Activity, ActivityKind, ActivityResult, ActivityStatus, ActivityUpdate, Command, EventKind,
    };
    use crate::error::{Error, ErrorCode, VendorError};

    fn commands(names: &[(&str, Option<&str>)]) -> EventKind {
        EventKind::CommandsAvailable {
            commands: names
                .iter()
                .map(|(name, description)| Command {
                    name: (*name).to_owned(),
                    description: description.map(ToOwned::to_owned),
                })
                .collect(),
        }
    }

    fn normalized_commands(kind: EventKind) -> Vec<Command> {
        match kind.normalized().expect("expected a catalog") {
            EventKind::CommandsAvailable { commands } => commands,
            other => panic!("expected a catalog, received {other:?}"),
        }
    }

    #[test]
    fn drops_command_names_containing_whitespace() {
        // A name with an internal space or tab cannot round-trip through a composer's own `/name`
        // token boundary: a completion splitting on the first whitespace would advertise a command
        // the completion itself truncates.
        let kept = normalized_commands(commands(&[
            ("review notes", None),
            ("review\tnotes", None),
            ("  ", None),
            ("review", None),
        ]));

        assert_eq!(kept.len(), 1, "expected one command, received {kept:?}");
        assert_eq!(kept[0].name, "review");
    }

    #[test]
    fn keeps_single_token_names_and_their_descriptions() {
        let kept = normalized_commands(commands(&[("review", Some("Reviews the diff"))]));

        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name, "review");
        assert_eq!(kept[0].description.as_deref(), Some("Reviews the diff"));
    }

    #[test]
    fn drops_a_duplicate_name_rather_than_offering_it_twice() {
        let kept = normalized_commands(commands(&[
            ("review", Some("first")),
            ("review", Some("second")),
        ]));

        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].description.as_deref(), Some("first"));
    }

    #[test]
    fn drops_a_name_too_long_to_survive_bounding() {
        let long = "n".repeat(129);
        let kept = normalized_commands(commands(&[(&long, None), ("review", None)]));

        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].name, "review");
    }

    #[test]
    fn cuts_an_over_long_catalog_to_the_ceiling() {
        let names: Vec<String> = (0..300).map(|index| format!("command{index}")).collect();
        let kept = normalized_commands(EventKind::CommandsAvailable {
            commands: names
                .iter()
                .map(|name| Command {
                    name: name.clone(),
                    description: None,
                })
                .collect(),
        });

        assert_eq!(kept.len(), 256);
    }

    #[test]
    fn a_payload_free_event_passes_through_unchanged() {
        // This boundary's match is exhaustive with no default, so a kind missed here would not
        // fail loudly — it would fall off the end and the event would be lost on every turn that
        // opens a reasoning block.
        for kind in [EventKind::ReasoningStarted, EventKind::ReasoningEnded] {
            assert_eq!(
                kind.clone().normalized().expect("expected the event back"),
                kind
            );
        }
    }

    #[test]
    fn sanitises_streaming_text_without_cutting_it_to_a_label() {
        let long = "x".repeat(5_000);
        let kind = EventKind::TextDelta {
            text: format!("{long}\u{202e}"),
        };
        match kind.normalized().expect("expected a delta") {
            EventKind::TextDelta { text } => {
                assert_eq!(
                    text, long,
                    "expected the text kept and the override stripped"
                );
            }
            other => panic!("expected a text delta, received {other:?}"),
        }
    }

    #[test]
    fn refuses_an_activity_whose_call_id_cannot_survive_bounding() {
        let kind = EventKind::ActivityStarted {
            call_id: "c".repeat(129),
            activity: Activity {
                name: String::from("Bash"),
                kind: ActivityKind::Command,
                title: String::from("ls"),
                detail: None,
                truncated: false,
            },
        };

        let error = kind
            .normalized()
            .expect_err("expected a refusal, received an event");
        assert!(
            matches!(
                error,
                Error::InvalidVendorValue {
                    field: "activity call id",
                    ..
                }
            ),
            "expected an invalid call id, received {error:?}"
        );
    }

    #[test]
    fn marks_an_activity_truncated_when_any_field_was_cut() {
        let kind = EventKind::ActivityStarted {
            call_id: String::from("call_1"),
            activity: Activity {
                name: "n".repeat(200),
                kind: ActivityKind::Command,
                title: String::from("ls"),
                detail: None,
                truncated: false,
            },
        };
        match kind.normalized().expect("expected an activity") {
            EventKind::ActivityStarted { activity, .. } => {
                assert_eq!(activity.name.chars().count(), 128);
                assert!(activity.truncated);
            }
            other => panic!("expected an activity, received {other:?}"),
        }
    }

    #[test]
    fn an_update_and_a_result_carry_the_truncation_they_caused() {
        let update = ActivityUpdate {
            title: Some("t".repeat(300)),
            detail: None,
            truncated: false,
        }
        .normalized();
        assert!(update.truncated);

        let result = ActivityResult {
            status: ActivityStatus::Failed,
            detail: Some("d".repeat(5_000)),
            truncated: false,
        }
        .normalized();
        assert!(result.truncated);
        assert_eq!(
            result.detail.map(|detail| detail.chars().count()),
            Some(4_096)
        );
    }

    /// `used_percent` is derived, and a derivation divides. A window with no denominator yields
    /// `NaN`, which `serde_json` refuses to write — so a host that persists its events would lose
    /// the whole event over one field rather than one unreadable percentage.
    #[test]
    fn a_percentage_that_could_not_be_written_is_bounded_rather_than_carried() {
        use super::{AccountLimits, RateLimitWindow};
        use std::time::SystemTime;

        let limits = AccountLimits {
            windows: vec![
                RateLimitWindow {
                    used_percent: f64::NAN,
                    ..RateLimitWindow::default()
                },
                RateLimitWindow {
                    used_percent: 412.5,
                    ..RateLimitWindow::default()
                },
                RateLimitWindow {
                    used_percent: -1.0,
                    ..RateLimitWindow::default()
                },
            ],
            plan_type: None,
            observed_at: SystemTime::UNIX_EPOCH,
        }
        .normalized();

        let percentages: Vec<f64> = limits
            .windows
            .iter()
            .map(|window| window.used_percent)
            .collect();
        assert_eq!(percentages, vec![0.0, 100.0, 0.0]);
        serde_json::to_string(&limits).expect("expected a snapshot that can be written");
    }

    #[test]
    fn bounds_an_error_rather_than_refusing_the_turn_that_is_already_failing() {
        let kind = EventKind::Error {
            error: VendorError::new(ErrorCode::from_static("codex-failed"), "m".repeat(4_000))
                .with_vendor_code("v".repeat(200), true),
        };
        match kind.normalized().expect("expected an error event") {
            EventKind::Error { error } => {
                assert_eq!(error.message.chars().count(), 2_048);
                assert_eq!(
                    error.vendor_code.map(|code| code.chars().count()),
                    Some(128)
                );
                assert!(error.retryable);
            }
            other => panic!("expected an error, received {other:?}"),
        }
    }
}
