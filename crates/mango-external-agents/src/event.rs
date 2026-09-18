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

use crate::content::ActivityContent;
use crate::error::{Result, VendorError};
use crate::extension::Extensions;
use crate::interaction::{InteractionId, QuestionOutcome, QuestionRequest};
use crate::normalize::{self, COMMAND_CATALOG_MAX_ITEMS, TextLimit};
use crate::operation::{AttemptId, OperationRef};
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

/// The host's own id for one logical turn.
///
/// A retry that means "the same turn" reuses it; a retry that means "a new turn" mints a new one.
/// Which dispatch of it is [`crate::AttemptId`], and the vendor's own handle is a third
/// thing again — see [`crate::operation`] for why none of the three can stand in for another.
///
/// **Not an idempotency key.** Reusing it does not make a vendor deduplicate anything, and nothing
/// in this library pretends otherwise; whether a dispatch is safe to repeat is
/// [`Dispatch`](crate::Dispatch)'s question.
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
///
/// Every event names the attempt it came from as well as the turn. A late event from an attempt
/// the host has already replaced is one it must not let mutate the attempt that replaced it, and
/// without the attempt on the event there is nothing to check that against —
/// [`OperationRef::is_superseded_by`](crate::OperationRef::is_superseded_by) is the check.
///
/// Nothing that is a fact about the *session* arrives here. The vendor's session handle, the
/// slash-command catalog and the settings in force are read from
/// [`Session::snapshot`](crate::Session::snapshot) and watched through
/// [`Session::subscribe`](crate::Session::subscribe), because they change between turns and before
/// the first one — and carrying them here meant inventing a turn id for something no turn
/// produced.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct AgentEvent {
    /// The session this belongs to.
    pub session_id: SessionId,
    /// The logical turn this belongs to.
    pub turn_id: TurnId,
    /// Which dispatch of that turn produced it.
    pub attempt: AttemptId,
    /// When the library stamped it, from the host's clock.
    pub at: SystemTime,
    /// What happened.
    pub kind: EventKind,
}

impl fmt::Debug for AgentEvent {
    /// Shows event routing metadata without logging host or vendor identifiers.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentEvent")
            .field("has_session_id", &true)
            .field("has_turn_id", &true)
            .field("at", &self.at)
            .field("kind", &self.kind)
            .finish()
    }
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

    /// Which session, turn and attempt this event belongs to.
    pub fn operation(&self) -> OperationRef {
        OperationRef::new(self.session_id.clone(), self.turn_id.clone(), self.attempt)
    }
}

