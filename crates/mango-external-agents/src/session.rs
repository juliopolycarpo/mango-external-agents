//! A live conversation with one vendor CLI, and the typed reasons it ends by.
//!
//! The library hands a host [`Session`] handles and reason enums; it keeps no
//! registry of live sessions, polls no consent and fans nothing out to a hub. Those are host
//! policy, and a host builds whatever registry it needs on top of these handles.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use crate::configuration::{ConfigurationOutcome, ConfigurationPatch};
use crate::discovery::DiscoveryReceipt;
use crate::error::{Error, Result};
use crate::event::{AccountLimits, SessionId, TurnId};
use crate::harness::{Capability, SessionCapabilities};
use crate::interaction::QuestionResponse;
use crate::normalize::{self, TextLimit};
use crate::operation::AttemptId;
use crate::permission::PermissionResponse;
use crate::state::{SessionSnapshot, SessionState, SessionSubscription};
use crate::stream::{ReviewStream, TurnStream};
use crate::transport::TransportKind;

/// Why a turn was stopped.
///
/// A reason enum rather than a message: the library never returns copy, and a host maps these to
/// its own words. The distinction between the four is load-bearing — "you stopped this turn" is a
/// lie for a shutdown, and a timeout is not a user's decision.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum CancelReason {
    /// Somebody asked for it: a stop button, an API call.
    Requested,
    /// The machine's owner withdrew permission to run external agents.
    ConsentRevoked,
    /// The turn passed a deadline the host set.
    Timeout,
    /// The host is going away.
    Shutdown,
}

impl fmt::Display for CancelReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Requested => "requested",
            Self::ConsentRevoked => "consent revoked",
            Self::Timeout => "timeout",
            Self::Shutdown => "shutdown",
        })
    }
}

/// Why a session was closed.
///
/// One reason shorter than [`CancelReason`]: a timeout ends a turn, never a session. A session
/// with a stalled turn is still a session, and closing it would throw away the vendor
/// conversation the host may still resume.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum CloseReason {
    /// Somebody asked for it.
    Requested,
    /// The machine's owner withdrew permission to run external agents.
    ConsentRevoked,
    /// The host is going away.
    Shutdown,
}

impl fmt::Display for CloseReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Requested => "requested",
            Self::ConsentRevoked => "consent revoked",
            Self::Shutdown => "shutdown",
        })
    }
}

impl From<CloseReason> for CancelReason {
    /// Closing a session cancels whatever turn was running, for the same reason.
    fn from(reason: CloseReason) -> Self {
        match reason {
            CloseReason::Requested => Self::Requested,
            CloseReason::ConsentRevoked => Self::ConsentRevoked,
            CloseReason::Shutdown => Self::Shutdown,
        }
    }
}

/// The two ids one session answers to.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIds {
    /// The host's own id, which the host minted and can rely on.
    pub session_id: SessionId,
    /// The vendor's own handle, opaque and the vendor's to recycle.
    pub native_session_id: String,
}

impl fmt::Debug for SessionIds {
    /// Reports that the session answers to both, not what either one is.
    ///
    /// The value [`Session::ids`] returns, so this is what a host reaches for with `dbg!` or
    /// embeds in a type of its own — a carrier, unlike [`SessionId`] itself, which is the id and
    /// prints it. The vendor's handle is the vendor's, and [`Resume`] and [`SessionSnapshot`]
    /// report it this way.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionIds")
            .field("has_session_id", &true)
            .field("has_native_session_id", &!self.native_session_id.is_empty())
            .finish_non_exhaustive()
    }
}

/// Whether a session that cannot be resumed should start fresh or fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ResumeMode {
    /// Fail if the vendor cannot resume this conversation.
    Strict,
    /// Start a new conversation instead, and say so in
    /// [`SessionSnapshot::fallback_reason`](crate::SessionSnapshot::fallback_reason).
    Fallback,
}

/// A vendor conversation to continue.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resume {
    /// The vendor's own handle for it.
    pub native_session_id: String,
    /// What to do when the vendor will not.
    pub mode: ResumeMode,
}

impl fmt::Debug for Resume {
    /// Shows the resume policy without exposing the opaque handle supplied to a vendor.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Resume")
            .field("has_native_session_id", &true)
            .field("mode", &self.mode)
            .finish()
    }
}

/// One MCP server a host configured, for a vendor that accepts them.
///
/// Passed through untouched: the library never inspects a server, never connects to one and never
/// puts a vendor's MCP tools into a host's own tool registry. It maps this shape onto whatever the
/// vendor's dialect spells it as and stops there.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServer {
    /// The name the vendor lists this server under.
    pub name: String,
    /// How the vendor reaches it.
    pub transport: McpTransport,
}

impl fmt::Debug for McpServer {
    /// Avoids logging the host's arbitrary server name while retaining transport metadata.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpServer")
            .field("has_name", &!self.name.is_empty())
            .field("transport", &self.transport)
            .finish()
    }
}

impl McpServer {
    /// A server the vendor starts as a child of its own.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::McpServer;
    ///
    /// let server = McpServer::stdio("docs", "docs-mcp");
    /// assert!(server.is_usable());
    /// ```
    pub fn stdio(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            transport: McpTransport::Stdio {
                command: command.into(),
                args: Vec::new(),
                env: std::collections::BTreeMap::new(),
            },
        }
    }

    /// Whether this server is complete enough to hand to a vendor.
    ///
    /// A name or a command the host left empty produces a configuration entry the vendor either
    /// refuses or, worse, reads as something else. Refusing one is the harness's job; saying what
    /// "usable" means is this type's.
    pub fn is_usable(&self) -> bool {
        if self.name.trim().is_empty() {
            return false;
        }
        match &self.transport {
            McpTransport::Stdio { command, .. } => !command.trim().is_empty(),
            McpTransport::Http { url, .. } => !url.trim().is_empty(),
        }
    }
}

/// How a vendor reaches one MCP server.
///
/// The `env` and `headers` maps are the one place a host-supplied value reaches a vendor process,
/// and they are deliberately not the environment allowlist's business: they configure the host's
/// **own** MCP server, which the vendor spawns or dials on the host's behalf, and they never widen
/// what the vendor's own child inherits. A host that puts a token here has decided to give its own
/// server a credential; it cannot use this seam to add anything to the vendor CLI's environment.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum McpTransport {
    /// A child the vendor spawns.
    Stdio {
        /// The executable.
        command: String,
        /// Its arguments.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        /// The environment that server — not the vendor's own child — receives.
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        env: std::collections::BTreeMap<String, String>,
    },
    /// An endpoint the vendor dials.
    Http {
        /// Where it is.
        url: String,
        /// Headers the vendor sends with every request to it.
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        headers: std::collections::BTreeMap<String, String>,
    },
}

