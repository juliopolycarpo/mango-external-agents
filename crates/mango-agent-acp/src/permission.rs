//! `session/request_permission` in, a brokered answer out.
//!
//! Nothing here decides anything. The mapping's whole job is to make a decision *possible*: ACP's
//! four option kinds become the core's effect/scope/policy-changing vocabulary, so a host policy
//! can answer "allow" without reading a label in a language it does not know, and the option set
//! itself is passed through untouched — same ids, same order, same words the agent wrote.
//!
//! ACP v1 reference: <https://agentclientprotocol.com/protocol/v1/tool-calls#requesting-permission>

use std::time::SystemTime;

use agent_client_protocol::schema::v1::{
    PermissionOption as AcpPermissionOption, PermissionOptionKind as AcpPermissionOptionKind,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, ToolCallUpdate,
};
use mango_external_agents::event::SessionId;
use mango_external_agents::operation::OperationRef;
use mango_external_agents::permission::{PermissionEffect, PermissionOption, PermissionRequest};
use mango_external_agents::{Interaction, InteractionId, InteractionKind};

use crate::reducer;

/// One ACP permission request as the question a host answers.
///
/// `id` is the harness's own correlation id rather than anything in the payload: ACP's
/// `session/request_permission` carries no id of its own, and the tool call id it does carry repeats
/// when an agent asks about the same call twice. It is not the JSON-RPC request id either — that id
/// is the agent's to choose, and an agent is free to reuse one once the request it named is no longer
/// outstanding, which would let a host answer meant for one question land on a later, unrelated one
/// that happened to reuse the same id. The harness mints this id itself, once per question and never
/// reused, and passes it in here already resolved.
#[must_use]
pub fn request_from(
    request: &RequestPermissionRequest,
    id: String,
    session_id: SessionId,
    operation: OperationRef,
    expires_at: SystemTime,
) -> PermissionRequest {
    let tool_call = &request.tool_call;
    let interaction = Interaction::new(
        InteractionId::new(id),
        InteractionKind::Permission,
        session_id,
        expires_at,
    )
    .during(operation);
    let built = PermissionRequest::new(
        interaction,
        reducer::activity_kind(tool_call.fields.kind.unwrap_or_default()),
        title(tool_call),
        request.options.iter().map(option).collect(),
    );
    match detail(tool_call) {
        Some(detail) => built.with_detail(detail),
        None => built,
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
///
/// ACP v1 has no destructive marker on a permission option, so [`PermissionOption::risk`] is left
/// at its default of `Unspecified` — reporting `Destructive` would put a warning on a choice the
/// agent never flagged.
fn option(option: &AcpPermissionOption) -> PermissionOption {
    let built = PermissionOption::new(option.option_id.to_string(), effect(&option.kind))
        .with_label(option.name.clone());
    match scope(&option.kind) {
        Scoped::Once => built.with_scope(mango_external_agents::PermissionScope::Once),
        // "Allow/reject this operation and remember the choice" states that a standing rule is
        // written, but never says how far it reaches — not the rest of the session, not
        // persistently across sessions. Reporting a scope here would be inventing a reach the
        // protocol never promised; `policy_changing` alone says everything ACP actually states.
        Scoped::Remembered => built.policy_changing(),
    }
}

enum Scoped {
    Once,
    Remembered,
}

fn effect(kind: &AcpPermissionOptionKind) -> PermissionEffect {
    match kind {
        AcpPermissionOptionKind::AllowOnce | AcpPermissionOptionKind::AllowAlways => {
            PermissionEffect::Allow
        }
        AcpPermissionOptionKind::RejectOnce | AcpPermissionOptionKind::RejectAlways => {
            PermissionEffect::Reject
        }
        // `#[non_exhaustive]`: a kind this build does not know is a choice only a person can
        // weigh, and guessing that an unknown kind allows would be the worst possible guess.
        _ => PermissionEffect::Other,
    }
}

fn scope(kind: &AcpPermissionOptionKind) -> Scoped {
    match kind {
        AcpPermissionOptionKind::AllowOnce | AcpPermissionOptionKind::RejectOnce => Scoped::Once,
        _ => Scoped::Remembered,
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
    use super::{cancelled, effect, request_from, scope, selected};
    use agent_client_protocol::schema::v1::{
        PermissionOptionKind as AcpPermissionOptionKind, RequestPermissionOutcome,
        RequestPermissionRequest,
    };
    use mango_external_agents::event::ActivityKind;
    use mango_external_agents::operation::{AttemptId, OperationRef};
    use mango_external_agents::permission::{DecisionSource, PermissionEffect};
    use mango_external_agents::{PermissionScope, SessionId, TurnId};
    use serde_json::json;
    use std::time::{Duration, SystemTime};

    fn deadline() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    fn operation() -> OperationRef {
        OperationRef::new(
            SessionId::new("sess_1"),
            TurnId::new("turn-1"),
            AttemptId::new("attempt-1"),
        )
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
        let request = request_from(
            &asking(),
            String::from("rpc-7"),
            SessionId::new("sess_1"),
            operation(),
            deadline(),
        );

        assert_eq!(request.id().as_str(), "rpc-7");
        assert_eq!(request.kind, ActivityKind::Command);
        assert_eq!(request.title, "Run `rm -rf build`");
        assert_eq!(request.detail.as_deref(), Some("cwd /repo"));
        assert_eq!(request.expires_at(), deadline());
        assert_eq!(
            request
                .options
                .iter()
                .map(|option| (
                    option.id.as_str(),
                    option.effect,
                    option.scope,
                    option.policy_changing,
                    option.label.as_deref()
                ))
                .collect::<Vec<_>>(),
            vec![
                (
                    "allow",
                    PermissionEffect::Allow,
                    Some(PermissionScope::Once),
                    false,
                    Some("Allow")
                ),
                (
                    "allow-all",
                    PermissionEffect::Allow,
                    None,
                    true,
                    Some("Always allow")
                ),
                (
                    "reject",
                    PermissionEffect::Reject,
                    Some(PermissionScope::Once),
                    false,
                    Some("Reject")
                ),
            ]
        );
    }

    /// The point of mapping the kinds: a host policy answers without reading a label, and the answer
    /// it produces names one of the agent's own option ids.
    #[test]
    fn a_mapped_request_can_be_allowed_and_refused_by_a_policy_that_never_read_a_label() {
        let request = request_from(
            &asking(),
            String::from("rpc-7"),
            SessionId::new("sess_1"),
            operation(),
            deadline(),
        );

        let allow = request.allow().expect("expected an allowing option");
        assert_eq!(allow.option_id, "allow", "expected the narrow allow to win");
        assert_eq!(allow.source, DecisionSource::User);

        let deny = request.deny().expect("expected a refusing option");
        assert_eq!(deny.option_id, "reject");
    }

    #[test]
    fn every_acp_option_kind_maps_onto_an_effect() {
        let cases = [
            (AcpPermissionOptionKind::AllowOnce, PermissionEffect::Allow),
            (
                AcpPermissionOptionKind::AllowAlways,
                PermissionEffect::Allow,
            ),
            (
                AcpPermissionOptionKind::RejectOnce,
                PermissionEffect::Reject,
            ),
            (
                AcpPermissionOptionKind::RejectAlways,
                PermissionEffect::Reject,
            ),
        ];
        for (acp, expected) in cases {
            assert_eq!(effect(&acp), expected, "received a mismatch for {acp:?}");
        }
    }

    /// "Remember the choice" is the whole of what ACP states about `*_always`: not "for this
    /// session", not "forever". Reporting a scope would invent a reach the protocol never
    /// promised, so only `policy_changing` is set.
    #[test]
    fn an_always_option_is_policy_changing_with_no_invented_scope() {
        assert!(matches!(
            scope(&AcpPermissionOptionKind::AllowAlways),
            super::Scoped::Remembered
        ));
        assert!(matches!(
            scope(&AcpPermissionOptionKind::RejectAlways),
            super::Scoped::Remembered
        ));
        assert!(matches!(
            scope(&AcpPermissionOptionKind::AllowOnce),
            super::Scoped::Once
        ));
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
            SessionId::new("sess_1"),
            operation(),
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
