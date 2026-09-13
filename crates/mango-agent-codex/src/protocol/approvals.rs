//! What the app-server asks the client, and the answers it will take.
//!
//! This is the direction that makes a harness semantic rather than a codec: the server stops and
//! waits. Two of its questions are approvals a person can answer; the rest are refused with a
//! JSON-RPC error, and [`ServerRequest::refusal`] says why for each — including
//! `item/tool/call`, where the refusal is the library's central invariant rather than a gap.

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
    use super::{ApprovalDecisionValue, ApprovalResponse, Refusal, ServerRequest, method};
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
            method::TOOL_REQUEST_USER_INPUT,
            method::MCP_ELICITATION,
            method::PERMISSIONS_APPROVAL,
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