impl fmt::Debug for McpTransport {
    /// Hand-written, for the same reason [`WsSpec`](crate::WsSpec) is: this is where a host's own
    /// credential lives.
    ///
    /// `env` and `headers` are declared credential carriers by this type's own documentation, and
    /// every type above this one derives `Debug` from it — so a `tracing::debug!(?request)` of an
    /// open that configured MCP, or the crate's `received {value:?}` assertion idiom, would print
    /// the token in the clear. Values go, names stay: a host debugging a server it misconfigured
    /// still has to see which variable and which header. URLs are omitted because they can carry
    /// arbitrary credentials in a query, while commands retain only their basename and argument
    /// count.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio { command, args, env } => formatter
                .debug_struct("Stdio")
                .field("command", &crate::redact::program_name(command))
                .field("argument_count", &args.len())
                .field("env", &redacted_values(env))
                .finish(),
            Self::Http { headers, .. } => formatter
                .debug_struct("Http")
                .field("endpoint_configured", &true)
                .field("headers", &redacted_values(headers))
                .finish(),
            // No wildcard arm: `#[non_exhaustive]` does not apply inside the defining crate, so a
            // new variant breaks this match rather than silently falling into a `{ .. }` that
            // prints nothing. Deciding what a new field is worth printing is part of adding it.
        }
    }
}

/// Every name, no values.
fn redacted_values(
    map: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<&str, &str> {
    map.keys()
        .map(|name| (name.as_str(), "[REDACTED]"))
        .collect()
}

/// What a host asks for when it opens a session.
///
/// Built rather than struct-literalled: this is the request surface most likely to grow, and a
/// host that wrote `..Default::default()` would silently start defaulting whatever came next.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OpenSession {
    /// The host's own id for the session.
    pub session_id: SessionId,
    /// The settings to open under, as a patch.
    ///
    /// A patch rather than a set of values, so "leave this to the vendor" and "remove whatever
    /// override is configured" stop being the same omission. An empty patch opens the session on
    /// the vendor's own configuration, untouched.
    pub configuration: ConfigurationPatch,
    /// Which carrier to run on, when the host has a preference.
    ///
    /// Absent takes the harness's own first choice. A kind the harness does not declare is refused
    /// by [`validate_open_session`](crate::Harness::validate_open_session) rather than quietly
    /// swapped for one it does.
    pub transport: Option<TransportKind>,
    /// A vendor conversation to continue, when the host is continuing one.
    pub resume: Option<Resume>,
    /// Where this harness's executable is, when the host resolved it.
    ///
    /// Per request, because a resolved path belongs to one harness. It is usually
    /// [`Discovery::executable`](crate::Discovery::executable) from the probe of the same harness
    /// this session is being opened on.
    pub executable: crate::transport::ExecutablePath,
    /// A probe the host already ran and is vouching for, so opening need not run it again.
    ///
    /// Must be bound after current host measurements; opening checks its full identity, effective
    /// transport, workspace, request context and freshness, then forgets it. See
    /// [`DiscoveryReceipt`].
    pub discovery: Option<DiscoveryReceipt>,
    /// MCP servers the vendor should load for this session.
    ///
    /// Honoured only by a harness whose
    /// [`Capabilities::mcp_passthrough`](crate::Capabilities) is set; the rest refuse rather than
    /// accept a request they would silently drop.
    pub mcp_servers: Vec<McpServer>,
}

impl fmt::Debug for OpenSession {
    /// Shows open-session shape without logging opaque ids or host-supplied configuration.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OpenSession")
            .field("has_session_id", &true)
            .field("configuration", &self.configuration)
            .field("resume", &self.resume)
            .field("executable", &self.executable)
            .field("mcp_server_count", &self.mcp_servers.len())
            .finish()
    }
}

impl OpenSession {
    /// A new session that leaves vendor-controlled settings at their configured defaults.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::OpenSession;
    ///
    /// let request = OpenSession::new("chat-42");
    /// assert!(request.configuration.is_empty());
    /// assert!(request.resume.is_none());
    /// assert_eq!(request.transport, None);
    /// ```
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: SessionId::new(session_id),
            configuration: ConfigurationPatch::new(),
            transport: None,
            resume: None,
            executable: crate::transport::ExecutablePath::default(),
            discovery: None,
            mcp_servers: Vec::new(),
        }
    }

    /// Spawns this executable rather than leaving the launcher to resolve the program name.
    ///
    /// The path the host resolved for *this* harness — `Discovery::executable`, usually.
    #[must_use]
    pub fn with_executable(mut self, executable: crate::transport::ExecutablePath) -> Self {
        self.executable = executable;
        self
    }

    /// Runs under this explicit configuration instead of leaving settings to the vendor.
    #[must_use]
    pub fn with_configuration(mut self, configuration: ConfigurationPatch) -> Self {
        self.configuration = configuration;
        self
    }

    /// Runs on this carrier rather than the harness's own first choice.
    #[must_use]
    pub fn over_transport(mut self, transport: TransportKind) -> Self {
        self.transport = Some(transport);
        self
    }

    /// Offers a probe the host already ran, so opening need not run it again.
    #[must_use]
    pub fn with_discovery(mut self, receipt: DiscoveryReceipt) -> Self {
        self.discovery = Some(receipt);
        self
    }

    /// Loads these MCP servers for the session.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::{McpServer, OpenSession};
    ///
    /// let request = OpenSession::new("chat-42")
    ///     .with_mcp_servers(vec![McpServer::stdio("docs", "docs-mcp")]);
    /// assert_eq!(request.mcp_servers.len(), 1);
    /// ```
    #[must_use]
    pub fn with_mcp_servers(mut self, mcp_servers: Vec<McpServer>) -> Self {
        self.mcp_servers = mcp_servers;
        self
    }

    /// Continues the vendor conversation with this handle.
    #[must_use]
    pub fn resuming(mut self, native_session_id: impl Into<String>, mode: ResumeMode) -> Self {
        self.resume = Some(Resume {
            native_session_id: native_session_id.into(),
            mode,
        });
        self
    }
}

/// Explains why resume fell back without including vendor payloads.
///
/// ```
/// use mango_external_agents::{Capability, Error, resume_fallback_reason};
/// let reason = resume_fallback_reason("session/load", &Error::not_supported(Capability::Resume));
/// assert_eq!(reason, "session/load needs resume, which this agent does not declare");
/// ```
pub fn resume_fallback_reason(operation: &str, error: &Error) -> String {
    match error.cause() {
        Error::Vendor(vendor) => format!(
            "{operation} was refused by the vendor ({}, retryable {})",
            vendor.code, vendor.retryable
        ),
        Error::NotSupported { capability } => {
            format!("{operation} needs {capability}, which this agent does not declare")
        }
        Error::Timeout { after, .. } => format!("{operation} did not answer within {after:?}"),
        Error::Protocol { .. } => {
            format!("{operation} answered with a shape this harness does not read")
        }
        Error::Link { .. } => format!("{operation} lost the vendor link"),
        Error::Closed { subject } => format!("{operation} found a closed {subject}"),
        other => format!("{operation} failed: {other}"),
    }
}

/// The largest attachment the vendor wire carries, and how many of them.
pub const ATTACHMENT_MAX_BYTES: usize = 2 * 1024 * 1024;
/// How many attachments one turn may carry.
pub const TURN_MAX_ATTACHMENTS: usize = 4;

/// What one attachment is, for a vendor that takes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum AttachmentKind {
    /// An image.
    Image,
    /// Text.
    Text,
    /// A PDF.
    Pdf,
    /// Bytes with a type the host recognised.
    Data,
    /// Bytes with a type nobody recognised.
    Unknown,
}

