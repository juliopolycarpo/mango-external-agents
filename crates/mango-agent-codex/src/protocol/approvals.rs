//! What the app-server asks the client, and the answers it will take.
//!
//! This is the direction that makes a harness semantic rather than a codec: the server stops and
//! waits. Five of its questions are driven — two ordinary approvals, a permissions grant, a
//! question round and an MCP elicitation's native decline; the rest are refused with a JSON-RPC
//! error, and [`ServerRequest::refusal`] says why for each — including `item/tool/call`, where the
//! refusal is the library's central invariant rather than a gap.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The requests the app-server initiates, in the families this harness recognises.
pub mod method {
    /// May the agent run this command?
    pub const COMMAND_EXECUTION_APPROVAL: &str = "item/commandExecution/requestApproval";
    /// May the agent write these files?
    pub const FILE_CHANGE_APPROVAL: &str = "item/fileChange/requestApproval";
    /// Would the client run this tool on the agent's behalf?
    pub const TOOL_CALL: &str = "item/tool/call";
    /// Would the client ask the user something on the agent's behalf?
    pub const TOOL_REQUEST_USER_INPUT: &str = "item/tool/requestUserInput";
    /// An MCP server wants a form filled in.
    pub const MCP_ELICITATION: &str = "mcpServer/elicitation/request";
    /// Would the client grant this permission profile?
    pub const PERMISSIONS_APPROVAL: &str = "item/permissions/requestApproval";
    /// Would the client hand over a refreshed ChatGPT token?
    pub const CHATGPT_AUTH_TOKENS_REFRESH: &str = "account/chatgptAuthTokens/refresh";
    /// Would the client produce an attestation?
    pub const ATTESTATION_GENERATE: &str = "attestation/generate";
    /// The v1 spelling of the command approval, from before the item families.
    pub const LEGACY_EXEC_COMMAND_APPROVAL: &str = "execCommandApproval";
    /// The v1 spelling of the file-change approval.
    pub const LEGACY_APPLY_PATCH_APPROVAL: &str = "applyPatchApproval";
}

/// Why one of the server's questions is refused rather than answered.
///
/// Each is a sentence a maintainer can check against `docs/compliance.md`, not a shrug.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The host's tools are not the agent's to call.
    VendorToolsNeverEnterTheHostRegistry,
    /// The library has no login handling and forwards no token.
    NoLoginHandling,
    /// The question has no answer in the neutral approval contract.
    NoNeutralAnswer,
}

impl Refusal {
    /// The message the server receives, which is also what a maintainer reads in a log.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::VendorToolsNeverEnterTheHostRegistry => {
                "expected an approval this client can put to a person, received a request to run a \
                 tool on the agent's behalf; vendor tools never enter the host's tool registry"
            }
            Self::NoLoginHandling => {
                "expected an approval this client can put to a person, received a credential \
                 exchange; this client never reads, stores or forwards a vendor token"
            }
            Self::NoNeutralAnswer => {
                "expected an approval this client can put to a person, received a request whose \
                 answer is not a choice among the options the vendor offered"
            }
        }
    }
}

/// One question the app-server asked and is waiting on.
#[derive(Clone, Debug, PartialEq)]
pub enum ServerRequest {
    /// May the agent run this command?
    CommandExecution(CommandExecutionApprovalParams),
    /// May the agent write these files?
    FileChange(FileChangeApprovalParams),
    /// Would the client grant this permission profile?
    Permissions(PermissionsRequestApprovalParams),
    /// Would the client ask the user something on the agent's behalf?
    RequestUserInput(ToolRequestUserInputParams),
    /// An MCP server wants a form filled in.
    McpElicitation(McpServerElicitationRequestParams),
    /// Something this harness refuses, and why.
    Refused {
        /// The method the server used.
        method: String,
        /// Why it is refused.
        refusal: Refusal,
    },
}

