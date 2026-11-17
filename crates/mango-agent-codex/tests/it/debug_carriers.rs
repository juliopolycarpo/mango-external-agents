//! `Debug` for the Codex interaction carriers reports metadata and never the vendor's content.
//!
//! `docs/compliance.md` puts interactions under the metadata-only `Debug` policy. A derive would
//! print a command, a reason, an amendment prefix, a host or a permission profile the moment a
//! host debug-logs one of these values, so each carrier is given a canary in every content field
//! and formatted both ways a logger might.

use std::fmt::Debug;

use mango_agent_codex::protocol::approvals::{
    ApprovalDecisionValue, ApprovalResponse, CommandExecutionApprovalParams,
    FileChangeApprovalParams, McpServerElicitationRequestParams, PermissionGrantScope,
    PermissionsRequestApprovalParams, PermissionsRequestApprovalResponse, ServerAnswer,
    ServerRequest, ToolRequestUserInputAnswer, ToolRequestUserInputResponse, method,
};
use serde_json::json;

/// Fails, naming the type and the field, when either debug form carries the canary.
fn assert_omits(type_name: &str, value: &dyn Debug, canaries: &[(&str, &str)]) {
    let compact = format!("{value:?}");
    let pretty = format!("{value:#?}");
    for (field, canary) in canaries {
        assert!(
            !compact.contains(canary),
            "expected {type_name} {{:?}} to omit the {field} canary {canary:?} | received: {compact}"
        );
        assert!(
            !pretty.contains(canary),
            "expected {type_name} {{:#?}} to omit the {field} canary {canary:?} | received: {pretty}"
        );
    }
}

/// The debug form still says what kind of thing it is, so a log line is worth reading.
fn assert_names(type_name: &str, value: &dyn Debug, expected: &str) {
    let compact = format!("{value:?}");
    assert!(
        compact.contains(expected),
        "expected {type_name} {{:?}} to name {expected:?} | received: {compact}"
    );
}

fn command_params() -> serde_json::Value {
    json!({
        "threadId": "CANARY-thread", "turnId": "CANARY-turn", "itemId": "CANARY-item",
        "approvalId": "CANARY-approval", "command": "CANARY-command", "cwd": "CANARY-cwd",
        "reason": "CANARY-reason",
        "proposedExecpolicyAmendment": ["CANARY-prefix"],
        "proposedNetworkPolicyAmendments": [{"host": "CANARY-host", "action": "allow"}],
    })
}

const COMMAND_CANARIES: [(&str, &str); 8] = [
    ("thread_id", "CANARY-thread"),
    ("turn_id", "CANARY-turn"),
    ("item_id", "CANARY-item"),
    ("approval_id", "CANARY-approval"),
    ("command", "CANARY-command"),
    ("cwd", "CANARY-cwd"),
    ("reason", "CANARY-reason"),
    ("proposed_execpolicy_amendment", "CANARY-prefix"),
];

#[test]
fn a_command_approval_request_does_not_print_its_content() {
    let request = ServerRequest::parse(method::COMMAND_EXECUTION_APPROVAL, command_params());
    assert_omits("ServerRequest", &request, &COMMAND_CANARIES);
    assert_omits(
        "ServerRequest",
        &request,
        &[("proposed_network_policy_amendments", "CANARY-host")],
    );
    assert_names("ServerRequest", &request, "CommandExecution");
}

#[test]
fn the_command_approval_params_do_not_print_their_content() {
    let ServerRequest::CommandExecution(params) =
        ServerRequest::parse(method::COMMAND_EXECUTION_APPROVAL, command_params())
    else {
        panic!("expected a command approval to parse as one");
    };
    assert_omits("CommandExecutionApprovalParams", &params, &COMMAND_CANARIES);
    assert_omits(
        "CommandExecutionApprovalParams",
        &params,
        &[("proposed_network_policy_amendments", "CANARY-host")],
    );
    let default = CommandExecutionApprovalParams::default();
    assert_names(
        "CommandExecutionApprovalParams",
        &default,
        "CommandExecutionApprovalParams",
    );
}