/// One file travelling with a turn.
///
/// Bytes rather than base64: a harness encodes for its own dialect, and a host that already has
/// the bytes should not have to encode them for a wire it cannot see.
#[derive(Clone, PartialEq, Eq)]
pub struct Attachment {
    /// The host's own id for it.
    pub id: String,
    /// The name a person would recognise.
    pub name: String,
    /// Its media type.
    pub mime_type: String,
    /// What it is.
    pub kind: AttachmentKind,
    /// The bytes themselves.
    pub bytes: Vec<u8>,
}

impl fmt::Debug for Attachment {
    /// Reports attachment metadata without logging the host's names, ids, or bytes.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Attachment")
            .field("kind", &self.kind)
            .field("byte_count", &self.bytes.len())
            .finish()
    }
}

/// One turn's input.
///
/// Carries two of the three identities a turn has: the logical [`TurnId`] and the
/// [`AttemptId`] of this particular dispatch. The third — the vendor's own handle — does not
/// exist yet, and arrives on the [`TurnStream`] the vendor answers with.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TurnRequest {
    /// The host's own id for this logical turn.
    ///
    /// Stable across retries of the same turn: a retry that means "the same turn, again" reuses
    /// it, and a retry that means "a new turn" mints a new one.
    ///
    /// **It is not an idempotency key.** Nothing in this library, and nothing in any vendor this
    /// library drives, promises that a second dispatch under the same id is deduplicated. What it
    /// does is let a host recognise its own work; whether re-sending is safe is
    /// [`Dispatch`](crate::Dispatch)'s question.
    pub turn_id: TurnId,
    /// Which dispatch of that turn this is.
    pub attempt: AttemptId,
    /// What to say to the agent.
    pub input: String,
    /// Files travelling with it.
    pub attachments: Vec<Attachment>,
    /// Explicit settings for this turn, when they differ from the session's own.
    ///
    /// A patch, so a turn can remove an override the session carries as well as set one.
    pub configuration: Option<ConfigurationPatch>,
}

impl fmt::Debug for TurnRequest {
    /// Shows a turn's shape without logging its prompt or attachment contents.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnRequest")
            .field("has_turn_id", &true)
            .field("input_bytes", &self.input.len())
            .field("attachment_count", &self.attachments.len())
            .field("configuration", &self.configuration)
            .finish()
    }
}

impl TurnRequest {
    /// A turn that is only text, on its first attempt.
    ///
    /// The turn id is the host's: the library does not mint ids, because an id a host cannot
    /// reproduce is an id it cannot reconcile with.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::TurnRequest;
    ///
    /// let turn = TurnRequest::new("turn-1", "say hello");
    /// assert_eq!(turn.turn_id.as_str(), "turn-1");
    /// assert_eq!(turn.attempt.get(), 1);
    /// assert!(turn.attachments.is_empty());
    /// ```
    pub fn new(turn_id: impl Into<String>, input: impl Into<String>) -> Self {
        Self {
            turn_id: TurnId::new(turn_id),
            attempt: AttemptId::default(),
            input: input.into(),
            attachments: Vec::new(),
            configuration: None,
        }
    }

    /// Dispatches the same logical turn under a new attempt.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::{AttemptId, TurnRequest};
    ///
    /// let retry = TurnRequest::new("turn-1", "say hello").as_attempt(AttemptId::new(2));
    /// assert_eq!(retry.turn_id.as_str(), "turn-1");
    /// assert_eq!(retry.attempt.get(), 2);
    /// ```
    #[must_use]
    pub fn as_attempt(mut self, attempt: AttemptId) -> Self {
        self.attempt = attempt;
        self
    }

    /// Carries these files with the turn.
    #[must_use]
    pub fn with_attachments(mut self, attachments: Vec<Attachment>) -> Self {
        self.attachments = attachments;
        self
    }

    /// Runs this turn under an explicit configuration patch.
    #[must_use]
    pub fn with_configuration(mut self, configuration: ConfigurationPatch) -> Self {
        self.configuration = Some(configuration);
        self
    }
}

/// More input for a turn that is already running.
#[derive(Clone, PartialEq, Eq)]
pub struct Steer {
    /// The turn to steer.
    pub turn_id: TurnId,
    /// The vendor's handle for that turn.
    pub native_turn_id: String,
    /// What to add.
    pub input: String,
}

impl fmt::Debug for Steer {
    /// Shows a steer without logging its prompt or opaque vendor identifiers.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Steer")
            .field("has_turn_id", &true)
            .field("has_native_turn_id", &true)
            .field("input_bytes", &self.input.len())
            .finish()
    }
}

/// Whether a steer landed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SteerOutcome {
    /// The vendor took it.
    Accepted,
    /// It did not, and why.
    Rejected {
        /// The reason the vendor gave.
        reason: SteerRejection,
    },
}

/// Why a vendor refused a steer.
///
/// Only the two a vendor can decide. "Not supported" and "no such session" are refused before a
/// harness is called at all, because neither is a fact about the vendor's turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum SteerRejection {
    /// The turn ended before the steer reached it — a race a person can legitimately hit.
    TurnAlreadyCompleted,
    /// This particular turn refuses steering, such as a review or a compaction.
    TurnNotSteerable,
}

/// What a vendor-native review is pointed at.
///
/// Harnesses may reject targets their vendor does not support.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum ReviewTarget {
    /// Staged, unstaged and untracked work, as the vendor defines it. The library does not narrow
    /// the definition.
    UncommittedChanges,
    /// Changes relative to a branch or revision.
    BaseBranch {
        /// The base branch or revision.
        branch: String,
    },
    /// One commit.
    Commit {
        /// The commit hash or revision.
        sha: String,
        /// Optional display title for the review.
        title: Option<String>,
    },
    /// A review directed by host-supplied instructions.
    Custom {
        /// What the reviewer should inspect.
        instructions: String,
    },
}

impl fmt::Debug for ReviewTarget {
    /// Identifies the review mode without logging host-provided revisions or instructions.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UncommittedChanges => formatter.write_str("UncommittedChanges"),
            Self::BaseBranch { .. } => formatter.write_str("BaseBranch { configured: true }"),
            Self::Commit { title, .. } => formatter
                .debug_struct("Commit")
                .field("has_title", &title.is_some())
                .finish(),
            Self::Custom { .. } => formatter.write_str("Custom { configured: true }"),
        }
    }
}

/// Start a vendor-native review on an open session.
///
/// A review is a turn that happens to be a review: it is deduplicated, ordered, cancelled and
/// persisted by the same machinery, so it carries a turn id like any other. It runs under the
/// permissions the session already has — reconfiguring mid-review would be a way around a choice
/// somebody already made.
#[derive(Clone, PartialEq, Eq)]
pub struct ReviewRequest {
    /// The host's own id for this turn.
    pub turn_id: TurnId,
    /// What to review.
    pub target: ReviewTarget,
}

impl fmt::Debug for ReviewRequest {
    /// Shows a review request without logging its opaque turn id.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReviewRequest")
            .field("has_turn_id", &true)
            .field("target", &self.target)
            .finish()
    }
}