impl ServerRequest {
    /// Reads one question.
    ///
    /// A question in an approval family whose params will not deserialise is refused rather than
    /// dropped: the server is blocked on an answer, and saying nothing would hang the turn.
    #[must_use]
    pub fn parse(method: &str, params: Value) -> Self {
        match method {
            method::COMMAND_EXECUTION_APPROVAL => serde_json::from_value(params).map_or_else(
                |_| Self::refused(method, Refusal::NoNeutralAnswer),
                Self::CommandExecution,
            ),
            method::FILE_CHANGE_APPROVAL => serde_json::from_value(params).map_or_else(
                |_| Self::refused(method, Refusal::NoNeutralAnswer),
                Self::FileChange,
            ),
            method::PERMISSIONS_APPROVAL => serde_json::from_value(params).map_or_else(
                |_| Self::refused(method, Refusal::NoNeutralAnswer),
                Self::Permissions,
            ),
            method::TOOL_REQUEST_USER_INPUT => serde_json::from_value(params).map_or_else(
                |_| Self::refused(method, Refusal::NoNeutralAnswer),
                Self::RequestUserInput,
            ),
            method::MCP_ELICITATION => serde_json::from_value(params).map_or_else(
                |_| Self::refused(method, Refusal::NoNeutralAnswer),
                Self::McpElicitation,
            ),
            method::TOOL_CALL => {
                Self::refused(method, Refusal::VendorToolsNeverEnterTheHostRegistry)
            }
            method::CHATGPT_AUTH_TOKENS_REFRESH => Self::refused(method, Refusal::NoLoginHandling),
            other => Self::refused(other, Refusal::NoNeutralAnswer),
        }
    }

    fn refused(method: &str, refusal: Refusal) -> Self {
        Self::Refused {
            method: method.to_owned(),
            refusal,
        }
    }

    /// Why this question is refused, when it is.
    #[must_use]
    pub fn refusal(&self) -> Option<Refusal> {
        match self {
            Self::Refused { refusal, .. } => Some(*refusal),
            _ => None,
        }
    }

    /// Which conversation the question belongs to, when the server said.
    #[must_use]
    pub fn thread_id(&self) -> Option<&str> {
        match self {
            Self::CommandExecution(params) => Some(params.thread_id.as_str()),
            Self::FileChange(params) => Some(params.thread_id.as_str()),
            Self::Permissions(params) => Some(params.thread_id.as_str()),
            Self::RequestUserInput(params) => Some(params.thread_id.as_str()),
            Self::McpElicitation(params) => Some(params.thread_id.as_str()),
            Self::Refused { .. } => None,
        }
    }

    /// Which native turn the question belongs to, when the server named one.
    /// For example, compare `request.turn_id()` with the active turn before offering approval.
    #[must_use]
    pub fn turn_id(&self) -> Option<&str> {
        match self {
            Self::CommandExecution(params) => Some(params.turn_id.as_str()),
            Self::FileChange(params) => Some(params.turn_id.as_str()),
            Self::Permissions(params) => Some(params.turn_id.as_str()),
            Self::RequestUserInput(params) => Some(params.turn_id.as_str()),
            Self::McpElicitation(params) => params.turn_id.as_deref(),
            Self::Refused { .. } => None,
        }
    }
}

/// May the agent run this command?
///
/// The server also writes an `availableDecisions` member that its own generated schema does not
/// declare. It is deliberately not read: this harness drives the documented surface, and building
/// the option set a person chooses from out of an undeclared field would make every prompt depend
/// on a member that can change without a schema change. The options are derived from the declared
/// [`ApprovalDecisionValue`] and the two declared amendment fields instead.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandExecutionApprovalParams {
    /// Which conversation.
    pub thread_id: String,
    /// Which turn.
    #[serde(default)]
    pub turn_id: String,
    /// The command-execution item this gates.
    #[serde(default)]
    pub item_id: String,
    /// The callback this question answers, when one item raises more than one.
    ///
    /// Upstream declares it as an opaque id distinguishing callbacks that share a parent item, so
    /// two questions about one command stay two questions. Absent on the ordinary shell approval,
    /// where the item is the question.
    #[serde(default)]
    pub approval_id: Option<String>,
    /// The command itself.
    #[serde(default)]
    pub command: Option<String>,
    /// Where it would run.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Why the agent is asking, in its own words.
    #[serde(default)]
    pub reason: Option<String>,
    /// An exec-policy change the agent proposes, so commands like this stop prompting.
    ///
    /// Present only when the server offers the amendment, which is what gates the matching option.
    #[serde(default)]
    pub proposed_execpolicy_amendment: Option<Value>,
    /// Network-policy changes it proposes, on the same terms.
    #[serde(default)]
    pub proposed_network_policy_amendments: Option<Vec<Value>>,
}

