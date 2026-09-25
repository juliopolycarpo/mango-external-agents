//! What this harness sends, and what the app-server answers with.
//!
//! Every field the server writes but this harness never reads is left out: `serde` drops unknown
//! members on the way in, so a build newer than the pin deserialises fine. Fields this harness
//! *sends* are the opposite — an unknown member would be refused by a strict server — so nothing
//! here is written speculatively, and an absent option is an absent member rather than a `null`.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Who the host says it is, as the app-server's `clientInfo`.
///
/// `name` is always the host's own, from [`HostContext::client_info`]. The app-server README says
/// this identifies the client to OpenAI's compliance logging, so writing anything but the host's
/// real name would be a misattribution rather than a nicety.
///
/// [`HostContext::client_info`]: mango_external_agents::HostContext::client_info
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    /// The host product's name.
    pub name: String,
    /// A longer name for a person, when the host has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The host product's version.
    pub version: String,
}

/// The one call that must precede every other on a connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// Who the host says it is.
    pub client_info: ClientInfo,
    /// What the client wants suppressed or turned on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<InitializeCapabilities>,
}

/// The per-connection capabilities the handshake declares.
///
/// Only the opt-out list is used. The experimental API is deliberately not requested: this harness
/// drives the documented surface, and a field marked experimental upstream is one that can be
/// withdrawn between two patch releases.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeCapabilities {
    /// Notification methods this connection does not want, matched exactly.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub opt_out_notification_methods: Vec<String>,
}

/// What the app-server answers the handshake with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    /// The user agent the server will present upstream.
    #[serde(default)]
    pub user_agent: String,
    /// The operating system the server is running on, as it names it.
    #[serde(default)]
    pub platform_os: String,
}

/// How much the agent has to ask before acting, as the app-server spells it.
///
/// The `granular` variant upstream carries a struct of flags. It is never sent by this harness —
/// three levels map onto the three plain values — so it is modelled as the opaque variant that
/// keeps whatever arrived rather than as a shape that would have to track upstream's flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AskForApproval {
    /// Ask before anything outside a trusted set.
    Untrusted,
    /// Ask when the agent asks to escalate.
    OnRequest,
    /// Never ask.
    Never,
}

/// What the agent may touch, as the app-server spells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    /// Reads only.
    ReadOnly,
    /// Writes inside the workspace.
    WorkspaceWrite,
    /// Everything.
    DangerFullAccess,
}

/// The complete sandbox policy sent alongside a turn's approval policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SandboxPolicy {
    /// Read-only filesystem access with an explicit network policy.
    ReadOnly {
        /// Whether network access is allowed.
        network_access: bool,
    },
    /// Writes confined to the host's authorised workspace.
    WorkspaceWrite {
        /// Directories the host authorised for writes.
        writable_roots: Vec<String>,
        /// Whether network access is allowed without approval.
        network_access: bool,
        /// Exclude the process's temporary directory from writable roots.
        exclude_tmpdir_env_var: bool,
        /// Exclude the system temporary directory from writable roots.
        exclude_slash_tmp: bool,
    },
    /// No vendor sandbox, only when the host explicitly chose full access.
    DangerFullAccess,
}

/// Who answers the agent's approval prompts, as the app-server spells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalsReviewer {
    /// A person, through this harness's own approval events.
    User,
    /// The vendor's own reviewing subagent.
    AutoReview,
}

/// Opening a conversation.
#[derive(Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartParams {
    /// The directory the host authorised.
    pub cwd: String,
    /// The vendor's own model id, when the host chose one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// How much the agent has to ask.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<AskForApproval>,
    /// What it may touch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxMode>,
    /// Who answers its prompts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approvals_reviewer: Option<ApprovalsReviewer>,
    /// Request-scoped app-server config; never written to the user's config file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<BTreeMap<String, serde_json::Value>>,
}

