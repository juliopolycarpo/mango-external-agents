//! What this harness sends, and what the app-server answers with.
//!
//! Every field the server writes but this harness never reads is left out: `serde` drops unknown
//! members on the way in, so a build newer than the pin deserialises fine. Fields this harness
//! *sends* are the opposite — an unknown member would be refused by a strict server — so nothing
//! here is written speculatively, and an absent option is an absent member rather than a `null`.

use std::collections::BTreeMap;

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
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
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

/// Continuing one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
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

/// One conversation, as much of it as this harness reads.
///
/// The upstream `Thread` carries thirty members; the five kept here are the ones a session id, a
/// picker row and a working-directory filter are built from. Everything else — the persisted
/// turns above all — stays where the vendor wrote it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
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
}

/// What opening or continuing a conversation answered with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
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
pub struct TurnError {
    /// The message it wrote.
    #[serde(default)]
    pub message: String,
    /// More, when it had more to say.
    #[serde(default)]
    pub additional_details: Option<String>,
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

/// Listing the conversations this machine already has.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
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
}

/// One page of them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadListResponse {
    /// The rows.
    #[serde(default)]
    pub data: Vec<ThreadSummary>,
    /// Where the next page starts.
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// Asking which models this build accepts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelListParams {
    /// How many to return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
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
pub struct ModelListResponse {
    /// The rows.
    #[serde(default)]
    pub data: Vec<Model>,
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
pub struct RateLimitsReadResponse {
    /// The snapshot, when the server had one.
    #[serde(default)]
    pub rate_limits: Option<super::notifications::RateLimitSnapshot>,
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
    use super::{
        Account, AccountReadResponse, AskForApproval, ClientInfo, InitializeParams, SandboxMode,
        ThreadStartParams, TurnStartParams, TurnStatus, UserInput,
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