/// How many sessions one page may carry.
pub const SESSION_PAGE_LIMIT: usize = 50;

/// Which of a vendor's own sessions to list.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SessionQuery {
    /// Where to continue from, from a previous page.
    pub cursor: Option<String>,
    /// How many to return, up to [`SESSION_PAGE_LIMIT`].
    pub limit: Option<usize>,
    /// Only sessions in this directory, exactly as the vendor spells it.
    pub workspace_path: Option<PathBuf>,
}

impl SessionQuery {
    /// This query with its page size brought inside [`SESSION_PAGE_LIMIT`].
    ///
    /// Applied by [`Session::list_sessions`] before the harness sees it, so the cap is something
    /// the vendor is asked for rather than something the library applies to the answer. A page cut
    /// down afterwards is a page whose tail has no cursor pointing at it — the vendor's cursor
    /// resumes after the last row it *sent*, not after the last row a host was shown.
    ///
    /// An absent limit becomes the cap for the same reason: "as many as you like" is the request
    /// that produces the page there is no way to finish reading.
    #[must_use]
    pub fn normalized(self) -> Self {
        Self {
            limit: Some(
                self.limit
                    .unwrap_or(SESSION_PAGE_LIMIT)
                    .min(SESSION_PAGE_LIMIT),
            ),
            ..self
        }
    }
}

/// One conversation the vendor already owns, as a row in a picker.
///
/// A pointer, not an import: nothing here carries transcript content. Adopting a session records
/// which vendor conversation a chat continues, and the vendor keeps the history it wrote.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeSession {
    /// The vendor's own handle.
    pub native_session_id: String,
    /// A title, when the vendor kept one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// A preview of the conversation, when the vendor offers one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// The vendor's own working directory for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_path: Option<String>,
    /// When it last changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<SystemTime>,
}

/// One page of a vendor's own sessions.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SessionPage {
    /// The rows.
    pub sessions: Vec<NativeSession>,
    /// Where the next page starts, when there is one.
    pub next_cursor: Option<String>,
    /// True when the page held more usable rows than [`SESSION_PAGE_LIMIT`] and the rest were cut.
    ///
    /// Worth carrying because [`next_cursor`](Self::next_cursor) cannot stand in for it: the
    /// cursor is the vendor's own and resumes after the last row the vendor sent, so paging on it
    /// skips whatever the cap removed. A host that sees this set knows the gap is there and that
    /// no cursor will close it.
    ///
    /// A vendor given a bounded [`SessionQuery`] never trips this. Seeing it set means the vendor
    /// returned more than it was asked for.
    pub truncated: bool,
}

impl fmt::Debug for SessionQuery {
    /// Records the shape of the request without the cursor or the directory it names.
    ///
    /// A vendor cursor is an opaque token and a workspace path is host-provided, so both are
    /// reported as present rather than written.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionQuery")
            .field("has_cursor", &self.cursor.is_some())
            .field("limit", &self.limit)
            .field("has_workspace_path", &self.workspace_path.is_some())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for NativeSession {
    /// Records that a row exists without logging the conversation it points at.
    ///
    /// The id, the title, the preview and the working directory are the vendor's own record of
    /// what somebody talked about and where — a title is often the first thing they typed — so a
    /// host that logs a listing logs how many rows it received and what each one carries, not what
    /// any of them say. The fields are public and a host that needs them reads them.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeSession")
            .field("has_title", &self.title.is_some())
            .field("has_preview", &self.preview.is_some())
            .field("has_workspace_path", &self.workspace_path.is_some())
            .field("updated_at", &self.updated_at)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for SessionPage {
    /// Records what the page holds without the rows or the vendor's cursor.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionPage")
            .field("session_count", &self.sessions.len())
            .field("has_next_cursor", &self.next_cursor.is_some())
            .field("truncated", &self.truncated)
            .finish_non_exhaustive()
    }
}

impl SessionPage {
    /// This page with unusable rows dropped rather than repaired.
    ///
    /// A row whose id could not survive bounding is dropped: a truncated session id points at a
    /// different conversation, and adopting one would be worse than not offering it. A path is
    /// sanitised but never shortened, for the same reason — a shortened path names nothing — so an
    /// unbounded one drops its row too.
    ///
    /// Cutting the page to [`SESSION_PAGE_LIMIT`] sets [`truncated`](Self::truncated), which those
    /// per-row drops do not: a row refused as unusable is a row nothing could have shown, while a
    /// row past the cap is one the vendor sent, the host will never see, and the cursor will skip.
    #[must_use]
    pub fn normalized(self) -> Self {
        let mut usable = self.sessions.into_iter().filter_map(|session| {
            let native_session_id =
                normalize::opaque_id(&session.native_session_id, "native session id").ok()?;
            let workspace_path = match session.workspace_path {
                Some(path) => Some(normalize::vendor_path(&path)?),
                None => None,
            };
            Some(NativeSession {
                native_session_id,
                title: bounded_label(session.title),
                preview: bounded_label(session.preview),
                workspace_path,
                updated_at: session.updated_at,
            })
        });
        // Taken lazily, then asked whether a usable row survived the cap, so a vendor that ignored
        // its limit is noticed without the whole overrun being built first.
        let sessions: Vec<NativeSession> = usable.by_ref().take(SESSION_PAGE_LIMIT).collect();
        Self {
            truncated: usable.next().is_some(),
            sessions,
            next_cursor: self.next_cursor,
        }
    }
}

fn bounded_label(raw: Option<String>) -> Option<String> {
    raw.map(|text| normalize::bound_text(&text, TextLimit::SessionTitle).text)
        .filter(|text| !text.is_empty())
}

/// Account-level plan quota, for a vendor that reports one.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AccountUsage {
    /// What the vendor said, or nothing.
    ///
    /// Absence is unknown, never zero: a harness that could not read a baseline — signed out,
    /// timed out — says so by omission rather than by reporting an empty quota.
    pub limits: Option<AccountLimits>,
}

/// One live conversation with a vendor CLI.
///
/// A handle, not a supervisor. The library keeps no registry of open sessions, polls no consent
/// and fans nothing out: those are host policy, and a host builds whatever registry it needs
/// around these handles.
///
/// The four optional methods default to [`Error::NotSupported`], so a harness implements what its
/// vendor has and a host that calls the rest receives a typed refusal.
#[async_trait::async_trait]
pub trait Session: Send + Sync {
    /// This session's live, observable state.
    ///
    /// The one method a harness must supply for the whole session-state surface: everything else
    /// here reads from it. Holding a [`SessionState`] rather than a frozen struct is what lets a
    /// vendor rename its own handle, announce a command catalog, or have its settings change
    /// between turns without any of it having to arrive as a turn event.
    fn state(&self) -> &SessionState;

    /// Everything true about this session at this instant.
    ///
    /// A value, so two fields read off one snapshot were read at one instant.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # fn example(session: &dyn mango_external_agents::Session) {
    /// let snapshot = session.snapshot();
    /// println!("revision {} on {}", snapshot.revision, snapshot.transport.effective);
    /// # }
    /// ```
    fn snapshot(&self) -> Arc<SessionSnapshot> {
        self.state().snapshot()
    }