/// What happened, in the vocabulary every harness normalises onto.
///
/// Turn-scoped, all of it. See [`AgentEvent`] for where session-scoped facts went and why.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum EventKind {
    /// The vendor accepted this turn and named its own handle for it.
    ///
    /// The turn-scoped counterpart to opening a session: it says "this attempt is running, and the
    /// vendor calls it this". Emitted once per accepted attempt, before anything else on the
    /// stream.
    TurnStarted {
        /// The vendor's own handle for this turn, for the calls that name one.
        native_turn_id: String,
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
        interaction_id: InteractionId,
        /// What was decided, including how far the chosen option reached.
        decision: ApprovalDecision,
    },
    /// The vendor is asking for information, and the turn is waiting.
    ///
    /// Not an approval. Answering it through [`Session::answer`](crate::Session::answer) tells the
    /// agent something and authorises nothing; a [`PermissionBroker`](crate::PermissionBroker) is
    /// never consulted about one.
    QuestionAsked {
        /// What is being asked, and the choices the vendor offered.
        request: QuestionRequest,
    },
    /// A round of questions ended, however it ended.
    QuestionResolved {
        /// Which round.
        interaction_id: InteractionId,
        /// How it ended.
        outcome: QuestionOutcome,
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

impl fmt::Debug for EventKind {
    /// Formats event shape without replaying prompts, answers, or vendor payloads into logs.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TurnStarted { .. } => formatter.write_str("TurnStarted"),
            Self::QuestionAsked { request } => formatter
                .debug_struct("QuestionAsked")
                .field("question_count", &request.questions.len())
                .finish(),
            Self::QuestionResolved { outcome, .. } => formatter
                .debug_struct("QuestionResolved")
                .field("outcome", outcome)
                .finish(),
            Self::TextDelta { text } => formatter
                .debug_struct("TextDelta")
                .field("text_bytes", &text.len())
                .finish(),
            Self::ReasoningStarted => formatter.write_str("ReasoningStarted"),
            Self::ReasoningDelta { text } => formatter
                .debug_struct("ReasoningDelta")
                .field("text_bytes", &text.len())
                .finish(),
            Self::ReasoningEnded => formatter.write_str("ReasoningEnded"),
            Self::ActivityStarted { activity, .. } => formatter
                .debug_struct("ActivityStarted")
                .field("activity_kind", &activity.kind)
                .finish(),
            Self::ActivityUpdated { update, .. } => formatter
                .debug_struct("ActivityUpdated")
                .field("has_title", &update.title.is_some())
                .field("has_detail", &update.detail.is_some())
                .finish(),
            Self::ActivityCompleted { result, .. } => formatter
                .debug_struct("ActivityCompleted")
                .field("status", &result.status)
                .field("has_detail", &result.detail.is_some())
                .finish(),
            Self::ApprovalRequested { request } => formatter
                .debug_struct("ApprovalRequested")
                .field("request", request)
                .finish(),
            Self::ApprovalResolved { decision, .. } => formatter
                .debug_struct("ApprovalResolved")
                .field("decision", decision)
                .finish(),
            Self::Usage { usage } => formatter
                .debug_struct("Usage")
                .field("usage", usage)
                .finish(),
            Self::ThreadUsage { usage } => formatter
                .debug_struct("ThreadUsage")
                .field("usage", usage)
                .finish(),
            Self::AccountLimits { limits } => formatter
                .debug_struct("AccountLimits")
                .field("window_count", &limits.windows.len())
                .finish(),
            Self::Cancelled { reason } => formatter
                .debug_struct("Cancelled")
                .field("reason", reason)
                .finish(),
            Self::Completed => formatter.write_str("Completed"),
            Self::Error { error } => formatter
                .debug_struct("Error")
                .field("error", error)
                .finish(),
        }
    }
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
            Self::TurnStarted { native_turn_id } => Self::TurnStarted {
                native_turn_id: normalize::opaque_id(&native_turn_id, "native turn id")?,
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
                interaction_id,
                decision,
            } => Self::ApprovalResolved {
                interaction_id: interaction_id.normalized()?,
                decision: ApprovalDecision {
                    option_id: normalize::opaque_id(&decision.option_id, "approval option id")?,
                    ..decision
                },
            },
            Self::QuestionAsked { request } => Self::QuestionAsked {
                request: request.normalized()?,
            },
            Self::QuestionResolved {
                interaction_id,
                outcome,
            } => Self::QuestionResolved {
                interaction_id: interaction_id.normalized()?,
                outcome: outcome.normalized()?,
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
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Command {
    /// Invoked as `/name`. The vendor's own spelling, never re-slugged.
    pub name: String,
    /// One line of help, when the vendor wrote one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl fmt::Debug for Command {
    /// Reports command availability without logging vendor-provided command text.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Command")
            .field("has_name", &!self.name.is_empty())
            .field("has_description", &self.description.is_some())
            .finish()
    }
}

impl Command {
    /// A command with no help text.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
        }
    }

    /// Carries the one line of help the vendor wrote.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// Bounds a whole slash-command catalog before it reaches a host.