/// May the agent write these files?
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeApprovalParams {
    /// Which conversation.
    pub thread_id: String,
    /// Which turn.
    #[serde(default)]
    pub turn_id: String,
    /// The file-change item this gates.
    #[serde(default)]
    pub item_id: String,
    /// Why the agent is asking.
    #[serde(default)]
    pub reason: Option<String>,
    /// A directory it asks to write under for the rest of the session.
    #[serde(default)]
    pub grant_root: Option<String>,
}

/// Would the client grant this permission profile?
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionsRequestApprovalParams {
    /// Which conversation.
    pub thread_id: String,
    /// Which turn.
    #[serde(default)]
    pub turn_id: String,
    /// The item this gates.
    #[serde(default)]
    pub item_id: String,
    /// Where it would run.
    #[serde(default)]
    pub cwd: Option<String>,
    /// The profile the agent is asking to be granted.
    ///
    /// Opaque to this harness on purpose: the only things ever done with it are echoing it back
    /// verbatim on a grant and rendering it, compactly and unchanged, at the head of the
    /// [`PermissionRequest`] detail a host is shown before it can grant anything — never widened,
    /// narrowed or reshaped.
    ///
    /// [`PermissionRequest`]: mango_external_agents::permission::PermissionRequest
    pub permissions: Value,
    /// Why the agent is asking, in its own words.
    #[serde(default)]
    pub reason: Option<String>,
}

/// How far a granted permission profile reaches.
///
/// Carried only by a grant: [`PermissionsRequestApprovalResponse::deny`] states none, because
/// refusing decides nothing about anything beyond this one request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionGrantScope {
    /// The rest of this turn.
    Turn,
    /// The rest of this session.
    Session,
}

/// The answer `item/permissions/requestApproval` takes.
///
/// Never carries `strictAutoReview`: that field asks the vendor to change how *it* reviews later
/// requests, which is a standing instruction this library has no basis to give — the only thing it
/// ever grants is exactly the profile that was asked for, or nothing.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionsRequestApprovalResponse {
    /// The profile granted, echoed back verbatim from the request — or `{}` for none.
    pub permissions: Value,
    /// How far the grant reaches. Absent on a denial.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<PermissionGrantScope>,
}

impl PermissionsRequestApprovalResponse {
    /// A granted profile that grants nothing — the native negative for this family.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_agent_codex::protocol::approvals::PermissionsRequestApprovalResponse;
    /// let response = PermissionsRequestApprovalResponse::deny();
    /// assert_eq!(
    ///     serde_json::to_value(&response).unwrap(),
    ///     serde_json::json!({"permissions": {}})
    /// );
    /// ```
    #[must_use]
    pub fn deny() -> Self {
        Self {
            permissions: Value::Object(serde_json::Map::new()),
            scope: None,
        }
    }

    /// The requested profile, granted at this scope, exactly as asked.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_agent_codex::protocol::approvals::{
    ///     PermissionGrantScope, PermissionsRequestApprovalResponse,
    /// };
    /// let response = PermissionsRequestApprovalResponse::grant(
    ///     serde_json::json!({"fs": {"read": true}}),
    ///     PermissionGrantScope::Turn,
    /// );
    /// assert_eq!(
    ///     serde_json::to_value(&response).unwrap(),
    ///     serde_json::json!({"permissions": {"fs": {"read": true}}, "scope": "turn"})
    /// );
    /// ```
    #[must_use]
    pub fn grant(permissions: Value, scope: PermissionGrantScope) -> Self {
        Self {
            permissions,
            scope: Some(scope),
        }
    }
}

/// Would the client ask the user something on the agent's behalf?
///
/// Upstream's own schema marks this surface EXPERIMENTAL. `autoResolutionMs` is declared but
/// deliberately not read: it is documented as deprecated, and a deadline this harness already owns
/// (the same one every approval carries) is what governs an unanswered round instead.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRequestUserInputParams {
    /// Which conversation.
    pub thread_id: String,
    /// Which turn.
    #[serde(default)]
    pub turn_id: String,
    /// The tool-call item this round belongs to.
    #[serde(default)]
    pub item_id: String,
    /// Whether the vendor needs an answer before the tool call can go on.
    #[serde(default)]
    pub is_blocking: bool,
    /// What it wants to know.
    pub questions: Vec<ToolRequestUserInputQuestion>,
}