    /// Watches this session's state change, with no window in which an update can be missed.
    ///
    /// See [`crate::state`] for why read-then-subscribe cannot lose an update here.
    fn subscribe(&self) -> SessionSubscription {
        self.state().subscribe()
    }

    /// The two ids this session answers to, as they stand now.
    ///
    /// Owned rather than borrowed, because this is the one answer a session is allowed to change
    /// after it is open: a vendor may mint its own handle and report a different one once a run
    /// has started, and the value a host persists to resume with has to be that one.
    fn ids(&self) -> SessionIds {
        self.snapshot().ids.clone()
    }

    /// What this session can do, after whatever its handshake narrowed.
    fn capabilities(&self) -> SessionCapabilities {
        self.snapshot().capabilities
    }

    /// Refuses a call that this opened session did not advertise.
    ///
    /// Harness implementations call this before they encode a vendor request. Keeping the check on
    /// the session uses the capability reading from the handshake when a protocol exposes one.
    ///
    /// # Errors
    ///
    /// [`Error::NotSupported`] when the session did not advertise `capability`.
    fn require_capability(&self, capability: Capability) -> Result<()> {
        self.capabilities().require(capability)
    }

    /// Refuses per-turn input that needs a capability the session did not advertise.
    ///
    /// # Errors
    ///
    /// [`Error::NotSupported`] when configuration or an image attachment is unsupported.
    fn validate_turn_request(&self, request: &TurnRequest) -> Result<()> {
        if request.configuration.is_some() {
            self.require_capability(Capability::Configuration)?;
        }
        if request
            .attachments
            .iter()
            .any(|attachment| matches!(attachment.kind, AttachmentKind::Image))
        {
            self.require_capability(Capability::Images)?;
        }
        Ok(())
    }

    /// Starts a turn and returns its bounded event stream.
    ///
    /// A genuinely active or stopping attempt causes a typed busy refusal. Completion releases
    /// admission independently of transcript consumption. Accepted or uncertain work retains an
    /// owned stream; inspect its dispatch certainty before considering a replay.
    /// Dropping this future abandons its attempt. A host supervisor retains the returned stream
    /// across browser disconnects; dropping the stream requests native cleanup.
    ///
    /// # Errors
    ///
    /// Whatever the vendor or the link reported.
    async fn start_turn(&self, request: TurnRequest) -> Result<TurnStream>;

    /// Answers one approval.
    ///
    /// An authorisation decision. [`Session::answer`] is the other half — a reply to a question,
    /// which grants nothing — and the two are separate methods so a host cannot route one to the
    /// other by accident.
    ///
    /// # Errors
    ///
    /// Whatever the vendor or the link reported.
    async fn respond(&self, response: PermissionResponse) -> Result<()>;

    /// Answers one round of questions.
    ///
    /// Information, not authorisation: nothing a host sends here lets the agent do anything it
    /// could not already do. See [`Session::respond`] for the surface that authorises.
    ///
    /// # Errors
    ///
    /// [`Error::NotSupported`] unless the harness implements it.
    async fn answer(&self, _response: QuestionResponse) -> Result<()> {
        Err(Error::not_supported(Capability::Questions))
    }

    /// Changes this session's settings, outside any turn.
    ///
    /// Answers with what actually happened rather than a bare success: most vendors cannot set
    /// several options atomically, so a patch can land in part. See
    /// [`crate::ConfigurationOutcome`] for what partial looks like and
    /// [`Rollback`](crate::Rollback) for what became of the part that had already landed.
    ///
    /// # Errors
    ///
    /// [`Error::NotSupported`] unless the harness implements it.
    async fn configure(&self, _patch: ConfigurationPatch) -> Result<ConfigurationOutcome> {
        Err(Error::not_supported(Capability::SessionConfiguration))
    }

    /// Requests cancellation of the owned turn without granting pending approvals.
    ///
    /// Native cancellation acknowledgement, terminal commitment and process reaping are separate
    /// stages. A stopping attempt continues to own admission until native work cannot conflict.
    ///
    /// # Errors
    ///
    /// Whatever the vendor or the link reported.
    async fn cancel(&self, reason: CancelReason) -> Result<()>;

    /// Ends the session and releases what it holds.
    ///
    /// Idempotent: a close racing a cancel, or two closes from different tasks, must not fail the
    /// second caller.
    ///
    /// # Errors
    ///
    /// Whatever the vendor or the link reported, and whatever a resource this session owns failed
    /// to release. The session is closed either way — a refusal here says what was left behind,
    /// not that the close should be tried again.
    async fn close(&self, reason: CloseReason) -> Result<()>;

    /// Adds to a turn that is already running.
    ///
    /// # Errors
    ///
    /// [`Error::NotSupported`] unless the harness implements it.
    async fn steer(&self, _steer: Steer) -> Result<SteerOutcome> {
        Err(Error::not_supported(Capability::Steering))
    }

    /// Starts a vendor-native review.
    ///
    /// # Errors
    ///
    /// [`Error::NotSupported`] unless the harness implements it.
    async fn start_review(&self, _request: ReviewRequest) -> Result<ReviewStream> {
        Err(Error::not_supported(Capability::NativeReview))
    }

    /// Lists the vendor's own sessions, bounded before a host sees them.
    ///
    /// What a host calls. Like [`Harness::discover`](crate::Harness::discover), it is the half
    /// that applies [`SessionPage::normalized`], so a title or a workspace path a vendor wrote
    /// cannot reach a host's list unbounded.
    ///
    /// # Errors
    ///
    /// [`Error::NotSupported`] unless the harness implements
    /// [`list_native_sessions`](Self::list_native_sessions).
    async fn list_sessions(&self, query: SessionQuery) -> Result<SessionPage> {
        Ok(self
            .list_native_sessions(query.normalized())
            .await?
            .normalized())
    }

    /// The page as the vendor returned it. Implemented by a harness that supports listing.
    ///
    /// # Errors
    ///
    /// [`Error::NotSupported`] unless the harness implements it.
    async fn list_native_sessions(&self, _query: SessionQuery) -> Result<SessionPage> {
        Err(Error::not_supported(Capability::SessionListing))
    }

    /// Reads account-level plan quota.
    ///
    /// # Errors
    ///
    /// [`Error::NotSupported`] unless the harness implements it.
    async fn refresh_account_usage(&self) -> Result<AccountUsage> {
        Err(Error::not_supported(Capability::AccountUsage))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Attachment, AttachmentKind, CancelReason, CloseReason, McpServer, McpTransport,
        NativeSession, OpenSession, ResumeMode, Session, SessionIds, SessionPage, SessionQuery,
        TurnRequest,
    };
    use crate::configuration::{ConfigurationChange, ConfigurationPatch};
    use crate::harness::SessionCapabilities;
    use crate::identity::HarnessIdentity;
    use crate::operation::AttemptId;
    use crate::permission::PermissionLevel;
    use crate::state::{SessionSnapshot, SessionState, TransportSelection};
    use crate::transport::TransportKind;
    use std::time::SystemTime;