impl fmt::Debug for ThreadStartParams {
    /// Shows request metadata without exposing host-supplied MCP configuration values.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ThreadStartParams")
            .field("cwd_bytes", &self.cwd.len())
            .field("has_model", &self.model.is_some())
            .field("approval_policy", &self.approval_policy)
            .field("sandbox", &self.sandbox)
            .field("approvals_reviewer", &self.approvals_reviewer)
            .field("has_config", &self.config.is_some())
            .finish()
    }
}

/// Continuing one.
#[derive(Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResumeParams {
    /// The vendor's own handle for the conversation.
    pub thread_id: String,
    /// The directory the host authorised.
    pub cwd: String,
    /// The vendor's own model id, when the host chose one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// How much the agent has to ask.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<AskForApproval>,
    /// What it may touch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxMode>,
    /// Who answers its prompts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approvals_reviewer: Option<ApprovalsReviewer>,
    /// Request-scoped app-server config; never written to the user's config file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<BTreeMap<String, serde_json::Value>>,
    /// Metadata only: the transcript is the vendor's, and this harness never replays one.
    pub exclude_turns: bool,
}

impl fmt::Debug for ThreadResumeParams {
    /// Shows request metadata without exposing host-supplied MCP configuration values.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ThreadResumeParams")
            .field("thread_id_bytes", &self.thread_id.len())
            .field("cwd_bytes", &self.cwd.len())
            .field("has_model", &self.model.is_some())
            .field("approval_policy", &self.approval_policy)
            .field("sandbox", &self.sandbox)
            .field("approvals_reviewer", &self.approvals_reviewer)
            .field("has_config", &self.config.is_some())
            .field("exclude_turns", &self.exclude_turns)
            .finish()
    }
}

/// Reads native metadata without loading transcript turns into the response.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadReadParams {
    /// The native conversation the host intends to resume.
    pub thread_id: String,
    /// Always false: workspace authorization needs only thread metadata.
    pub include_turns: bool,
}

impl fmt::Debug for ThreadReadParams {
    /// Retains request shape without writing vendor thread ids into diagnostics.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ThreadReadParams")
            .field("thread_id_bytes", &self.thread_id.len())
            .field("include_turns", &self.include_turns)
            .finish()
    }
}

/// The app-server's metadata-only thread answer.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadReadResponse {
    /// The thread identity and original workspace.
    pub thread: ThreadSummary,
}

impl fmt::Debug for ThreadReadResponse {
    /// Retains the response shape without writing vendor thread metadata into diagnostics.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ThreadReadResponse")
            .field("thread", &self.thread)
            .finish()
    }
}

/// One conversation, as much of it as this harness reads.
///
/// The upstream `Thread` carries thirty members; the five kept here are the ones a session id, a
/// picker row and a working-directory filter are built from. Everything else — the persisted
/// turns above all — stays where the vendor wrote it.
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSummary {
    /// The vendor's own handle.
    pub id: String,
    /// Usually the conversation's first user message.
    #[serde(default)]
    pub preview: String,
    /// A title, when somebody set one.
    #[serde(default)]
    pub name: Option<String>,
    /// The directory the conversation was opened in.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Unix seconds when it last changed.
    #[serde(default)]
    pub updated_at: Option<i64>,
    /// Unix seconds when it was last used; `thread/list` sorts on it for `recency_at`.
    #[serde(default)]
    pub recency_at: Option<i64>,
}

impl ThreadSummary {
    /// When the thread was last used, as the picker sorts it: `recencyAt`, else `updatedAt`.
    #[must_use]
    pub fn last_used_at(&self) -> Option<i64> {
        self.recency_at.or(self.updated_at)
    }
}

impl fmt::Debug for ThreadSummary {
    /// Retains which thread metadata arrived without writing vendor or host values into diagnostics.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ThreadSummary")
            .field("id_bytes", &self.id.len())
            .field("has_preview", &!self.preview.is_empty())
            .field("has_name", &self.name.is_some())
            .field("has_cwd", &self.cwd.is_some())
            .field("has_updated_at", &self.updated_at.is_some())
            .field("has_recency_at", &self.recency_at.is_some())
            .finish()
    }
}