/// One question inside a `item/tool/requestUserInput` round.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRequestUserInputQuestion {
    /// The vendor's own id for this question.
    pub id: String,
    /// A short label.
    pub header: String,
    /// The question itself.
    pub question: String,
    /// The choices offered, when this is a choice question rather than free text.
    #[serde(default)]
    pub options: Option<Vec<ToolRequestUserInputOption>>,
    /// Whether the vendor offers a "something else" path alongside its declared choices.
    ///
    /// Not read beyond this struct: the neutral question contract has no arm for "one of these, or
    /// write your own", so a round that sets it is presented as its declared choices only.
    #[serde(default)]
    pub is_other: bool,
    /// Whether the vendor is asking for a credential, a password or a token.
    ///
    /// A round with any question marked this way is refused whole — see
    /// `crate::interaction::UnsupportedQuestion::SecretCollection`.
    #[serde(default)]
    pub is_secret: bool,
}

/// One choice a `ToolRequestUserInputQuestion` offers.
///
/// Carries no id of its own: the vendor's schema gives a choice only a label, so the label is what
/// this harness offers as the option's native identity and what the answer echoes back.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolRequestUserInputOption {
    /// The vendor's own label. Doubles as this option's id: nothing else names it.
    pub label: String,
    /// What the vendor says it means.
    #[serde(default)]
    pub description: Option<String>,
}

/// One question's answer, as `item/tool/requestUserInput` takes it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ToolRequestUserInputAnswer {
    /// The chosen labels, the written text as one string, or nothing for a declined question.
    pub answers: Vec<String>,
}

/// The answer `item/tool/requestUserInput` takes: one entry per question, by the vendor's own id.
///
/// A [`BTreeMap`] rather than the vendor's own arrival order, so two harness builds never write the
/// same round in a different byte order on the wire.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ToolRequestUserInputResponse {
    /// Which question each entry answers, and what it was answered with.
    pub answers: BTreeMap<String, ToolRequestUserInputAnswer>,
}

impl ToolRequestUserInputResponse {
    /// No question in the round was answered.
    ///
    /// What an expired, cancelled or refused round replies with: an empty map, never a partial one.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_agent_codex::protocol::approvals::ToolRequestUserInputResponse;
    /// assert_eq!(
    ///     serde_json::to_value(ToolRequestUserInputResponse::none()).unwrap(),
    ///     serde_json::json!({"answers": {}})
    /// );
    /// ```
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }
}

/// Would an MCP server like a form filled in?
///
/// Deliberately narrow: upstream also writes `message`, `requestedSchema`, `content`, `serverName`,
/// `_meta` and `url`. None of those are declared here, and unknown fields are simply ignored on the
/// way in — a form's fields can name a credential, and what this harness never deserialises it
/// cannot leak into a host-visible event or a log. See
/// `crate::interaction::UnsupportedQuestion::ArbitraryForm`, the only answer this harness ever gives
/// one.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerElicitationRequestParams {
    /// Which conversation.
    pub thread_id: String,
    /// Which turn, when the server named one.
    ///
    /// Optional here where the other approval families declare a plain `String`: the pinned
    /// schema's `url` branch carries no `turnId` at all, and a server is free to write the member
    /// as `null`. A `String` rejects an explicit null outright, which would fail the whole frame
    /// to deserialise and fall back to the JSON-RPC error this path exists to stop sending.
    #[serde(default)]
    pub turn_id: Option<String>,
    /// The vendor's own id for this ask, when the ask has one.
    ///
    /// Present only in the `url` mode: the pinned schema declares this type as a `oneOf` over
    /// `mode`, and `elicitationId` is required on that branch alone. Required here it would make
    /// every ordinary **form-mode** elicitation fail to deserialise and fall to the JSON-RPC error
    /// this whole path exists to stop sending. The audit event falls back to the request's own
    /// JSON-RPC id, which the server is waiting on either way.
    #[serde(default)]
    pub elicitation_id: Option<String>,
}

/// The only answer this harness ever gives an elicitation: a native decline.
///
/// Never `accept` — this library renders no arbitrary form. Never `cancel` either: the vendor's own
/// documentation is that `decline` lets the turn continue while `cancel` ends it, and refusing to
/// render a form this library does not own is not a reason to end somebody's turn.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct McpServerElicitationRequestResponse {
    action: &'static str,
}