    /// The helper both fallback sites depend on, across every arm a resume can fail through.
    ///
    /// Only the vendor arm is reachable from a harness test with a recorded transcript, so the
    /// rest are pinned here: a reason that silently degraded to "thread/resume failed" would be
    /// invisible until a host asked why its history disappeared.
    #[test]
    fn a_resume_fallback_reason_explains_every_way_a_load_can_fail() {
        use crate::error::{Error, ErrorCode, VendorError};
        use std::time::Duration;

        let cases = [
            (
                Error::Vendor(
                    VendorError::new(ErrorCode::from_static("acp-request-failed"), "expired")
                        .with_vendor_code("thread_gone", true),
                ),
                "session/load was refused by the vendor (acp-request-failed, retryable true)",
            ),
            (
                Error::Timeout {
                    operation: String::from("session/load"),
                    after: Duration::from_secs(30),
                },
                "session/load did not answer within 30s",
            ),
            (
                Error::Protocol {
                    expected: String::from("a loaded session"),
                    received: String::from("payload-secret"),
                },
                "session/load answered with a shape this harness does not read",
            ),
            (
                Error::Link {
                    peer: String::from("agent"),
                    message: String::from("link-secret"),
                },
                "session/load lost the vendor link",
            ),
            (
                Error::Closed { subject: "link" },
                "session/load found a closed link",
            ),
        ];

        for (error, expected) in cases {
            assert_eq!(
                crate::resume_fallback_reason("session/load", &error),
                expected
            );
        }
    }

    /// `Debug` stays on the public snapshot, because a host wrapping it derives its own.
    #[test]
    fn session_snapshot_debug_reports_metadata_without_ids_or_vendor_text() {
        let info = SessionSnapshot::opening(
            SessionIds {
                session_id: crate::SessionId::new("chat-id-secret"),
                native_session_id: String::from("native-id-secret"),
            },
            HarnessIdentity::claude(),
            TransportSelection::new(None, TransportKind::Stdio),
            SystemTime::UNIX_EPOCH,
        )
        .with_fallback_reason("fallback-text-secret");

        // `ids()` is the value a host reaches for on its own, so it carries the same claim.
        let rendered = format!("{info:?} {:?}", info.ids);
        for secret in ["chat-id-secret", "native-id-secret", "fallback-text-secret"] {
            assert!(
                !rendered.contains(secret),
                "expected no session payload in diagnostics, received {rendered}"
            );
        }
        assert!(
            rendered.contains("has_native_session_id: true"),
            "expected the vendor handle to be reported as present, received {rendered}"
        );
        assert!(
            rendered.contains("has_fallback_reason: true"),
            "expected the fallback to be reported as present, received {rendered}"
        );
    }

    /// The listing surface carries more of a conversation than anything else the library returns.
    ///
    /// A title is usually the first thing somebody typed, a preview is the conversation itself, and
    /// a workspace path names the machine and often the person. `list_sessions` is the one call
    /// that hands a host a page of them, so a host that logs its result must log a shape.
    #[test]
    fn session_listing_debug_reports_shape_without_titles_previews_or_paths() {
        let query = SessionQuery {
            cursor: Some(String::from("cursor-secret")),
            limit: Some(10),
            workspace_path: Some(std::path::PathBuf::from("/home/person-secret/work")),
        };
        let page = SessionPage {
            sessions: vec![NativeSession {
                native_session_id: String::from("native-id-secret"),
                title: Some(String::from("title-secret")),
                preview: Some(String::from("preview-secret")),
                workspace_path: Some(String::from("/home/person-secret/work")),
                updated_at: None,
            }],
            next_cursor: Some(String::from("next-cursor-secret")),
            truncated: true,
        };

        let rendered = format!("{query:?} {page:?} {:?}", page.sessions[0]);
        for secret in [
            "cursor-secret",
            "next-cursor-secret",
            "person-secret",
            "native-id-secret",
            "title-secret",
            "preview-secret",
        ] {
            assert!(
                !rendered.contains(secret),
                "expected no listing payload in diagnostics, received {rendered}"
            );
        }
        for metadata in ["has_cursor: true", "session_count: 1", "truncated: true"] {
            assert!(
                rendered.contains(metadata),
                "expected {metadata} to survive, received {rendered}"
            );
        }
    }

    #[test]
    fn mcp_debug_keeps_values_and_positional_arguments_out_of_diagnostics() {
        let stdio = McpServer {
            name: String::from("docs"),
            transport: McpTransport::Stdio {
                command: String::from("docs-mcp"),
                args: vec![String::from("--api-key"), String::from("sk-live-args")],
                env: [(String::from("API_KEY"), String::from("sk-live-env"))]
                    .into_iter()
                    .collect(),
            },
        };
        let http = McpServer {
            name: String::from("search"),
            transport: McpTransport::Http {
                url: String::from("https://user:sk-live-url@search.example/mcp?key=query-secret"),
                headers: [(
                    String::from("Authorization"),
                    String::from("Bearer sk-live-header"),
                )]
                .into_iter()
                .collect(),
            },
        };
        let printed = format!("{stdio:?}\n{http:?}");

        for secret in [
            "sk-live-env",
            "sk-live-header",
            "sk-live-url",
            "sk-live-args",
            "query-secret",
        ] {
            assert!(
                !printed.contains(secret),
                "expected {secret:?} to be redacted, received {printed}"
            );
        }
        for kept in [
            "custom executable",
            "API_KEY",
            "Authorization",
            "argument_count: 2",
        ] {
            assert!(
                printed.contains(kept),
                "expected {kept:?} to survive redaction, received {printed}"
            );
        }
    }

    #[test]
    fn nested_request_debug_omits_prompts_ids_and_host_configuration_values() {
        let request = OpenSession::new("session-id-secret")
            .with_configuration(
                ConfigurationPatch::new()
                    .model(ConfigurationChange::Set(String::from("model-secret")))
                    .effort(ConfigurationChange::Set(String::from("effort-secret"))),
            )
            .resuming("resume-id-secret", ResumeMode::Fallback)
            .with_mcp_servers(vec![McpServer::stdio("server-name-secret", "docs-mcp")]);
        let turn = TurnRequest::new("turn-id-secret", "prompt-secret").with_attachments(vec![
            Attachment {
                id: String::from("attachment-id-secret"),
                name: String::from("attachment-name-secret"),
                mime_type: String::from("text/plain"),
                kind: AttachmentKind::Text,
                bytes: b"attachment-bytes-secret".to_vec(),
            },
        ]);

        for printed in [format!("{request:?}"), format!("{turn:?}")] {
            for secret in [
                "session-id-secret",
                "model-secret",
                "effort-secret",
                "resume-id-secret",
                "server-name-secret",
                "turn-id-secret",
                "prompt-secret",
                "attachment-id-secret",
                "attachment-name-secret",
                "attachment-bytes-secret",
            ] {
                assert!(
                    !printed.contains(secret),
                    "expected no request payload in debug output, received {printed}"
                );
            }
        }
    }