/// What opening or continuing a conversation answered with.
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartResponse {
    /// The conversation itself.
    pub thread: ThreadSummary,
    /// The model the server actually chose.
    #[serde(default)]
    pub model: Option<String>,
    /// The reasoning effort it actually chose.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// The approval policy it actually applied.
    #[serde(default)]
    pub approval_policy: Option<AskForApproval>,
    /// The reviewer it actually routed to.
    #[serde(default)]
    pub approvals_reviewer: Option<ApprovalsReviewer>,
}

impl fmt::Debug for ThreadStartResponse {
    /// Retains which fields the server returned without writing vendor values into diagnostics.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ThreadStartResponse")
            .field("thread", &self.thread)
            .field("has_model", &self.model.is_some())
            .field("has_reasoning_effort", &self.reasoning_effort.is_some())
            .field("approval_policy", &self.approval_policy)
            .field("approvals_reviewer", &self.approvals_reviewer)
            .finish()
    }
}

/// One piece of a turn's input.
///
/// Only the two shapes this harness produces. `text_elements` is snake_case on the wire while its
/// siblings are camelCase — that is upstream's spelling, and matching it is the point.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum UserInput {
    /// What the person typed.
    Text {
        /// The text itself.
        text: String,
        /// Spans a renderer would highlight. Always empty here: this harness sends plain text.
        ///
        /// Named against the grain on purpose: `rename_all_fields` puts every sibling in
        /// camelCase, and upstream spells this one member snake_case. `textElements` is a member
        /// the app-server never declared.
        #[serde(rename = "text_elements")]
        text_elements: Vec<serde_json::Value>,
    },
    /// An image, as a `data:` URL the host already has the bytes for.
    Image {
        /// The `data:` URL.
        url: String,
    },
}

impl UserInput {
    /// Plain text, with no spans.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            text_elements: Vec::new(),
        }
    }
}

/// Starting a turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams {
    /// Which conversation.
    pub thread_id: String,
    /// What to say.
    pub input: Vec<UserInput>,
    /// The vendor's own model id, when this turn overrides the session's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The vendor's own reasoning-effort id, when this turn overrides the session's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// How much the agent has to ask, when this turn overrides the session's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<AskForApproval>,
    /// What the turn may touch, paired with its approval policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_policy: Option<SandboxPolicy>,
    /// Who answers, when this turn overrides the session's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approvals_reviewer: Option<ApprovalsReviewer>,
}

/// How a turn ended, as the app-server spells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    /// It is still going.
    InProgress,
    /// It finished.
    Completed,
    /// Somebody stopped it.
    Interrupted,
    /// It failed.
    Failed,
    /// A spelling this build does not know.
    ///
    /// Every other unknown value in this crate costs one event. This one would cost the turn:
    /// `turn/completed` is the only frame that ends one, and a strict enum would fail the whole
    /// notification over a status the next release adds — leaving the host a stream that never
    /// terminates and a session that refuses every later turn as already running. An ending
    /// nobody can name is still an ending, and it is read as one.
    #[serde(other)]
    Unknown,
}

/// What the server said about a failed turn.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct TurnError {
    /// The message it wrote.
    #[serde(default)]
    pub message: String,
    /// More, when it had more to say.
    #[serde(default)]
    pub additional_details: Option<String>,
    /// The vendor's own classification: a string such as `usageLimitExceeded`, or a single-key
    /// object such as `{"httpConnectionFailed": {"httpStatusCode": 502}}`.
    #[serde(default)]
    pub codex_error_info: Option<serde_json::Value>,
}