impl McpServerElicitationRequestResponse {
    /// The one answer this harness ever sends.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_agent_codex::protocol::approvals::McpServerElicitationRequestResponse;
    /// assert_eq!(
    ///     serde_json::to_value(McpServerElicitationRequestResponse::decline()).unwrap(),
    ///     serde_json::json!({"action": "decline"})
    /// );
    /// ```
    #[must_use]
    pub const fn decline() -> Self {
        Self { action: "decline" }
    }
}

/// One family's answer, in the shape its own wire response takes.
///
/// Not every family answers with `{"decision": ...}`: `item/permissions/requestApproval` answers
/// with its own `{"permissions": ..., "scope": ...}` shape entirely. This is what lets a session
/// hand any settled approval back to the app-server without a caller matching on which family it
/// belongs to.
#[derive(Clone, Debug, PartialEq)]
pub enum ServerAnswer {
    /// The command- and file-change families: a bare decision string or a tagged amendment.
    Approval(ApprovalDecisionValue),
    /// The permissions family: the profile granted, or `{}` for none.
    Permissions {
        /// The id this harness offers the option under, and reads back from the answer.
        option_id: &'static str,
        /// The wire response this option answers with.
        response: PermissionsRequestApprovalResponse,
    },
}

impl ServerAnswer {
    /// The id this harness offers the option under, and reads back from the answer.
    #[must_use]
    pub fn option_id(&self) -> &str {
        match self {
            Self::Approval(decision) => decision.option_id(),
            Self::Permissions { option_id, .. } => option_id,
        }
    }

    /// This answer, in the shape the app-server takes on the wire.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_agent_codex::protocol::approvals::{ApprovalDecisionValue, ServerAnswer};
    /// let answer = ServerAnswer::Approval(ApprovalDecisionValue::Decline);
    /// assert_eq!(answer.to_wire(), serde_json::json!({"decision": "decline"}));
    /// ```
    #[must_use]
    pub fn to_wire(&self) -> Value {
        match self {
            Self::Approval(decision) => serde_json::to_value(ApprovalResponse {
                decision: decision.clone(),
            }),
            Self::Permissions { response, .. } => serde_json::to_value(response),
        }
        .unwrap_or_else(|_| Value::Object(serde_json::Map::new()))
    }
}

/// One answer the app-server will take, as it is written on the wire.
///
/// The two plain-string values both families share, plus the two amendment-carrying values only a
/// command approval offers. Serialised by hand rather than derived: the amendment arms are tagged
/// objects whose inner keys are snake_case inside an otherwise camelCase dialect, and the payload
/// is whatever JSON the request carried rather than a shape this harness models.
#[derive(Clone, Debug, PartialEq)]
pub enum ApprovalDecisionValue {
    /// Allow this one thing.
    Accept,
    /// Allow this and anything like it for the rest of the session.
    AcceptForSession,
    /// Refuse this one thing. The turn goes on.
    Decline,
    /// Refuse and stop the turn.
    Cancel,
    /// Allow, and take the exec-policy amendment the request proposed.
    AcceptWithExecpolicyAmendment(Value),
    /// Allow, and take the network-policy amendment the request proposed.
    ApplyNetworkPolicyAmendment(Value),
}

impl ApprovalDecisionValue {
    /// The id this harness offers the option under, and reads back from the answer.
    #[must_use]
    pub fn option_id(&self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::AcceptForSession => "acceptForSession",
            Self::Decline => "decline",
            Self::Cancel => "cancel",
            Self::AcceptWithExecpolicyAmendment(_) => "acceptWithExecpolicyAmendment",
            Self::ApplyNetworkPolicyAmendment(_) => "applyNetworkPolicyAmendment",
        }
    }
}

impl Serialize for ApprovalDecisionValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            Self::Accept | Self::AcceptForSession | Self::Decline | Self::Cancel => {
                serializer.serialize_str(self.option_id())
            }
            Self::AcceptWithExecpolicyAmendment(amendment) => {
                let mut inner = serde_json::Map::new();
                inner.insert(String::from("execpolicy_amendment"), amendment.clone());
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry(self.option_id(), &Value::Object(inner))?;
                map.end()
            }
            Self::ApplyNetworkPolicyAmendment(amendment) => {
                let mut inner = serde_json::Map::new();
                inner.insert(String::from("network_policy_amendment"), amendment.clone());
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry(self.option_id(), &Value::Object(inner))?;
                map.end()
            }
        }
    }
}