#[test]
fn a_file_change_request_does_not_print_its_content() {
    let params = json!({
        "threadId": "CANARY-thread", "turnId": "CANARY-turn", "itemId": "CANARY-item",
        "reason": "CANARY-reason", "grantRoot": "CANARY-root",
    });
    let canaries = [
        ("thread_id", "CANARY-thread"),
        ("turn_id", "CANARY-turn"),
        ("item_id", "CANARY-item"),
        ("reason", "CANARY-reason"),
        ("grant_root", "CANARY-root"),
    ];
    let request = ServerRequest::parse(method::FILE_CHANGE_APPROVAL, params);
    assert_omits("ServerRequest", &request, &canaries);
    assert_names("ServerRequest", &request, "FileChange");
    let ServerRequest::FileChange(inner) = request else {
        panic!("expected a file change to parse as one");
    };
    assert_omits("FileChangeApprovalParams", &inner, &canaries);
    let _ = FileChangeApprovalParams::default();
}

#[test]
fn a_permissions_request_does_not_print_the_profile() {
    let params = json!({
        "threadId": "CANARY-thread", "turnId": "CANARY-turn", "itemId": "CANARY-item",
        "cwd": "CANARY-cwd", "reason": "CANARY-reason",
        "permissions": {"fileSystem": {"read": ["CANARY-profile"]}},
    });
    let canaries = [
        ("thread_id", "CANARY-thread"),
        ("turn_id", "CANARY-turn"),
        ("item_id", "CANARY-item"),
        ("cwd", "CANARY-cwd"),
        ("reason", "CANARY-reason"),
        ("permissions", "CANARY-profile"),
    ];
    let request = ServerRequest::parse(method::PERMISSIONS_APPROVAL, params);
    assert_omits("ServerRequest", &request, &canaries);
    assert_names("ServerRequest", &request, "Permissions");
    let ServerRequest::Permissions(inner) = request else {
        panic!("expected a permissions request to parse as one");
    };
    assert_omits("PermissionsRequestApprovalParams", &inner, &canaries);
    let _ = PermissionsRequestApprovalParams::default();
}

#[test]
fn a_question_round_does_not_print_its_questions() {
    let params = json!({
        "threadId": "CANARY-thread", "turnId": "CANARY-turn", "itemId": "CANARY-item",
        "questions": [{
            "id": "CANARY-question-id", "header": "CANARY-header", "question": "CANARY-question",
            "options": [{"label": "CANARY-label", "description": "CANARY-description"}],
        }],
    });
    let canaries = [
        ("thread_id", "CANARY-thread"),
        ("turn_id", "CANARY-turn"),
        ("item_id", "CANARY-item"),
        ("question.id", "CANARY-question-id"),
        ("question.header", "CANARY-header"),
        ("question.question", "CANARY-question"),
        ("option.label", "CANARY-label"),
        ("option.description", "CANARY-description"),
    ];
    let request = ServerRequest::parse(method::TOOL_REQUEST_USER_INPUT, params);
    assert_omits("ServerRequest", &request, &canaries);
    assert_names("ServerRequest", &request, "RequestUserInput");
    let ServerRequest::RequestUserInput(inner) = request else {
        panic!("expected a question round to parse as one");
    };
    assert_omits("ToolRequestUserInputParams", &inner, &canaries);
    assert_omits(
        "ToolRequestUserInputQuestion",
        &inner.questions[0],
        &canaries,
    );
    let option = &inner.questions[0]
        .options
        .as_ref()
        .expect("expected options")[0];
    assert_omits("ToolRequestUserInputOption", option, &canaries);
}

#[test]
fn an_elicitation_does_not_print_its_ids() {
    let params = json!({
        "threadId": "CANARY-thread", "turnId": "CANARY-turn", "elicitationId": "CANARY-elicitation",
    });
    let canaries = [
        ("thread_id", "CANARY-thread"),
        ("turn_id", "CANARY-turn"),
        ("elicitation_id", "CANARY-elicitation"),
    ];
    let request = ServerRequest::parse(method::MCP_ELICITATION, params);
    assert_omits("ServerRequest", &request, &canaries);
    assert_names("ServerRequest", &request, "McpElicitation");
    let ServerRequest::McpElicitation(inner) = request else {
        panic!("expected an elicitation to parse as one");
    };
    assert_omits("McpServerElicitationRequestParams", &inner, &canaries);
    let _ = McpServerElicitationRequestParams::default();
}