impl TurnError {
    /// The vendor's classification as one label, when it gave one.
    ///
    /// A string member is its own label; an object member is labelled by its one key. Anything
    /// else reads as `other`, the vendor's own catch-all.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_agent_codex::protocol::requests::TurnError;
    ///
    /// let error: TurnError = serde_json::from_value(serde_json::json!({
    ///     "message": "limit", "codexErrorInfo": "usageLimitExceeded"
    /// })).unwrap();
    /// assert_eq!(error.vendor_code().as_deref(), Some("usageLimitExceeded"));
    /// ```
    #[must_use]
    pub fn vendor_code(&self) -> Option<String> {
        self.codex_error_info.as_ref().map(codex_error_code)
    }
}

/// One label for a `CodexErrorInfo` value, as [`TurnError::vendor_code`] describes.
#[must_use]
pub fn codex_error_code(info: &serde_json::Value) -> String {
    match info {
        serde_json::Value::String(code) if !code.is_empty() => code.clone(),
        serde_json::Value::Object(members) => members
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| String::from("other")),
        _ => String::from("other"),
    }
}

/// One turn, as much of it as this harness reads.
///
/// `items` is deliberately absent. The app-server repeats a completed turn's whole item list in
/// `turn/completed`, and re-reading it would emit every activity a second time — the turn already
/// streamed each one as `item/started` and `item/completed`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnHandle {
    /// The vendor's own handle for this turn.
    pub id: String,
    /// How it is going, or how it ended.
    #[serde(default)]
    pub status: Option<TurnStatus>,
    /// Why it failed, when it did.
    #[serde(default)]
    pub error: Option<TurnError>,
}

/// What starting a turn answered with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartResponse {
    /// The turn the server opened.
    pub turn: TurnHandle,
}

/// Adding to a turn that is already running.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnSteerParams {
    /// Which conversation.
    pub thread_id: String,
    /// What to add.
    pub input: Vec<UserInput>,
    /// The turn this steer is for.
    ///
    /// A precondition rather than a hint: the server refuses the call when the turn it names is
    /// not the one running, which is what keeps a steer from landing on the turn after the one
    /// the host meant.
    pub expected_turn_id: String,
}

/// What steering answered with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnSteerResponse {
    /// The turn the input landed on.
    #[serde(default)]
    pub turn_id: String,
}

/// Stopping the turn that is running.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnInterruptParams {
    /// Which conversation.
    pub thread_id: String,
    /// Which turn.
    pub turn_id: String,
}

/// What a vendor-native review is pointed at.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ReviewTarget {
    /// Staged, unstaged and untracked work, as the vendor defines it.
    UncommittedChanges,
    /// Changes relative to a base branch.
    BaseBranch {
        /// The branch or revision to compare against.
        branch: String,
    },
    /// One commit.
    Commit {
        /// The commit hash or revision.
        sha: String,
        /// Optional display title.
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// Host-supplied review instructions.
    Custom {
        /// What the reviewer should inspect.
        instructions: String,
    },
}

/// Starting the vendor's own review.
///
/// `delivery` is left absent, which the server reads as inline: a detached review runs on a thread
/// this session is not subscribed to, and its events would arrive under an id the reducer drops.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewStartParams {
    /// Which conversation.
    pub thread_id: String,
    /// What to review.
    pub target: ReviewTarget,
}

/// What starting a review answered with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewStartResponse {
    /// The turn the review runs as.
    pub turn: TurnHandle,
    /// The thread it runs on, which for an inline review is the session's own.
    #[serde(default)]
    pub review_thread_id: String,
}

/// How a thread listing is ordered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ThreadSortKey {
    /// When the thread was created, the server's default.
    CreatedAt,
    /// When it last changed.
    UpdatedAt,
    /// When it was last used, which is what a picker wants.
    RecencyAt,
}

/// Which way a listing runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum SortDirection {
    /// Newest first, the server's default.
    Desc,
    /// Oldest first.
    Asc,
}

/// Where a thread came from, as `thread/list` filters on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum ThreadSourceKind {
    /// The interactive CLI.
    Cli,
    /// The VS Code extension.
    Vscode,
    /// `codex exec`.
    Exec,
    /// An app-server client, such as a host built on this library.
    AppServer,
}