///
/// Called by a harness on its way to
/// [`SessionState::set_commands`](crate::SessionState::set_commands), which is where a catalog
/// lives now that it is session state rather than a turn event. A name is dropped rather than
/// truncated, a name with whitespace in it is dropped, a leading `/` is taken off, and the whole
/// catalog is cut to its ceiling — each of those a drop rather than a repair, because a repaired
/// command name is a command the CLI does not have.
///
/// # Example
///
/// ```
/// use mango_external_agents::{Command, event};
///
/// let kept = event::normalized_catalog(vec![
///     Command::new("/review"),
///     Command::new("bad name"),
///     Command::new("compact"),
/// ]);
/// assert_eq!(
///     kept.iter().map(|command| command.name.as_str()).collect::<Vec<_>>(),
///     vec!["review", "compact"]
/// );
/// ```
#[must_use]
pub fn normalized_catalog(commands: Vec<Command>) -> Vec<Command> {
    normalize_commands(commands)
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
///
/// Carries three kinds of thing beyond the label: **identity**, so a later update can address this
/// activity rather than replace the list; **relationships**, so a host can nest what a subagent did
/// under the call that started it; and **content**, so a plan stays a plan and a diff stays a diff
/// instead of both becoming paragraphs of `detail`.
///
/// Everything a vendor reported that has no field here goes to [`extensions`](Self::extensions),
/// which is scalar-only, capped and redacted. There is no raw-frame channel anywhere: a host never
/// has to know a vendor's wire types to read one of these.
#[derive(Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
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
    /// The vendor's own id for the item this activity is, when it has one distinct from the call.
    ///
    /// Several vendors number their transcript items separately from their tool calls, and an
    /// update naming an item id is one a host cannot route without keeping it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    /// The activity this one happened inside, when the vendor nests them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Which subagent did it, when a vendor runs more than one.
    ///
    /// Kept apart from [`parent_id`](Self::parent_id) because they answer different questions: the
    /// parent is where this sits in a tree, the subagent is who was running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_id: Option<String>,
    /// The structured thing it produced, when it produced one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<ActivityContent>,
    /// Useful vendor detail with no field of its own: bounded, observational, never executable.
    #[serde(default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
    /// True when any field above was cut to fit its bound.
    #[serde(default)]
    pub truncated: bool,
}

impl fmt::Debug for Activity {
    /// Reports activity shape without logging vendor-provided labels or detail.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Activity")
            .field("has_name", &!self.name.is_empty())
            .field("kind", &self.kind)
            .field("has_title", &!self.title.is_empty())
            .field("has_detail", &self.detail.is_some())
            .field("truncated", &self.truncated)
            .finish()
    }
}

impl Activity {
    /// An activity with a name, a bucket and a one-line title.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::{Activity, ActivityKind};
    ///
    /// let activity = Activity::new("Bash", ActivityKind::Command, "ls -la");
    /// assert_eq!(activity.name, "Bash");
    /// assert!(activity.extensions.is_empty());
    /// ```
    pub fn new(name: impl Into<String>, kind: ActivityKind, title: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind,
            title: title.into(),
            ..Self::default()
        }
    }

    /// Carries more of what the vendor said.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Carries the vendor's own item id.
    #[must_use]
    pub fn with_item_id(mut self, item_id: impl Into<String>) -> Self {
        self.item_id = Some(item_id.into());
        self
    }

    /// Records the activity this one happened inside.
    #[must_use]
    pub fn inside(mut self, parent_id: impl Into<String>) -> Self {
        self.parent_id = Some(parent_id.into());
        self
    }

    /// Records which subagent was running.
    #[must_use]
    pub fn by_subagent(mut self, subagent_id: impl Into<String>) -> Self {
        self.subagent_id = Some(subagent_id.into());
        self
    }

    /// Carries the structured thing it produced.
    #[must_use]
    pub fn with_content(mut self, content: ActivityContent) -> Self {
        self.content = Some(content);
        self
    }

    /// Carries bounded observational metadata.
    #[must_use]
    pub fn with_extensions(mut self, extensions: Extensions) -> Self {
        self.extensions = extensions;
        self
    }

    /// This activity with every field bounded.
    ///
    /// Ids are dropped rather than cut, and dropping one does not take the activity with it: an
    /// activity nobody can address is still one somebody can read, whereas an activity carrying a
    /// shortened parent id would nest itself under the wrong thing.
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
            item_id: self
                .item_id
                .and_then(|id| normalize::opaque_id(&id, "activity item id").ok()),
            parent_id: self
                .parent_id
                .and_then(|id| normalize::opaque_id(&id, "activity parent id").ok()),
            subagent_id: self
                .subagent_id
                .and_then(|id| normalize::opaque_id(&id, "activity subagent id").ok()),
            content: self.content.map(ActivityContent::normalized),
            extensions: self.extensions.normalized(),
            truncated,
        }
    }
}