    #[test]
    fn closing_carries_its_reason_into_the_turn_it_cancels() {
        let cases = [
            (CloseReason::Requested, CancelReason::Requested),
            (CloseReason::ConsentRevoked, CancelReason::ConsentRevoked),
            (CloseReason::Shutdown, CancelReason::Shutdown),
        ];
        for (close, expected) in cases {
            assert_eq!(
                CancelReason::from(close),
                expected,
                "expected {expected:?}, received a different cancel reason for {close:?}"
            );
        }
    }

    #[test]
    fn reasons_print_as_words_a_host_can_map() {
        assert_eq!(CancelReason::ConsentRevoked.to_string(), "consent revoked");
        assert_eq!(CloseReason::Shutdown.to_string(), "shutdown");
    }

    #[test]
    fn opening_a_session_asks_for_nothing_a_host_did_not_choose() {
        let request = OpenSession::new("chat-42");
        assert!(request.configuration.is_empty());
        assert_eq!(request.resume, None);
        assert_eq!(request.transport, None);
        assert!(request.discovery.is_none());

        let resuming = OpenSession::new("chat-42").resuming("thread_9", ResumeMode::Fallback);
        assert_eq!(
            resuming.resume.map(|resume| resume.native_session_id),
            Some(String::from("thread_9"))
        );
    }

    /// An open request carries a patch, so a host can clear an override the vendor's own
    /// configuration holds rather than only add one on top of it.
    #[test]
    fn opening_under_an_explicit_patch_can_set_and_reset() {
        let request = OpenSession::new("chat-1").with_configuration(
            ConfigurationPatch::new()
                .level(ConfigurationChange::Set(PermissionLevel::ReadOnly))
                .model(ConfigurationChange::Reset),
        );

        assert_eq!(
            request.configuration.level.set_value(),
            Some(&PermissionLevel::ReadOnly)
        );
        assert!(request.configuration.model.is_reset());
        assert!(request.configuration.asks_for_a_reset());
    }

    #[test]
    fn a_requested_transport_is_recorded_on_the_request_that_asked_for_it() {
        let request = OpenSession::new("chat-1").over_transport(TransportKind::WebSocket);
        assert_eq!(request.transport, Some(TransportKind::WebSocket));
    }

    /// The doc comment used to call the turn id an idempotency key. Nothing in this library or in
    /// any vendor it drives deduplicates on it, so a host reading that would have built recovery
    /// on a promise nobody made.
    #[test]
    fn a_turn_carries_a_logical_id_and_an_attempt_that_are_not_the_same_thing() {
        let turn = TurnRequest::new("turn-7", "ship it");
        assert_eq!(turn.turn_id.as_str(), "turn-7");
        assert_eq!(turn.attempt, AttemptId::default());
        assert_eq!(turn.configuration, None);

        let retry = turn.clone().as_attempt(AttemptId::new(2));
        assert_eq!(
            retry.turn_id, turn.turn_id,
            "expected a retry to stay the same logical turn"
        );
        assert_ne!(
            retry.attempt, turn.attempt,
            "expected a retry to be a different attempt"
        );
    }

    fn row(native_session_id: &str) -> NativeSession {
        NativeSession {
            native_session_id: native_session_id.to_owned(),
            title: None,
            preview: None,
            workspace_path: None,
            updated_at: None,
        }
    }

    #[test]
    fn a_listing_drops_a_row_whose_id_would_have_to_be_truncated() {
        let page = SessionPage {
            sessions: vec![row(&"i".repeat(129)), row("thread_1"), row("  ")],
            next_cursor: Some(String::from("next")),
            truncated: false,
        }
        .normalized();

        assert_eq!(page.sessions.len(), 1, "received {:?}", page.sessions);
        assert_eq!(page.sessions[0].native_session_id, "thread_1");
        assert_eq!(page.next_cursor, Some(String::from("next")));
    }

    #[test]
    fn a_listing_drops_a_row_whose_path_cannot_be_carried_whole() {
        let page = SessionPage {
            sessions: vec![
                NativeSession {
                    workspace_path: Some("p".repeat(4_097)),
                    ..row("thread_1")
                },
                NativeSession {
                    workspace_path: Some(String::from("/home/ada/mango")),
                    ..row("thread_2")
                },
            ],
            next_cursor: None,
            truncated: false,
        }
        .normalized();

        assert_eq!(page.sessions.len(), 1, "received {:?}", page.sessions);
        assert_eq!(page.sessions[0].native_session_id, "thread_2");
    }

    #[test]
    fn a_listing_bounds_titles_and_drops_the_ones_that_are_left_empty() {
        let page = SessionPage {
            sessions: vec![NativeSession {
                title: Some("t".repeat(300)),
                preview: Some(String::from("\u{202e}")),
                ..row("thread_1")
            }],
            next_cursor: None,
            truncated: false,
        }
        .normalized();

        assert_eq!(
            page.sessions[0]
                .title
                .as_ref()
                .map(|title| title.chars().count()),
            Some(256)
        );
        assert_eq!(page.sessions[0].preview, None);
    }

    /// The cap and the cursor describe different points in the same page: the cap stops at row 50,
    /// the vendor's cursor resumes after row 80. Paging on it steps over rows 51-80, so a page
    /// that was cut has to say it was — nothing else in the page can be read to mean it.
    #[test]
    fn a_listing_cut_to_one_page_says_so_because_the_cursor_will_skip_the_rest() {
        let page = SessionPage {
            sessions: (0..80)
                .map(|index| row(&format!("thread_{index}")))
                .collect(),
            next_cursor: Some(String::from("after-thread-79")),
            truncated: false,
        }
        .normalized();

        assert_eq!(page.sessions.len(), 50);
        assert!(
            page.truncated,
            "expected a page of 80 cut to 50 to report the cut, received {:?}",
            page.truncated
        );
        // The cursor is the vendor's and is carried as it was: dropping it would lose rows 81 and
        // beyond on top of the ones the cap already took.
        assert_eq!(page.next_cursor, Some(String::from("after-thread-79")));
    }

    /// The flag means "the cap fired", not "something was dropped". A row refused as unusable is a
    /// row nothing could have shown, and reporting it here would make the flag unreadable on the
    /// one page a host can actually act on.
    #[test]
    fn a_page_inside_the_cap_is_not_truncated_by_a_row_it_refused() {
        let page = SessionPage {
            sessions: vec![row(&"i".repeat(129)), row("thread_1")],
            next_cursor: None,
            truncated: false,
        }
        .normalized();

        assert_eq!(page.sessions.len(), 1);
        assert!(!page.truncated, "received {:?}", page.truncated);
    }

    /// A page of exactly the cap is whole, not cut. The boundary is worth pinning: the check asks
    /// whether a row survived *past* the cap, and an off-by-one here would mark every full page.
    #[test]
    fn a_page_of_exactly_the_cap_is_not_reported_as_cut() {
        let page = SessionPage {
            sessions: (0..super::SESSION_PAGE_LIMIT)
                .map(|index| row(&format!("thread_{index}")))
                .collect(),
            next_cursor: None,
            truncated: false,
        }
        .normalized();

        assert_eq!(page.sessions.len(), super::SESSION_PAGE_LIMIT);
        assert!(!page.truncated, "received {:?}", page.truncated);
    }