/// The sources a person started, and a host's picker lists.
///
/// The server's default is interactive sources only (`cli` and `vscode`), which leaves out threads
/// started by `codex exec` or by an app-server client. `vscode` is excluded on purpose, as the
/// TypeScript adapter did: an editor-owned thread has a live owner the host cannot see. The
/// subagent kinds are Codex's own machinery and never a conversation somebody chose.
pub const USER_THREAD_SOURCES: &[ThreadSourceKind] = &[
    ThreadSourceKind::Cli,
    ThreadSourceKind::Exec,
    ThreadSourceKind::AppServer,
];

/// Listing the conversations this machine already has.
#[derive(Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ThreadListParams {
    /// Where to continue from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// How many to return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// Only conversations opened in this directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// How to order them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_key: Option<ThreadSortKey>,
    /// Which way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_direction: Option<SortDirection>,
    /// Only threads from these sources.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_kinds: Option<Vec<ThreadSourceKind>>,
    /// Archived threads only when true; non-archived only when false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
}

impl ThreadListParams {
    /// A picker page: the user's own non-archived threads in `cwd`, most recently used first.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_agent_codex::protocol::requests::ThreadListParams;
    ///
    /// let params = ThreadListParams::picker(None, Some(20), "/workspace");
    /// let wire = serde_json::to_value(&params).unwrap();
    /// assert_eq!(wire["sortKey"], "recency_at");
    /// assert_eq!(wire["archived"], false);
    /// ```
    #[must_use]
    pub fn picker(cursor: Option<String>, limit: Option<usize>, cwd: &str) -> Self {
        Self {
            cursor,
            limit,
            cwd: Some(cwd.to_owned()),
            sort_key: Some(ThreadSortKey::RecencyAt),
            sort_direction: Some(SortDirection::Desc),
            source_kinds: Some(USER_THREAD_SOURCES.to_vec()),
            archived: Some(false),
        }
    }
}

impl fmt::Debug for ThreadListParams {
    /// Retains request shape without writing a vendor cursor or host workspace into diagnostics.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ThreadListParams")
            .field("has_cursor", &self.cursor.is_some())
            .field("limit", &self.limit)
            .field("has_cwd", &self.cwd.is_some())
            .field("sort_key", &self.sort_key)
            .field(
                "source_kind_count",
                &self.source_kinds.as_ref().map(Vec::len),
            )
            .field("archived", &self.archived)
            .finish()
    }
}

/// One page of them.
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadListResponse {
    /// The rows.
    #[serde(default)]
    pub data: Vec<ThreadSummary>,
    /// Where the next page starts.
    #[serde(default)]
    pub next_cursor: Option<String>,
}

impl fmt::Debug for ThreadListResponse {
    /// Retains page shape without writing rows or a vendor cursor into diagnostics.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ThreadListResponse")
            .field("thread_count", &self.data.len())
            .field("has_next_cursor", &self.next_cursor.is_some())
            .finish()
    }
}

/// Asking which models this build accepts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ModelListParams {
    /// Where to continue from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// How many to return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

impl ModelListParams {
    /// The page that starts at `cursor`, or the first page for `None`, at the server's page size.
    #[must_use]
    pub fn page(cursor: Option<String>) -> Self {
        Self {
            cursor,
            limit: None,
        }
    }
}

/// One reasoning choice a model offers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningEffortOption {
    /// The vendor's own id.
    #[serde(default)]
    pub reasoning_effort: String,
    /// What the vendor says it does.
    #[serde(default)]
    pub description: Option<String>,
}

