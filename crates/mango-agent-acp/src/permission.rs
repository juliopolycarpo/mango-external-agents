//! `session/request_permission` in, a brokered answer out.
//!
//! Nothing here decides anything. The mapping's whole job is to make a decision *possible*: ACP's
//! four option kinds become the core's four, so a host policy can answer "allow" without reading a
//! label in a language it does not know, and the option set itself is passed through untouched —
//! same ids, same order, same words the agent wrote.
//!
//! ACP v1 reference: <https://agentclientprotocol.com/protocol/v1/tool-calls#requesting-permission>

use std::time::SystemTime;

use agent_client_protocol::schema::v1::{
    PermissionOption as AcpPermissionOption, PermissionOptionKind as AcpPermissionOptionKind,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, ToolCallUpdate,
};
use mango_external_agents::permission::{
    PermissionOption, PermissionOptionKind, PermissionRequest,
};

use crate::reducer;

/// One ACP permission request as the question a host answers.
///
/// `id` is the harness's own correlation id rather than anything in the payload: ACP's
/// `session/request_permission` carries no id of its own, and the tool call id it does carry repeats
/// when an agent asks about the same call twice. The JSON-RPC request id is unique per connection and
/// is what the answer has to be routed back to, so that is what the harness uses.
#[must_use]
pub fn request_from(
    request: &RequestPermissionRequest,
    id: String,
    expires_at: SystemTime,
) -> PermissionRequest {
    let tool_call = &request.tool_call;
    PermissionRequest {
        id,
        kind: reducer::activity_kind(tool_call.fields.kind.unwrap_or_default()),
        title: title(tool_call),
        detail: detail(tool_call),
        options: request.options.iter().map(option).collect(),
        expires_at,
        truncated: false,
    }
}

/// The one line a host puts in the prompt.
///
/// Falls back to the agent's own call id rather than to empty text: a permission dialog that says
/// nothing is a dialog nobody can answer, and the id at least names what is being asked about.
fn title(tool_call: &ToolCallUpdate) -> String {
    tool_call
        .fields
        .title
        .clone()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| tool_call.tool_call_id.to_string())
}

fn detail(tool_call: &ToolCallUpdate) -> Option<String> {
    let content = tool_call.fields.content.as_deref()?;
    reducer::content_detail(content)
}

/// One choice, with the agent's own id and words kept exactly as sent.
fn option(option: &AcpPermissionOption) -> PermissionOption {
    PermissionOption {
        id: option.option_id.to_string(),
        kind: option_kind(&option.kind),
        label: Some(option.name.clone()),
        // ACP v1 has no destructive marker on a permission option. Reporting `true` would put a
        // warning on a choice the agent never flagged; reporting `false` is what the wire said.
        destructive: false,
    }
}

/// What an ACP option kind means, so a policy can answer without reading a label.
///
/// One to one: ACP's four are the same four the core models, which is why a broker written against
/// the core works against every ACP agent without a per-agent table. The `#[non_exhaustive]` tail
/// falls to [`PermissionOptionKind::Other`] — a kind this build does not know is a choice only a
/// person can weigh, and guessing that an unknown kind allows would be the worst possible guess.
#[must_use]
pub fn option_kind(kind: &AcpPermissionOptionKind) -> PermissionOptionKind {
    match kind {
        AcpPermissionOptionKind::AllowOnce => PermissionOptionKind::AllowOnce,
        AcpPermissionOptionKind::AllowAlways => PermissionOptionKind::AllowAlways,
        AcpPermissionOptionKind::RejectOnce => PermissionOptionKind::RejectOnce,
        AcpPermissionOptionKind::RejectAlways => PermissionOptionKind::RejectAlways,
        _ => PermissionOptionKind::Other,
    }
}

/// The answer that names one of the agent's own options.
#[must_use]
pub fn selected(option_id: &str) -> RequestPermissionResponse {
    RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
        SelectedPermissionOutcome::new(String::from(option_id)),
    ))
}

/// The answer for a question nobody will answer, because the turn is over.
///
/// ACP's own outcome for it, rather than a rejection: the agent is told the question was withdrawn,
/// not that somebody said no. A withdrawn question is not a refusal a `reject_always` agent should
/// remember.
#[must_use]
pub fn cancelled() -> RequestPermissionResponse {
    RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)
}

#[cfg(test)]
mod tests {
    use super::{cancelled, option_kind, request_from, selected};
    use agent_client_protocol::schema::v1::{
        PermissionOptionKind as AcpPermissionOptionKind, RequestPermissionOutcome,
        RequestPermissionRequest,
    };
    use mango_external_agents::event::ActivityKind;
    use mango_external_agents::permission::{DecisionSource, PermissionOptionKind};
    use serde_json::json;
    use std::time::{Duration, SystemTime};