/// The answer frame both approval families take.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalResponse {
    /// What was decided.
    pub decision: ApprovalDecisionValue,
}

#[cfg(test)]
mod tests {
    use super::{
        ApprovalDecisionValue, ApprovalResponse, McpServerElicitationRequestResponse,
        PermissionGrantScope, Refusal, ServerAnswer, ServerRequest, ToolRequestUserInputResponse,
        method,
    };
    use serde_json::json;

    /// The captured frame from a real turn, with the undeclared member the server also writes.
    #[test]
    fn a_command_approval_reads_the_frame_the_app_server_wrote() {
        let request = ServerRequest::parse(
            method::COMMAND_EXECUTION_APPROVAL,
            json!({
                "kind": "command",
                "threadId": "01a09999-7858",
                "turnId": "01a09999-78a8",
                "itemId": "exec-ee0f9baa",
                "startedAtMs": 1_789_283_381_284u64,
                "environmentId": "local",
                "reason": "Allow creating mango.txt outside the read-only sandbox?",
                "command": "/bin/bash -lc \"printf 'mango' > mango.txt\"",
                "cwd": "/workspace",
                "commandActions": [{"type": "unknown", "command": "printf 'mango' > mango.txt"}],
                "proposedExecpolicyAmendment": ["/bin/bash", "-lc", "printf 'mango' > mango.txt"],
                "availableDecisions": ["accept", "cancel"]
            }),
        );

        assert_eq!(request.turn_id(), Some("01a09999-78a8"));
        let ServerRequest::CommandExecution(params) = request else {
            panic!("expected a command approval");
        };
        assert_eq!(params.thread_id, "01a09999-7858");
        assert_eq!(params.item_id, "exec-ee0f9baa");
        assert_eq!(
            params.approval_id, None,
            "expected the ordinary shell approval to carry no callback of its own"
        );
        assert!(params.command.is_some());
        assert!(
            params.proposed_execpolicy_amendment.is_some(),
            "expected the declared amendment field to be read"
        );
    }

    /// The invariant, as a refusal the server receives rather than a silence it waits on.
    #[test]
    fn a_request_to_run_a_tool_is_refused_by_the_invariant_that_names_it() {
        let request = ServerRequest::parse(method::TOOL_CALL, json!({"tool": "read_file"}));
        assert_eq!(
            request.refusal(),
            Some(Refusal::VendorToolsNeverEnterTheHostRegistry)
        );
        assert!(
            Refusal::VendorToolsNeverEnterTheHostRegistry
                .message()
                .contains("never enter the host's tool registry"),
            "expected the invariant in the refusal the server reads"
        );
    }

    /// The library has no login handling anywhere, including here.
    #[test]
    fn a_request_for_a_refreshed_token_is_refused_without_looking_at_it() {
        let request = ServerRequest::parse(
            method::CHATGPT_AUTH_TOKENS_REFRESH,
            json!({"idToken": "eyJhbGciOi"}),
        );
        assert_eq!(request.refusal(), Some(Refusal::NoLoginHandling));
        let rendered = format!("{request:?}");
        assert!(
            !rendered.contains("eyJhbGciOi"),
            "expected nothing of the token to survive into the value, received {rendered}"
        );
    }

    /// Every remaining family the server can ask is refused rather than left hanging.
    #[test]
    fn every_other_question_is_refused_rather_than_left_waiting() {
        for family in [
            method::ATTESTATION_GENERATE,
            method::LEGACY_EXEC_COMMAND_APPROVAL,
            method::LEGACY_APPLY_PATCH_APPROVAL,
            "somethingTheNextReleaseAdded",
        ] {
            let request = ServerRequest::parse(family, json!({}));
            assert!(
                request.refusal().is_some(),
                "expected {family} to be refused, received {request:?}"
            );
        }
    }