/// One model, as the vendor advertises it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    /// The vendor's own id, sent back verbatim when this model is chosen.
    pub id: String,
    /// A name for a person.
    #[serde(default)]
    pub display_name: Option<String>,
    /// What the vendor says it is for.
    #[serde(default)]
    pub description: Option<String>,
    /// Whether the picker hides it.
    #[serde(default)]
    pub hidden: bool,
    /// Whether the vendor picks it when nobody chooses.
    #[serde(default)]
    pub is_default: bool,
    /// The reasoning choices it offers.
    #[serde(default)]
    pub supported_reasoning_efforts: Vec<ReasoningEffortOption>,
    /// Which of them applies when nobody chooses.
    #[serde(default)]
    pub default_reasoning_effort: Option<String>,
}

/// One page of models.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ModelListResponse {
    /// The rows.
    #[serde(default)]
    pub data: Vec<Model>,
    /// Where the next page starts, when there is one.
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// Asking which permission profiles this machine's configuration allows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PermissionProfileListParams {
    /// Where to continue from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// The project directory, so its configuration layers apply.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

impl PermissionProfileListParams {
    /// One page of profiles for the project at `cwd`, continuing from `cursor`.
    #[must_use]
    pub fn for_project(cwd: &str, cursor: Option<String>) -> Self {
        Self {
            cursor,
            cwd: Some(cwd.to_owned()),
        }
    }
}

/// One permission profile, and whether the effective requirements allow selecting it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PermissionProfileSummary {
    /// The profile's id, such as `:workspace`.
    pub id: String,
    /// Whether the effective requirements allow selecting it.
    #[serde(default)]
    pub allowed: bool,
}

/// One page of permission profiles.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PermissionProfileListResponse {
    /// The profiles.
    #[serde(default)]
    pub data: Vec<PermissionProfileSummary>,
    /// Where the next page starts, when there is one.
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// How an account is signed in, as the app-server reports it.
///
/// No credential is anywhere in this shape, and none is asked for: `account/read` answers with the
/// kind of account and, for a ChatGPT sign-in, a plan name. The email upstream also returns is not
/// modelled — a harness that does not need it should not be the thing that carries it into a log.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Account {
    /// An API key the user configured with the vendor's own CLI.
    ApiKey,
    /// A ChatGPT subscription.
    Chatgpt {
        /// The plan the vendor named.
        #[serde(default)]
        plan_type: Option<String>,
    },
    /// An Amazon Bedrock account.
    AmazonBedrock,
}

/// What asking who is signed in answered with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountReadResponse {
    /// The account, when there is one.
    #[serde(default)]
    pub account: Option<Account>,
    /// Whether this build needs an OpenAI sign-in at all.
    ///
    /// False for a build pointed at a provider of its own, where being signed out of OpenAI says
    /// nothing about whether a turn would run.
    #[serde(default)]
    pub requires_openai_auth: bool,
}

/// What asking for the account's quota answered with.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct RateLimitsReadResponse {
    /// The snapshot, when the server had one.
    #[serde(default)]
    pub rate_limits: Option<super::notifications::RateLimitSnapshot>,
    /// Earned rate-limit resets, when the service provides them. Only a full read carries these.
    #[serde(default)]
    pub rate_limit_reset_credits: Option<super::notifications::RateLimitResetCreditsSummary>,
}