impl Default for ActivityKind {
    /// Anything the library has no bucket for.
    fn default() -> Self {
        Self::Other
    }
}

/// What changed about a running activity.
#[derive(Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

impl fmt::Debug for ActivityUpdate {
    /// Reports update shape without logging changed vendor text.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActivityUpdate")
            .field("has_title", &self.title.is_some())
            .field("has_detail", &self.detail.is_some())
            .field("truncated", &self.truncated)
            .finish()
    }
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
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

impl fmt::Debug for ActivityResult {
    /// Reports result status without logging vendor-provided detail.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActivityResult")
            .field("status", &self.status)
            .field("has_detail", &self.detail.is_some())
            .field("truncated", &self.truncated)
            .finish()
    }
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
#[derive(Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
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

impl fmt::Debug for RateLimitWindow {
    /// Reports quota measurements without logging the vendor's window label.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RateLimitWindow")
            .field("has_label", &self.label.is_some())
            .field("used_percent", &self.used_percent)
            .field("window_duration_minutes", &self.window_duration_minutes)
            .field("resets_at", &self.resets_at)
            .finish()
    }
}

/// Account-level plan quota.
///
/// A stale snapshot renders as unknown, never as zero, which is why `observed_at` travels with it.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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

impl fmt::Debug for AccountLimits {
    /// Reports quota snapshot shape without logging plan or window labels.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccountLimits")
            .field("window_count", &self.windows.len())
            .field("has_plan_type", &self.plan_type.is_some())
            .field("observed_at", &self.observed_at)
            .finish()
    }
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
///
/// A leading `/` is taken off rather than dropping the row. Every surface the library drives sends
/// the name bare and leaves the sigil to whoever renders it — Claude Code's `slash_commands` is
/// `["clear", "compact", …]`, ACP's `AvailableCommand.name` is `create_plan`, and the ACP client
/// that inserts one does `format!("/{name}")` — so a name that arrives spelled `/review` is
/// off-contract, and inserting it produces `//review`, a command no CLI has. Taking the slash off
/// recovers the command; dropping the row would lose it.
///
/// Only the *leading* slash: Claude Code namespaces a path-scoped skill as `apps/web:deploy`, and
/// a rule that took every slash out would offer `appsweb:deploy` instead.
fn normalize_commands(commands: Vec<Command>) -> Vec<Command> {
    let mut kept: Vec<Command> = Vec::new();
    for command in commands {
        if kept.len() >= COMMAND_CATALOG_MAX_ITEMS {
            break;
        }
        let bounded = normalize::bound_text(&command.name, TextLimit::CommandName);
        // Bounded first, so a name whose slash sits behind a stripped invisible character is the
        // same name as one that wore it openly.
        let name = bounded.text.trim_start_matches('/');
        if bounded.truncated
            || name.is_empty()
            || name.chars().any(char::is_whitespace)
            || kept.iter().any(|seen| seen.name == name)
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
            name: name.to_owned(),
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
        AccountLimits, Activity, ActivityKind, ActivityResult, ActivityStatus, ActivityUpdate,
        Command, EventKind, RateLimitWindow,
    };
    use crate::error::{Error, ErrorCode, VendorError};
    use crate::interaction::{
        Answer, AnswerValue, InteractionId, QuestionId, QuestionOptionId, QuestionOutcome,
        UnsupportedQuestion,
    };
    use crate::normalize::TextLimit;

    fn commands(names: &[(&str, Option<&str>)]) -> Vec<Command> {
        names
            .iter()
            .map(|(name, description)| Command {
                name: (*name).to_owned(),
                description: description.map(ToOwned::to_owned),
            })
            .collect()
    }

    fn normalized_commands(commands: Vec<Command>) -> Vec<Command> {
        super::normalized_catalog(commands)
    }

    #[test]
    fn event_debug_omits_vendor_text_and_error_payloads() {
        let events = [
            EventKind::TextDelta {
                text: String::from("assistant-text-secret"),
            },
            EventKind::ReasoningDelta {
                text: String::from("reasoning-text-secret"),
            },
            EventKind::TurnStarted {
                native_turn_id: String::from("native-turn-id-secret"),
            },
            EventKind::Error {
                error: VendorError::new(
                    ErrorCode::from_static("vendor-error"),
                    "vendor-error-message-secret",
                ),
            },
        ];

        for event in events {
            let rendered = format!("{event:?}");
            for secret in [
                "assistant-text-secret",
                "reasoning-text-secret",
                "native-turn-id-secret",
                "vendor-error-message-secret",
            ] {
                assert!(
                    !rendered.contains(secret),
                    "expected no event payload in debug output, received {rendered}"
                );
            }
        }
    }

    #[test]
    fn direct_event_payload_debug_omits_vendor_text() {
        let command = Command {
            name: String::from("command-name-secret"),
            description: Some(String::from("command-description-secret")),
        };
        let activity = Activity {
            name: String::from("activity-name-secret"),
            kind: ActivityKind::Command,
            title: String::from("activity-title-secret"),
            detail: Some(String::from("activity-detail-secret")),
            truncated: false,
            ..Activity::default()
        };
        let update = ActivityUpdate {
            title: Some(String::from("update-title-secret")),
            detail: Some(String::from("update-detail-secret")),
            truncated: false,
        };
        let result = ActivityResult {
            status: ActivityStatus::Failed,
            detail: Some(String::from("result-detail-secret")),
            truncated: false,
        };
        let window = RateLimitWindow {
            label: Some(String::from("window-label-secret")),
            ..RateLimitWindow::default()
        };
        let limits = AccountLimits {
            windows: vec![window.clone()],
            plan_type: Some(String::from("plan-type-secret")),
            observed_at: std::time::SystemTime::UNIX_EPOCH,
        };

        for rendered in [
            format!("{command:?}"),
            format!("{activity:?}"),
            format!("{update:?}"),
            format!("{result:?}"),
            format!("{window:?}"),
            format!("{limits:?}"),
        ] {
            for secret in [
                "command-name-secret",
                "command-description-secret",
                "activity-name-secret",
                "activity-title-secret",
                "activity-detail-secret",
                "update-title-secret",
                "update-detail-secret",
                "result-detail-secret",
                "window-label-secret",
                "plan-type-secret",
            ] {
                assert!(
                    !rendered.contains(secret),
                    "expected no vendor payload in direct event diagnostics, received {rendered}"
                );
            }
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

    /// Every surface the library drives sends the name bare and leaves the sigil to whoever
    /// renders it, so a name that arrives wearing one is off-contract — and a palette that inserts
    /// `/{name}` turns it into `//review`, which no CLI answers to. The command is recovered
    /// rather than dropped: the vendor has the command, it only spelled the announcement wrong.
    #[test]
    fn takes_off_a_leading_slash_a_vendor_should_not_have_sent() {
        let kept = normalized_commands(commands(&[
            ("/review", Some("Reviews the diff")),
            ("//compact", None),
        ]));

        assert_eq!(kept.len(), 2, "expected two commands, received {kept:?}");
        assert_eq!(kept[0].name, "review");
        assert_eq!(kept[0].description.as_deref(), Some("Reviews the diff"));
        assert_eq!(kept[1].name, "compact");
    }

    /// Only the leading one. Claude Code namespaces a path-scoped skill as `apps/web:deploy`, so a
    /// rule that took every slash out would offer `appsweb:deploy` — a command that does not exist
    /// — while fixing a spelling nobody sent.
    #[test]
    fn keeps_a_slash_that_is_part_of_a_namespaced_name() {
        let kept = normalized_commands(commands(&[
            ("apps/web:deploy", None),
            ("my-plugin:custom-command", None),
        ]));

        assert_eq!(kept.len(), 2, "received {kept:?}");
        assert_eq!(kept[0].name, "apps/web:deploy");
        assert_eq!(kept[1].name, "my-plugin:custom-command");
    }

    /// A name that was nothing but its sigil names no command, so it goes the way of an empty one.
    #[test]
    fn drops_a_name_that_was_only_a_slash() {
        let kept = normalized_commands(commands(&[("/", None), ("///", None), ("review", None)]));

        assert_eq!(kept.len(), 1, "received {kept:?}");
        assert_eq!(kept[0].name, "review");
    }

    /// Taking the slash off can make two announcements the same name. They were always the same
    /// command: invocation is `/` + name, so both reach the vendor as `/review` and there is no
    /// way to type the other. The existing dedup keeps the first, which is the one whose
    /// description a palette shows.
    #[test]
    fn a_slashed_name_and_its_bare_twin_are_one_command() {
        let kept = normalized_commands(commands(&[
            ("review", Some("the bare one")),
            ("/review", Some("the slashed one")),
        ]));

        assert_eq!(kept.len(), 1, "received {kept:?}");
        assert_eq!(kept[0].name, "review");
        assert_eq!(kept[0].description.as_deref(), Some("the bare one"));
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
        let kept = normalized_commands(
            (0..300)
                .map(|index| Command::new(format!("command{index}")))
                .collect(),
        );

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
            activity: Activity::new("Bash", ActivityKind::Command, "ls"),
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

    /// The label on an unrecognised form is vendor text, and `normalized` promises every
    /// vendor-supplied value is bounded. Passing the outcome through untouched let it past the one
    /// door a turn event has.
    #[test]
    fn bounds_the_vendor_label_a_refused_question_carries() {
        let kind = EventKind::QuestionResolved {
            interaction_id: InteractionId::new("ask-1"),
            outcome: QuestionOutcome::Refused {
                reason: UnsupportedQuestion::UnrecognisedForm {
                    received: "f".repeat(4_096),
                },
            },
        };

        let EventKind::QuestionResolved {
            outcome:
                QuestionOutcome::Refused {
                    reason: UnsupportedQuestion::UnrecognisedForm { received },
                },
            ..
        } = kind.normalized().expect("expected a bounded event")
        else {
            panic!("expected the refusal to survive as a refusal");
        };
        assert_eq!(received.chars().count(), TextLimit::Title.max_code_points());
    }

    /// An answer a harness reports itself never passed `QuestionRequest::validate`, so the sink is
    /// the only place its ids are held to a bound.
    #[test]
    fn refuses_an_answered_outcome_whose_option_id_cannot_survive_bounding() {
        let kind = EventKind::QuestionResolved {
            interaction_id: InteractionId::new("ask-1"),
            outcome: QuestionOutcome::Answered {
                answers: vec![Answer::new(
                    QuestionId::new("branch"),
                    AnswerValue::Chosen {
                        option_ids: vec![QuestionOptionId::new("o".repeat(129))],
                    },
                )],
            },
        };

        let error = kind
            .normalized()
            .expect_err("expected a refusal, received an event");
        assert!(
            matches!(
                error,
                Error::InvalidVendorValue {
                    field: "question option id",
                    ..
                }
            ),
            "expected an invalid option id, received {error:?}"
        );
    }

    #[test]
    fn marks_an_activity_truncated_when_any_field_was_cut() {
        let kind = EventKind::ActivityStarted {
            call_id: String::from("call_1"),
            activity: Activity::new("n".repeat(200), ActivityKind::Command, "ls"),
        };
        match kind.normalized().expect("expected an activity") {
            EventKind::ActivityStarted { activity, .. } => {
                assert_eq!(activity.name.chars().count(), 128);
                assert!(activity.truncated);
            }
            other => panic!("expected an activity, received {other:?}"),
        }
    }

    /// Identity and relationships are what let a host nest a subagent's work under the call that
    /// started it, and address one activity in a later update instead of replacing the list.
    #[test]
    fn an_activity_keeps_its_identity_relationships_and_typed_content() {
        use crate::content::{ActivityContent, PlanStep};
        use crate::extension::{ExtensionValue, Extensions};

        let kind = EventKind::ActivityStarted {
            call_id: String::from("call_1"),
            activity: Activity::new("Task", ActivityKind::Subagent, "plan the work")
                .with_item_id("item_9")
                .inside("call_0")
                .by_subagent("explorer")
                .with_content(ActivityContent::Plan {
                    steps: vec![PlanStep::new("read the reducer").with_id("step-1")],
                })
                .with_extensions(Extensions::new().with("model", ExtensionValue::text("opus"))),
        };

        let EventKind::ActivityStarted { activity, .. } =
            kind.normalized().expect("expected an activity")
        else {
            panic!("expected an activity");
        };
        assert_eq!(activity.item_id.as_deref(), Some("item_9"));
        assert_eq!(activity.parent_id.as_deref(), Some("call_0"));
        assert_eq!(activity.subagent_id.as_deref(), Some("explorer"));
        assert_eq!(
            activity.extensions.get("model"),
            Some(&ExtensionValue::text("opus"))
        );
        let Some(ActivityContent::Plan { steps }) = &activity.content else {
            panic!("expected a plan, received {:?}", activity.content);
        };
        assert_eq!(steps[0].id.as_deref(), Some("step-1"));
    }

    /// An activity nobody can address is still one somebody can read, so an unusable relationship
    /// id is dropped rather than taking the event with it — and dropped rather than cut, because a
    /// shortened parent id would nest the activity under the wrong thing.
    #[test]
    fn an_unusable_relationship_id_is_dropped_without_losing_the_activity() {
        let kind = EventKind::ActivityStarted {
            call_id: String::from("call_1"),
            activity: Activity::new("Task", ActivityKind::Subagent, "work").inside("p".repeat(129)),
        };

        let EventKind::ActivityStarted { activity, .. } =
            kind.normalized().expect("expected the activity to survive")
        else {
            panic!("expected an activity");
        };
        assert_eq!(activity.parent_id, None);
        assert_eq!(activity.title, "work");
    }

    /// The turn-scoped counterpart to opening a session. A vendor handle that cannot be carried
    /// whole is refused rather than cut, like every other id echoed back to a vendor.
    #[test]
    fn a_turn_start_carries_the_vendors_own_handle_and_refuses_an_unusable_one() {
        let started = EventKind::TurnStarted {
            native_turn_id: String::from("turn_abc"),
        }
        .normalized()
        .expect("expected a turn start");
        assert_eq!(
            started,
            EventKind::TurnStarted {
                native_turn_id: String::from("turn_abc")
            }
        );

        let error = EventKind::TurnStarted {
            native_turn_id: "t".repeat(129),
        }
        .normalized()
        .expect_err("expected a refusal");
        assert!(
            matches!(
                error,
                Error::InvalidVendorValue {
                    field: "native turn id",
                    ..
                }
            ),
            "received {error:?}"
        );
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

    /// A quota the harness could not read is unknown, never zero, which is why the snapshot
    /// carries the instant it was read even when it carries no windows.
    #[test]
    fn an_unknown_quota_is_a_snapshot_with_no_windows_rather_than_an_empty_one() {
        use super::AccountLimits;
        use std::time::{Duration, SystemTime};

        let observed_at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let limits = AccountLimits::unknown(observed_at);

        assert!(limits.windows.is_empty());
        assert_eq!(limits.plan_type, None);
        assert_eq!(limits.observed_at, observed_at);
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