/// A method this harness knows is a library constant and reads as one; a method it does not know
/// is the server's text, so only its length is reported.
#[test]
fn a_refusal_names_a_known_method_and_hides_an_unknown_one() {
    let known = ServerRequest::parse(method::TOOL_CALL, json!({}));
    assert_names("ServerRequest", &known, method::TOOL_CALL);
    assert_names(
        "ServerRequest",
        &known,
        "VendorToolsNeverEnterTheHostRegistry",
    );

    let unknown = ServerRequest::parse("CANARY-method/unknown", json!({}));
    assert_omits("ServerRequest", &unknown, &[("method", "CANARY-method")]);
    assert_names("ServerRequest", &unknown, "Refused");
}

#[test]
fn an_amendment_decision_does_not_print_the_rule_it_carries() {
    let exec = ApprovalDecisionValue::AcceptWithExecpolicyAmendment(json!(["CANARY-prefix"]));
    let network = ApprovalDecisionValue::ApplyNetworkPolicyAmendment(
        json!({"host": "CANARY-host", "action": "allow"}),
    );
    assert_omits(
        "ApprovalDecisionValue",
        &exec,
        &[("execpolicy_amendment", "CANARY-prefix")],
    );
    assert_omits(
        "ApprovalDecisionValue",
        &network,
        &[("network_policy_amendment", "CANARY-host")],
    );
    assert_names(
        "ApprovalDecisionValue",
        &exec,
        "AcceptWithExecpolicyAmendment",
    );
    assert_names(
        "ApprovalDecisionValue",
        &network,
        "ApplyNetworkPolicyAmendment",
    );
    assert_names(
        "ApprovalDecisionValue",
        &ApprovalDecisionValue::Decline,
        "Decline",
    );

    let response = ApprovalResponse { decision: exec };
    assert_omits(
        "ApprovalResponse",
        &response,
        &[("decision", "CANARY-prefix")],
    );
}

#[test]
fn a_server_answer_does_not_print_an_amendment_or_a_profile() {
    let amendment = ServerAnswer::Approval(ApprovalDecisionValue::ApplyNetworkPolicyAmendment(
        json!({"host": "CANARY-host", "action": "deny"}),
    ));
    assert_omits(
        "ServerAnswer",
        &amendment,
        &[("network_policy_amendment", "CANARY-host")],
    );
    assert_names("ServerAnswer", &amendment, "ApplyNetworkPolicyAmendment");

    let profile = json!({"fileSystem": {"read": ["CANARY-profile"]}});
    let grant = ServerAnswer::Permissions {
        option_id: "grant:turn",
        response: PermissionsRequestApprovalResponse::grant(profile, PermissionGrantScope::Turn),
    };
    assert_omits("ServerAnswer", &grant, &[("permissions", "CANARY-profile")]);
    assert_names("ServerAnswer", &grant, "grant:turn");
    assert_names("ServerAnswer", &grant, "Turn");
    if let ServerAnswer::Permissions { response, .. } = &grant {
        assert_omits(
            "PermissionsRequestApprovalResponse",
            response,
            &[("permissions", "CANARY-profile")],
        );
    }
}

#[test]
fn a_question_answer_does_not_print_what_was_answered() {
    let mut response = ToolRequestUserInputResponse::none();
    response.answers.insert(
        String::from("CANARY-question-id"),
        ToolRequestUserInputAnswer {
            answers: vec![String::from("CANARY-answer")],
        },
    );
    let canaries = [
        ("answers key", "CANARY-question-id"),
        ("answers value", "CANARY-answer"),
    ];
    assert_omits("ToolRequestUserInputResponse", &response, &canaries);
    assert_omits(
        "ToolRequestUserInputAnswer",
        &response.answers["CANARY-question-id"],
        &canaries,
    );
}