/// Params for a call that takes none, as a strict server will accept it.
///
/// `{}` rather than an omitted member: the app-server declares object-shaped params for these
/// calls, and a dialect that validates would refuse the absence.
pub fn empty_params() -> BTreeMap<String, serde_json::Value> {
    BTreeMap::new()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        Account, AccountReadResponse, AskForApproval, ClientInfo, InitializeParams, SandboxMode,
        ThreadListParams, ThreadListResponse, ThreadReadParams, ThreadReadResponse,
        ThreadResumeParams, ThreadStartParams, ThreadStartResponse, ThreadSummary, TurnStartParams,
        TurnStatus, UserInput,
    };

    /// The handshake writes the host's own name and nothing it was not given.
    #[test]
    fn the_handshake_carries_the_hosts_name_and_omits_what_it_has_not_got() {
        let params = InitializeParams {
            client_info: ClientInfo {
                name: String::from("mangostudio"),
                title: None,
                version: String::from("1.4.0"),
            },
            capabilities: None,
        };
        let frame = serde_json::to_value(&params).expect("expected a frame");

        assert_eq!(frame["clientInfo"]["name"], "mangostudio");
        assert!(
            frame["clientInfo"].get("title").is_none(),
            "expected an absent title rather than a null, received {frame}"
        );
        assert!(
            frame.get("capabilities").is_none(),
            "expected absent capabilities rather than a null, received {frame}"
        );
    }

    /// A strict server refuses a member it did not declare, so an option nobody chose is absent.
    #[test]
    fn a_thread_start_writes_only_what_the_host_chose() {
        let params = ThreadStartParams {
            cwd: String::from("/workspace"),
            approval_policy: Some(AskForApproval::OnRequest),
            sandbox: Some(SandboxMode::ReadOnly),
            ..ThreadStartParams::default()
        };
        let frame = serde_json::to_value(&params).expect("expected a frame");

        assert_eq!(frame["cwd"], "/workspace");
        assert_eq!(frame["approvalPolicy"], "on-request");
        assert_eq!(frame["sandbox"], "read-only");
        assert!(
            frame.get("model").is_none(),
            "expected no model member, received {frame}"
        );
        assert!(
            frame.get("approvalsReviewer").is_none(),
            "expected no reviewer member, received {frame}"
        );
    }

    /// Per-thread MCP configuration can hold credentials, so diagnostics retain only its presence.
    #[test]
    fn thread_parameters_debug_redacts_mcp_values() {
        let config = BTreeMap::from([(
            String::from("mcp_servers"),
            serde_json::json!({
                "docs": {"env": {"DOCS_TOKEN": "stdio-env-secret"}},
                "remote": {"http_headers": {"Authorization": "http-header-secret"}},
            }),
        )]);
        let start = ThreadStartParams {
            cwd: String::from("/workspace"),
            config: Some(config.clone()),
            ..ThreadStartParams::default()
        };
        let resume = ThreadResumeParams {
            thread_id: String::from("thread-1"),
            cwd: String::from("/workspace"),
            config: Some(config),
            ..ThreadResumeParams::default()
        };

        for rendered in [format!("{start:?}"), format!("{resume:?}")] {
            for secret in ["stdio-env-secret", "http-header-secret"] {
                assert!(
                    !rendered.contains(secret),
                    "expected {secret:?} to stay out of diagnostics, received {rendered}"
                );
            }
        }
    }

    #[test]
    fn thread_metadata_and_picker_debug_redact_vendor_and_workspace_values() {
        let thread = ThreadSummary {
            id: String::from("vendor-thread-secret"),
            preview: String::from("preview-secret"),
            name: Some(String::from("name-secret")),
            cwd: Some(String::from("/host-workspace-secret")),
            updated_at: Some(1_725_000_000),
            recency_at: None,
        };
        let read = ThreadReadParams {
            thread_id: String::from("vendor-thread-secret"),
            include_turns: false,
        };
        let read_response = ThreadReadResponse {
            thread: thread.clone(),
        };
        let start_response = ThreadStartResponse {
            thread: thread.clone(),
            model: Some(String::from("model-secret")),
            reasoning_effort: Some(String::from("effort-secret")),
            ..ThreadStartResponse::default()
        };
        let list = ThreadListParams::picker(
            Some(String::from("vendor-cursor-secret")),
            Some(10),
            "/host-workspace-secret",
        );
        let list_response = ThreadListResponse {
            data: vec![thread],
            next_cursor: Some(String::from("vendor-cursor-secret")),
        };
        let start = ThreadStartParams {
            cwd: String::from("/host-workspace-secret"),
            model: Some(String::from("model-secret")),
            ..ThreadStartParams::default()
        };
        let resume = ThreadResumeParams {
            thread_id: String::from("vendor-thread-secret"),
            cwd: String::from("/host-workspace-secret"),
            model: Some(String::from("model-secret")),
            ..ThreadResumeParams::default()
        };

        for rendered in [
            format!("{start:?}"),
            format!("{resume:?}"),
            format!("{read:?}"),
            format!("{read_response:?}"),
            format!("{start_response:?}"),
            format!("{list:?}"),
            format!("{list_response:?}"),
        ] {
            for secret in [
                "vendor-thread-secret",
                "preview-secret",
                "name-secret",
                "/host-workspace-secret",
                "model-secret",
                "effort-secret",
                "vendor-cursor-secret",
            ] {
                assert!(
                    !rendered.contains(secret),
                    "expected {secret:?} to stay out of diagnostics, received {rendered}"
                );
            }
        }
    }

    /// Upstream spells this one member snake_case among camelCase siblings. Matching its spelling
    /// is the whole job: `textElements` is a member the server never declared.
    #[test]
    fn text_input_keeps_the_vendors_own_spelling_of_its_spans() {
        let frame = serde_json::to_value(UserInput::text("ship it")).expect("expected a frame");
        assert_eq!(frame["type"], "text");
        assert_eq!(frame["text"], "ship it");
        assert!(
            frame.get("text_elements").is_some(),
            "expected the vendor's own snake_case spelling, received {frame}"
        );
    }

    #[test]
    fn a_turn_overrides_only_what_it_was_given() {
        let params = TurnStartParams {
            thread_id: String::from("thread-1"),
            input: vec![UserInput::text("hello")],
            model: None,
            effort: Some(String::from("high")),
            approval_policy: None,
            sandbox_policy: None,
            approvals_reviewer: None,
        };
        let frame = serde_json::to_value(&params).expect("expected a frame");

        assert_eq!(frame["threadId"], "thread-1");
        assert_eq!(frame["effort"], "high");
        assert!(frame.get("model").is_none(), "received {frame}");
        assert!(frame.get("approvalPolicy").is_none(), "received {frame}");
    }

    #[test]
    fn a_turn_status_reads_the_vendors_own_spelling() {
        let status: TurnStatus = serde_json::from_str("\"inProgress\"").expect("expected a status");
        assert_eq!(status, TurnStatus::InProgress);
        let status: TurnStatus =
            serde_json::from_str("\"interrupted\"").expect("expected a status");
        assert_eq!(status, TurnStatus::Interrupted);
    }

    /// The real `account/read` answer carries an email this harness deliberately does not model.
    /// Deserialising must drop it rather than fail, and nothing about it may survive into a value
    /// the harness holds.
    #[test]
    fn reading_an_account_keeps_the_plan_and_drops_the_address() {
        let raw = r#"{"account":{"type":"chatgpt","email":"a@example.com","planType":"plus"},
                      "requiresOpenaiAuth":true}"#;
        let response: AccountReadResponse =
            serde_json::from_str(raw).expect("expected an account response");

        assert_eq!(
            response.account,
            Some(Account::Chatgpt {
                plan_type: Some(String::from("plus"))
            })
        );
        assert!(response.requires_openai_auth);
        let rendered = format!("{response:?}");
        assert!(
            !rendered.contains("example.com"),
            "expected no address anywhere in the value, received {rendered}"
        );
    }

    #[test]
    fn an_api_key_account_and_a_bedrock_account_both_deserialise() {
        let api_key: AccountReadResponse =
            serde_json::from_str(r#"{"account":{"type":"apiKey"},"requiresOpenaiAuth":true}"#)
                .expect("expected an account response");
        assert_eq!(api_key.account, Some(Account::ApiKey));

        let bedrock: AccountReadResponse = serde_json::from_str(
            r#"{"account":{"type":"amazonBedrock","usesCodexManagedCredentials":false},
                "requiresOpenaiAuth":false}"#,
        )
        .expect("expected an account response");
        assert_eq!(bedrock.account, Some(Account::AmazonBedrock));
    }
}