    /// The cap has to be something the vendor is *asked* for. Applied only to the answer, it cuts
    /// a page whose tail no cursor points at; applied to the query, a well-behaved vendor never
    /// sends the overrun in the first place.
    #[tokio::test]
    async fn a_listing_query_reaches_the_harness_already_bounded() {
        use super::{SESSION_PAGE_LIMIT, SessionQuery};

        let session = RecordingListing::default();
        session
            .list_sessions(SessionQuery {
                limit: Some(500),
                ..SessionQuery::default()
            })
            .await
            .expect("expected a page");
        assert_eq!(session.last_limit().await, Some(SESSION_PAGE_LIMIT));

        // Absent means "as many as you like", which is the request that produces the unfinishable
        // page, so it is spelled out rather than left to the vendor's default.
        session
            .list_sessions(SessionQuery::default())
            .await
            .expect("expected a page");
        assert_eq!(session.last_limit().await, Some(SESSION_PAGE_LIMIT));

        // A host that asked for less still gets less.
        session
            .list_sessions(SessionQuery {
                limit: Some(5),
                ..SessionQuery::default()
            })
            .await
            .expect("expected a page");
        assert_eq!(session.last_limit().await, Some(5));
    }

    fn opening_state(native_session_id: &str) -> SessionState {
        SessionState::new(
            std::sync::Arc::new(crate::host::SystemClock),
            SessionSnapshot::opening(
                SessionIds {
                    session_id: crate::event::SessionId::new("chat-1"),
                    native_session_id: String::from(native_session_id),
                },
                HarnessIdentity::claude(),
                TransportSelection::new(None, TransportKind::Stdio),
                SystemTime::UNIX_EPOCH,
            )
            .with_capabilities(SessionCapabilities::none()),
        )
    }

    /// A session that answers listings with nothing and remembers what it was asked for.
    struct RecordingListing {
        state: SessionState,
        last_query: tokio::sync::Mutex<Option<super::SessionQuery>>,
    }

    impl Default for RecordingListing {
        fn default() -> Self {
            Self {
                state: opening_state("native-1"),
                last_query: tokio::sync::Mutex::new(None),
            }
        }
    }

    impl RecordingListing {
        async fn last_limit(&self) -> Option<usize> {
            self.last_query
                .lock()
                .await
                .as_ref()
                .and_then(|query| query.limit)
        }
    }

    #[async_trait::async_trait]
    impl super::Session for RecordingListing {
        fn state(&self) -> &SessionState {
            &self.state
        }

        async fn start_turn(&self, _request: TurnRequest) -> crate::Result<crate::TurnStream> {
            Err(crate::Error::Closed { subject: "session" })
        }

        async fn respond(
            &self,
            _response: crate::permission::PermissionResponse,
        ) -> crate::Result<()> {
            Err(crate::Error::Closed { subject: "session" })
        }

        async fn cancel(&self, _reason: CancelReason) -> crate::Result<()> {
            Ok(())
        }

        async fn close(&self, _reason: CloseReason) -> crate::Result<()> {
            Ok(())
        }

        async fn list_native_sessions(
            &self,
            query: super::SessionQuery,
        ) -> crate::Result<SessionPage> {
            *self.last_query.lock().await = Some(query);
            Ok(SessionPage::default())
        }
    }

    /// The handle a host persists has to be the one a resume can actually use, and a vendor may
    /// mint its own after the open has already answered — the shape Claude Code has, where
    /// `--session-id` proposes one and the run's own announcement is free to report another.
    #[test]
    fn reports_the_handle_the_vendor_chose_over_the_one_opening_minted() {
        let session = RecordingListing::default();
        session.state().set_native_session_id("native-2");

        let ids = Session::ids(&session);
        assert_eq!(
            ids.native_session_id, "native-2",
            "expected the vendor's own handle, received {:?}",
            ids.native_session_id
        );
        assert_eq!(
            ids.session_id.as_str(),
            "chat-1",
            "expected the host's own id to be untouched"
        );
    }

    /// A vendor that keeps the handle it was given needs no override at all.
    #[test]
    fn a_vendor_that_keeps_its_handle_reports_the_one_opening_minted() {
        let session = RecordingListing::default();
        assert_eq!(Session::ids(&session).native_session_id, "native-1");
        assert_eq!(
            Session::snapshot(&session).revision,
            crate::state::SessionRevision::INITIAL
        );
    }

    /// A per-turn configuration needs a capability the session advertised; an image attachment
    /// needs another. Both refuse by name rather than being dropped on the way to the vendor.
    #[test]
    fn per_turn_input_is_refused_by_name_when_the_session_never_advertised_it() {
        use super::{Attachment, AttachmentKind};
        use crate::harness::Capability;

        let session = RecordingListing::default();

        let error = Session::validate_turn_request(
            &session,
            &TurnRequest::new("turn-1", "hi")
                .with_configuration(ConfigurationPatch::new().model(ConfigurationChange::Reset)),
        )
        .expect_err("expected a refusal");
        assert!(
            matches!(
                error,
                crate::Error::NotSupported {
                    capability: Capability::Configuration
                }
            ),
            "received {error:?}"
        );

        let error = Session::validate_turn_request(
            &session,
            &TurnRequest::new("turn-1", "hi").with_attachments(vec![Attachment {
                id: String::from("a1"),
                name: String::from("shot.png"),
                mime_type: String::from("image/png"),
                kind: AttachmentKind::Image,
                bytes: Vec::new(),
            }]),
        )
        .expect_err("expected a refusal");
        assert!(
            matches!(
                error,
                crate::Error::NotSupported {
                    capability: Capability::Images
                }
            ),
            "received {error:?}"
        );
    }

    /// Answering a question is not authorising a tool, and mid-session configuration is a third
    /// thing again. Each refuses under its own capability rather than one standing in for another.
    #[tokio::test]
    async fn the_interaction_surfaces_refuse_under_their_own_capabilities() {
        use crate::harness::Capability;
        use crate::interaction::{InteractionId, QuestionResponse};

        let session = RecordingListing::default();

        let error = session
            .answer(QuestionResponse::new(
                InteractionId::new("ask-1"),
                Vec::new(),
            ))
            .await
            .expect_err("expected a refusal");
        assert!(
            matches!(
                error,
                crate::Error::NotSupported {
                    capability: Capability::Questions
                }
            ),
            "received {error:?}"
        );

        let error = session
            .configure(ConfigurationPatch::new())
            .await
            .expect_err("expected a refusal");
        assert!(
            matches!(
                error,
                crate::Error::NotSupported {
                    capability: Capability::SessionConfiguration
                }
            ),
            "received {error:?}"
        );
    }

    #[test]
    fn review_targets_preserve_host_values_through_serialization() {
        for target in [
            super::ReviewTarget::UncommittedChanges,
            super::ReviewTarget::BaseBranch {
                branch: String::from("main"),
            },
            super::ReviewTarget::Commit {
                sha: String::from("abc123"),
                title: Some(String::from("Fix parser")),
            },
            super::ReviewTarget::Custom {
                instructions: String::from("Review error handling"),
            },
        ] {
            let json = serde_json::to_value(&target).expect("serializable review target");
            assert_eq!(
                serde_json::from_value::<super::ReviewTarget>(json).expect("review target"),
                target
            );
        }
    }
}