    /// A permissions grant reads the fields this harness drives, and leaves `permissions` opaque.
    #[test]
    fn a_permissions_request_reads_the_frame_the_app_server_wrote() {
        let request = ServerRequest::parse(
            method::PERMISSIONS_APPROVAL,
            json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "itemId": "item-1",
                "cwd": "/workspace",
                "permissions": {"fs": {"read": true}},
                "reason": "needs to read outside the sandbox",
                "environmentId": "local",
                "startedAtMs": 1u64,
            }),
        );
        assert_eq!(request.thread_id(), Some("thread-1"));
        assert_eq!(request.turn_id(), Some("turn-1"));
        let ServerRequest::Permissions(params) = request else {
            panic!("expected a permissions request");
        };
        assert_eq!(params.permissions, json!({"fs": {"read": true}}));
        assert_eq!(params.cwd.as_deref(), Some("/workspace"));
    }

    /// A request-user-input round reads its questions, including a question offering no options.
    #[test]
    fn a_request_user_input_round_reads_every_question_the_app_server_wrote() {
        let request = ServerRequest::parse(
            method::TOOL_REQUEST_USER_INPUT,
            json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "itemId": "item-1",
                "isBlocking": true,
                "questions": [
                    {
                        "id": "branch",
                        "header": "Branch",
                        "question": "Which branch?",
                        "options": [{"label": "main"}, {"label": "next", "description": "the other one"}],
                    },
                    {
                        "id": "note",
                        "header": "Note",
                        "question": "Anything else?",
                        "isSecret": true,
                    },
                ],
            }),
        );
        let ServerRequest::RequestUserInput(params) = request else {
            panic!("expected a request-user-input round");
        };
        assert!(params.is_blocking);
        assert_eq!(params.questions.len(), 2);
        assert_eq!(params.questions[0].options.as_ref().map(Vec::len), Some(2));
        assert!(params.questions[1].is_secret);
    }

    /// An elicitation reads only routing and identity — never the form's own fields.
    #[test]
    fn an_elicitation_never_deserialises_the_forms_own_fields() {
        let request = ServerRequest::parse(
            method::MCP_ELICITATION,
            json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "elicitationId": "elicit-1",
                "message": "enter your token",
                "requestedSchema": {"type": "object"},
                "serverName": "server-secret",
            }),
        );
        let ServerRequest::McpElicitation(params) = request else {
            panic!("expected an elicitation");
        };
        assert_eq!(params.thread_id, "thread-1");
        assert_eq!(params.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(params.elicitation_id.as_deref(), Some("elicit-1"));
        // The struct declares no field for `message`, `requestedSchema` or `serverName`, so
        // nothing of them can appear here even in debug output.
        let rendered = format!("{params:?}");
        assert!(!rendered.contains("token"));
        assert!(!rendered.contains("server-secret"));
    }

    /// The ordinary elicitation is the one that carries no id.
    ///
    /// The pinned schema declares this type as a `oneOf` over `mode`, and `elicitationId` is
    /// required on the `url` branch alone. A required field here would make every form-mode
    /// elicitation fail to deserialise and fall back to the JSON-RPC error this path exists to
    /// stop sending — the exact case the native decline is for.
    #[test]
    fn a_form_mode_elicitation_with_no_id_of_its_own_still_parses() {
        let request = ServerRequest::parse(
            method::MCP_ELICITATION,
            json!({
                "threadId": "thread-1",
                "turnId": "turn-1",
                "mode": "form",
                "message": "enter your token",
                "requestedSchema": {"type": "object"},
                "serverName": "server-1",
            }),
        );
        let ServerRequest::McpElicitation(params) = request else {
            panic!("expected an elicitation, received {request:?}");
        };
        assert_eq!(params.elicitation_id, None);
        assert_eq!(params.thread_id, "thread-1");

        let url_mode = ServerRequest::parse(
            method::MCP_ELICITATION,
            json!({
                "threadId": "thread-1",
                "mode": "url",
                "elicitationId": "elicit-1",
                "message": "open this",
                "url": "https://example.test/form",
            }),
        );
        let ServerRequest::McpElicitation(params) = url_mode else {
            panic!("expected an elicitation, received {url_mode:?}");
        };
        assert_eq!(params.elicitation_id.as_deref(), Some("elicit-1"));
        assert_eq!(
            params.turn_id, None,
            "the pin's url branch carries no turn of its own"
        );

        // A server that writes the member as `null` says the same thing: no correlation, not a
        // malformed frame. A plain `String` would reject it and fail the whole frame to parse.
        let nulled = ServerRequest::parse(
            method::MCP_ELICITATION,
            json!({
                "threadId": "thread-1",
                "turnId": null,
                "mode": "form",
                "message": "enter your token",
                "requestedSchema": {"type": "object"},
            }),
        );
        let ServerRequest::McpElicitation(params) = nulled else {
            panic!("expected an elicitation, received {nulled:?}");
        };
        assert_eq!(params.turn_id, None);
        assert_eq!(params.thread_id, "thread-1");
    }

    /// The wire shape for each of the three permission decisions.
    #[test]
    fn a_permissions_grant_echoes_its_scope_and_a_denial_echoes_nothing() {
        let deny = ServerAnswer::Permissions {
            option_id: "deny",
            response: super::PermissionsRequestApprovalResponse::deny(),
        };
        assert_eq!(deny.to_wire(), json!({"permissions": {}}));

        let turn = ServerAnswer::Permissions {
            option_id: "grant:turn",
            response: super::PermissionsRequestApprovalResponse::grant(
                json!({"fs": {"read": true}}),
                PermissionGrantScope::Turn,
            ),
        };
        assert_eq!(
            turn.to_wire(),
            json!({"permissions": {"fs": {"read": true}}, "scope": "turn"})
        );

        let session = ServerAnswer::Permissions {
            option_id: "grant:session",
            response: super::PermissionsRequestApprovalResponse::grant(
                json!({"fs": {"read": true}}),
                PermissionGrantScope::Session,
            ),
        };
        assert_eq!(
            session.to_wire(),
            json!({"permissions": {"fs": {"read": true}}, "scope": "session"})
        );
    }

    /// The wire shape for a native elicitation decline.
    #[test]
    fn an_elicitation_decline_is_the_bare_action_string() {
        assert_eq!(
            serde_json::to_value(McpServerElicitationRequestResponse::decline())
                .expect("expected a frame"),
            json!({"action": "decline"})
        );
    }

    /// The wire shape a question round answers with: one entry per question, by the vendor's own
    /// id, and an empty map for a round nobody answered.
    #[test]
    fn a_question_round_answer_maps_each_question_id_to_its_own_answers() {
        let mut answers = std::collections::BTreeMap::new();
        answers.insert(
            String::from("branch"),
            super::ToolRequestUserInputAnswer {
                answers: vec![String::from("next")],
            },
        );
        let response = ToolRequestUserInputResponse { answers };
        assert_eq!(
            serde_json::to_value(response).expect("expected a frame"),
            json!({"answers": {"branch": {"answers": ["next"]}}})
        );
        assert_eq!(
            serde_json::to_value(ToolRequestUserInputResponse::none()).expect("expected a frame"),
            json!({"answers": {}})
        );
    }

    /// A question whose params are malformed still blocks the server, so it is refused, not lost.
    #[test]
    fn an_approval_whose_params_will_not_parse_is_refused_rather_than_dropped() {
        let request = ServerRequest::parse(method::FILE_CHANGE_APPROVAL, json!({"threadId": 7}));
        assert_eq!(request.refusal(), Some(Refusal::NoNeutralAnswer));
    }

    #[test]
    fn a_plain_decision_is_a_bare_string_on_the_wire() {
        let frame = serde_json::to_value(ApprovalResponse {
            decision: ApprovalDecisionValue::Decline,
        })
        .expect("expected a frame");
        assert_eq!(frame, json!({"decision": "decline"}));
    }

    /// The amendment arms are tagged objects whose inner key is snake_case. Getting either wrong
    /// is a `-32602` on a prompt a person already answered.
    #[test]
    fn an_amendment_decision_carries_the_payload_under_the_vendors_own_inner_key() {
        let amendment = json!(["/bin/bash", "-lc", "printf 'mango' > mango.txt"]);
        let frame = serde_json::to_value(ApprovalResponse {
            decision: ApprovalDecisionValue::AcceptWithExecpolicyAmendment(amendment.clone()),
        })
        .expect("expected a frame");

        assert_eq!(
            frame,
            json!({"decision": {"acceptWithExecpolicyAmendment": {
                "execpolicy_amendment": amendment
            }}})
        );

        let network = json!({"host": "example.com", "allow": true});
        let frame = serde_json::to_value(ApprovalResponse {
            decision: ApprovalDecisionValue::ApplyNetworkPolicyAmendment(network.clone()),
        })
        .expect("expected a frame");
        assert_eq!(
            frame,
            json!({"decision": {"applyNetworkPolicyAmendment": {
                "network_policy_amendment": network
            }}})
        );
    }
}