    fn deadline() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    fn parse(value: serde_json::Value) -> RequestPermissionRequest {
        serde_json::from_value(value).expect("expected a v1 permission request")
    }

    fn asking() -> RequestPermissionRequest {
        parse(json!({
            "sessionId": "sess_1",
            "toolCall": {
                "toolCallId": "call_9",
                "kind": "execute",
                "title": "Run `rm -rf build`",
                "content": [{ "type": "content", "content": { "type": "text", "text": "cwd /repo" }}]
            },
            "options": [
                { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
                { "optionId": "allow-all", "name": "Always allow", "kind": "allow_always" },
                { "optionId": "reject", "name": "Reject", "kind": "reject_once" }
            ]
        }))
    }

    #[test]
    fn a_request_carries_what_is_being_asked_and_every_choice_untouched() {
        let request = request_from(&asking(), String::from("rpc-7"), deadline());

        assert_eq!(request.id, "rpc-7");
        assert_eq!(request.kind, ActivityKind::Command);
        assert_eq!(request.title, "Run `rm -rf build`");
        assert_eq!(request.detail.as_deref(), Some("cwd /repo"));
        assert_eq!(request.expires_at, deadline());
        assert_eq!(
            request
                .options
                .iter()
                .map(|option| (option.id.as_str(), option.kind, option.label.as_deref()))
                .collect::<Vec<_>>(),
            vec![
                ("allow", PermissionOptionKind::AllowOnce, Some("Allow")),
                (
                    "allow-all",
                    PermissionOptionKind::AllowAlways,
                    Some("Always allow")
                ),
                ("reject", PermissionOptionKind::RejectOnce, Some("Reject")),
            ]
        );
    }

    /// The point of mapping the kinds: a host policy answers without reading a label, and the answer
    /// it produces names one of the agent's own option ids.
    #[test]
    fn a_mapped_request_can_be_allowed_and_refused_by_a_policy_that_never_read_a_label() {
        let request = request_from(&asking(), String::from("rpc-7"), deadline());

        let allow = request.allow().expect("expected an allowing option");
        assert_eq!(allow.option_id, "allow", "expected the narrow allow to win");
        assert_eq!(allow.source, DecisionSource::User);

        let deny = request.deny().expect("expected a refusing option");
        assert_eq!(deny.option_id, "reject");
    }

    #[test]
    fn every_acp_option_kind_maps_onto_the_neutral_one() {
        let cases = [
            (
                AcpPermissionOptionKind::AllowOnce,
                PermissionOptionKind::AllowOnce,
            ),
            (
                AcpPermissionOptionKind::AllowAlways,
                PermissionOptionKind::AllowAlways,
            ),
            (
                AcpPermissionOptionKind::RejectOnce,
                PermissionOptionKind::RejectOnce,
            ),
            (
                AcpPermissionOptionKind::RejectAlways,
                PermissionOptionKind::RejectAlways,
            ),
        ];
        for (acp, expected) in cases {
            assert_eq!(
                option_kind(&acp),
                expected,
                "received a mismatch for {acp:?}"
            );
        }
    }

    /// A dialog with no words is a dialog nobody can answer, so the agent's own call id stands in.
    #[test]
    fn a_request_with_no_title_falls_back_to_the_agents_call_id() {
        let request = request_from(
            &parse(json!({
                "sessionId": "sess_1",
                "toolCall": { "toolCallId": "call_bare" },
                "options": [{ "optionId": "ok", "name": "OK", "kind": "allow_once" }]
            })),
            String::from("rpc-1"),
            deadline(),
        );
        assert_eq!(request.title, "call_bare");
        assert_eq!(request.detail, None);
        assert_eq!(request.kind, ActivityKind::Other);
    }

    #[test]
    fn an_answer_names_the_agents_own_option_id() {
        let RequestPermissionOutcome::Selected(outcome) = selected("allow-all").outcome else {
            panic!("expected a selection");
        };
        assert_eq!(outcome.option_id.to_string(), "allow-all");
    }

    /// A withdrawn question is not a refusal: telling an agent "rejected" would invite it to
    /// remember a standing "no" nobody said.
    #[test]
    fn a_withdrawn_question_is_cancelled_rather_than_rejected() {
        assert!(matches!(
            cancelled().outcome,
            RequestPermissionOutcome::Cancelled
        ));
    }
}
