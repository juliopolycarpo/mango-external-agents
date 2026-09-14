//! One of the server's questions, as something a person can be asked, and their answer on its way
//! back.
//!
//! Nothing here decides. The options are the vendor's, spelled with the vendor's own ids, and the
//! only judgement this module makes is which neutral kind each one is — which is what lets a
//! host's policy answer "allow" without reading a label in a language it does not know.

use std::time::{Duration, SystemTime};

use mango_external_agents::event::ActivityKind;
use mango_external_agents::permission::{
    PermissionOption, PermissionOptionKind, PermissionRequest,
};

use crate::protocol::approvals::{
    ApprovalDecisionValue, CommandExecutionApprovalParams, FileChangeApprovalParams, ServerRequest,
};

/// One question, and the answers this harness will take for it.
///
/// The options are derived from the decision values the pinned schema declares, narrowed by the
/// two amendment fields the request either carries or does not. The server also writes an
/// undeclared `availableDecisions` list; `crate::protocol::approvals` says why it is not read.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingApproval {
    /// What a host renders and a broker decides on.
    pub request: PermissionRequest,
    /// The wire value each option id answers with.
    decisions: Vec<(String, ApprovalDecisionValue)>,
}

impl PendingApproval {
    /// The wire answer for one of this question's own option ids.
    ///
    /// `None` for an id this question never offered, which is what
    /// [`PermissionRequest::respond`] already refuses — checked again here because the answer to
    /// an unoffered id would otherwise be a decision nobody chose.
    #[must_use]
    pub fn decision_for(&self, option_id: &str) -> Option<ApprovalDecisionValue> {
        self.decisions
            .iter()
            .find(|(id, _)| id == option_id)
            .map(|(_, decision)| decision.clone())
    }

    /// The answer that refuses without stopping the turn.
    ///
    /// What a timed-out or cancelled question is resolved with: the turn goes on, and the agent is
    /// told no. `cancel` would stop the turn, which is a different decision and not one a deadline
    /// gets to make.
    #[must_use]
    pub fn refusal(&self) -> ApprovalDecisionValue {
        ApprovalDecisionValue::Decline
    }
}

/// One of the server's approvals, as a question with options.
///
/// `None` for a request this harness refuses; the caller answers those with a protocol error.
#[must_use]
pub(crate) fn to_request(
    request: &ServerRequest,
    now: SystemTime,
    approval_timeout: Duration,
) -> Option<PendingApproval> {
    match request {
        ServerRequest::CommandExecution(params) => {
            Some(from_command(params, now, approval_timeout))
        }
        ServerRequest::FileChange(params) => Some(from_file_change(params, now, approval_timeout)),
        ServerRequest::Refused { .. } => None,
    }
}

fn from_command(
    params: &CommandExecutionApprovalParams,
    now: SystemTime,
    approval_timeout: Duration,
) -> PendingApproval {
    let command = params.command.as_deref().unwrap_or("a command");
    let title = format!("Run {command}");
    let detail = match (params.reason.as_deref(), params.cwd.as_deref()) {
        (Some(reason), Some(cwd)) => Some(format!("{reason}\n\nin {cwd}")),
        (Some(reason), None) => Some(reason.to_owned()),
        (None, Some(cwd)) => Some(format!("in {cwd}")),
        (None, None) => None,
    };

    let mut decisions = base_decisions();
    // Offered only when the request carries the proposal, because the answer echoes the payload
    // back: an amendment option with nothing to send is an option that cannot be chosen.
    if let Some(amendment) = params.proposed_execpolicy_amendment.clone() {
        decisions.push(ApprovalDecisionValue::AcceptWithExecpolicyAmendment(
            amendment,
        ));
    }
    if let Some(amendment) = params
        .proposed_network_policy_amendments
        .as_ref()
        .and_then(|amendments| amendments.first())
        .cloned()
    {
        decisions.push(ApprovalDecisionValue::ApplyNetworkPolicyAmendment(
            amendment,
        ));
    }

    build(
        params.approval_id.as_deref().unwrap_or(&params.item_id),
        ActivityKind::Command,
        title,
        detail,
        decisions,
        now,
        approval_timeout,
    )
}

fn from_file_change(
    params: &FileChangeApprovalParams,
    now: SystemTime,
    approval_timeout: Duration,
) -> PendingApproval {
    let title = match params.grant_root.as_deref() {
        Some(root) => format!("Write under {root}"),
        None => String::from("Apply file changes"),
    };
    build(
        &params.item_id,
        ActivityKind::FileChange,
        title,
        params.reason.clone(),
        base_decisions(),
        now,
        approval_timeout,
    )
}

/// The four decisions both approval families declare, in the order a prompt reads best.
fn base_decisions() -> Vec<ApprovalDecisionValue> {
    vec![
        ApprovalDecisionValue::Accept,
        ApprovalDecisionValue::AcceptForSession,
        ApprovalDecisionValue::Decline,
        ApprovalDecisionValue::Cancel,
    ]
}

fn build(
    id: &str,
    kind: ActivityKind,
    title: String,
    detail: Option<String>,
    decisions: Vec<ApprovalDecisionValue>,
    now: SystemTime,
    approval_timeout: Duration,
) -> PendingApproval {
    let options = decisions.iter().map(option_for).collect();
    let decisions = decisions
        .into_iter()
        .map(|decision| (decision.option_id().to_owned(), decision))
        .collect();
    PendingApproval {
        request: PermissionRequest {
            // The question, as the vendor names it: its own callback id where it has one, and the
            // item it gates otherwise. Not the JSON-RPC request id, which names the frame rather
            // than the thing being asked about — a host that stored one could not line it up with
            // anything it had already been told.
            //
            // The distinction matters where one command raises two questions. Upstream says
            // several callbacks can share a parent item, so keying by the item would have the
            // second question overwrite the first: the first host to answer would be answering
            // for both, and the waiter it displaced would sit out the deadline and decline a
            // question somebody had already allowed.
            id: id.to_owned(),
            kind,
            title,
            detail,
            options,
            expires_at: now + approval_timeout,
            truncated: false,
        },
        decisions,
    }
}

/// What one vendor decision means, in the neutral vocabulary a policy can answer in.
fn option_for(decision: &ApprovalDecisionValue) -> PermissionOption {
    let (kind, label, destructive) = match decision {
        ApprovalDecisionValue::Accept => (PermissionOptionKind::AllowOnce, "Allow once", false),
        ApprovalDecisionValue::AcceptForSession => (
            PermissionOptionKind::AllowAlways,
            "Allow for this session",
            false,
        ),
        ApprovalDecisionValue::Decline => (PermissionOptionKind::RejectOnce, "Deny", false),
        // Not RejectOnce. The core's contract for that kind is "refuse this one thing; the turn
        // goes on", and `cancel` stops the turn — so a broker answering `deny()` must never be
        // handed this one, and a person choosing it must know it is different.
        ApprovalDecisionValue::Cancel => {
            (PermissionOptionKind::Other, "Deny and stop the turn", true)
        }
        ApprovalDecisionValue::AcceptWithExecpolicyAmendment(_) => (
            PermissionOptionKind::Other,
            "Allow, and stop asking for commands like this",
            true,
        ),
        ApprovalDecisionValue::ApplyNetworkPolicyAmendment(_) => (
            PermissionOptionKind::Other,
            "Allow, and apply the proposed network rule",
            true,
        ),
    };
    let option = PermissionOption::new(decision.option_id(), kind).with_label(label);
    if destructive {
        option.destructive()
    } else {
        option
    }
}

#[cfg(test)]
mod tests {
    /// One command-execution approval as the server writes it, with or without a callback id.
    fn command_approval(item_id: &str, approval_id: Option<&str>) -> super::PendingApproval {
        let request = crate::protocol::approvals::ServerRequest::parse(
            crate::protocol::approvals::method::COMMAND_EXECUTION_APPROVAL,
            serde_json::json!({
                "threadId": "t",
                "turnId": "u",
                "itemId": item_id,
                "approvalId": approval_id,
                "kind": "shell",
                "startedAtMs": 1_u64,
                "command": "rm -rf /tmp/mango",
            }),
        );
        super::to_request(
            &request,
            std::time::SystemTime::UNIX_EPOCH,
            approval_timeout(),
        )
        .expect("expected a question a person can be asked")
    }

    use super::to_request as build_request;
    use crate::protocol::approvals::{ApprovalDecisionValue, ServerRequest, method};
    use mango_external_agents::event::ActivityKind;
    use mango_external_agents::permission::PermissionOptionKind;
    use serde_json::json;
    use std::time::{Duration, SystemTime};

    fn approval_timeout() -> Duration {
        mango_external_agents::Limits::default().approval_timeout
    }

    fn to_request(request: &ServerRequest, now: SystemTime) -> Option<super::PendingApproval> {
        build_request(request, now, approval_timeout())
    }

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_789_283_381)
    }

    fn command_request(extra: serde_json::Value) -> ServerRequest {
        let mut params = json!({
            "threadId": "thread-1",
            "turnId": "turn-1",
            "itemId": "exec-ee0f9baa",
            "startedAtMs": 1_789_283_381_284u64,
            "reason": "Allow creating mango.txt outside the read-only sandbox?",
            "command": "/bin/bash -lc \"printf 'mango' > mango.txt\"",
            "cwd": "/workspace"
        });
        if let (Some(base), Some(extra)) = (params.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                base.insert(key.clone(), value.clone());
            }
        }
        ServerRequest::parse(method::COMMAND_EXECUTION_APPROVAL, params)
    }

    /// A question names the item it gates, not the JSON-RPC call that carried it: the host already
    /// has that item on screen as an activity.
    /// Upstream declares a callback id precisely because several can share a parent item. Keying
    /// by the item would let the second question overwrite the first, so the first answer would
    /// settle both and the displaced waiter would sit out the deadline — declining something
    /// somebody had already allowed.
    #[test]
    fn two_questions_about_one_command_stay_two_questions() {
        let one = command_approval("exec-1", Some("callback-a"));
        let two = command_approval("exec-1", Some("callback-b"));
        assert_ne!(
            one.request.id, two.request.id,
            "expected two callbacks on one item to be told apart, received {} twice",
            one.request.id
        );
    }

    /// And where the vendor raises no callback of its own, the item is the question.
    #[test]
    fn a_command_with_no_callback_of_its_own_is_named_by_the_item_it_gates() {
        let asked = command_approval("exec-1", None);
        assert_eq!(asked.request.id, "exec-1");
    }

    #[test]
    fn a_command_approval_is_a_question_about_the_activity_a_host_already_shows() {
        let pending = to_request(&command_request(json!({})), now()).expect("expected a question");

        assert_eq!(pending.request.id, "exec-ee0f9baa");
        assert_eq!(pending.request.kind, ActivityKind::Command);
        assert!(
            pending.request.title.contains("printf 'mango'"),
            "expected the command in the title, received {:?}",
            pending.request.title
        );
        assert!(
            pending
                .request
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("/workspace")),
            "expected the working directory in the detail, received {:?}",
            pending.request.detail
        );
        assert_eq!(pending.request.expires_at, now() + approval_timeout());
    }

    /// `cancel` aborts the turn. The core's RejectOnce means the turn goes on, so mapping it there
    /// would let a broker's `deny()` stop a turn it only meant to refuse one command in.
    #[test]
    fn stopping_the_turn_is_not_offered_as_an_ordinary_refusal() {
        let pending = to_request(&command_request(json!({})), now()).expect("expected a question");

        let cancel = pending
            .request
            .options
            .iter()
            .find(|option| option.id == "cancel")
            .expect("expected a cancel option");
        assert_eq!(cancel.kind, PermissionOptionKind::Other);
        assert!(cancel.destructive);

        // And the narrow refusal a broker reaches for is still there, and still ordinary.
        let deny = pending.request.deny().expect("expected a refusal");
        assert_eq!(deny.option_id, "decline");
    }

    /// The narrow grant wins over the standing one, which is the core's rule and worth proving
    /// against this harness's own option order.
    #[test]
    fn allowing_prefers_the_one_time_grant_over_the_standing_one() {
        let pending = to_request(&command_request(json!({})), now()).expect("expected a question");
        let allow = pending.request.allow().expect("expected a grant");
        assert_eq!(allow.option_id, "accept");
    }

    /// An amendment option echoes the request's own payload back. Offering one when the request
    /// carried no proposal would be an option that cannot be answered.
    #[test]
    fn an_amendment_is_offered_only_when_the_request_proposed_one() {
        let without = to_request(&command_request(json!({})), now()).expect("expected a question");
        assert!(
            without
                .decision_for("acceptWithExecpolicyAmendment")
                .is_none(),
            "expected no amendment option, received {:?}",
            without.request.options
        );

        let amendment = json!(["/bin/bash", "-lc", "printf 'mango' > mango.txt"]);
        let with = to_request(
            &command_request(json!({"proposedExecpolicyAmendment": amendment})),
            now(),
        )
        .expect("expected a question");

        assert_eq!(
            with.decision_for("acceptWithExecpolicyAmendment"),
            Some(ApprovalDecisionValue::AcceptWithExecpolicyAmendment(json!(
                ["/bin/bash", "-lc", "printf 'mango' > mango.txt"]
            ))),
            "expected the request's own payload to travel back with the answer"
        );
    }

    #[test]
    fn a_network_amendment_carries_the_first_proposal_the_request_made() {
        let with = to_request(
            &command_request(json!({
                "proposedNetworkPolicyAmendments": [{"host": "example.com", "allow": true}]
            })),
            now(),
        )
        .expect("expected a question");

        assert_eq!(
            with.decision_for("applyNetworkPolicyAmendment"),
            Some(ApprovalDecisionValue::ApplyNetworkPolicyAmendment(
                json!({"host": "example.com", "allow": true})
            ))
        );
    }

    #[test]
    fn a_file_change_approval_names_the_root_it_asks_for() {
        let request = ServerRequest::parse(
            method::FILE_CHANGE_APPROVAL,
            json!({
                "threadId": "thread-1", "turnId": "turn-1", "itemId": "patch-1",
                "startedAtMs": 1u64, "reason": "needs to write outside the workspace",
                "grantRoot": "/etc"
            }),
        );
        let pending = to_request(&request, now()).expect("expected a question");

        assert_eq!(pending.request.kind, ActivityKind::FileChange);
        assert!(
            pending.request.title.contains("/etc"),
            "expected the root in the title, received {:?}",
            pending.request.title
        );
    }

    /// A refused family is not a question. The caller answers it with a protocol error instead.
    #[test]
    fn a_request_this_harness_refuses_produces_no_question_to_ask() {
        let request = ServerRequest::parse(method::TOOL_CALL, json!({}));
        assert_eq!(to_request(&request, now()), None);
    }

    /// An id this question never offered has no wire answer, whatever the caller believed.
    #[test]
    fn an_option_this_question_never_offered_has_no_answer() {
        let pending = to_request(&command_request(json!({})), now()).expect("expected a question");
        assert_eq!(pending.decision_for("acceptEverythingForever"), None);
        assert_eq!(
            pending.decision_for("decline"),
            Some(ApprovalDecisionValue::Decline)
        );
    }

    /// Every option this harness offers must survive the core's own bounding, or the request is
    /// refused on its way to the host and the turn waits on a prompt nobody sees.
    #[test]
    fn every_question_this_harness_builds_survives_the_cores_bounding() {
        let amendment = json!(["/bin/bash", "-lc", "x"]);
        let pending = to_request(
            &command_request(json!({
                "proposedExecpolicyAmendment": amendment,
                "proposedNetworkPolicyAmendments": [{"host": "example.com"}]
            })),
            now(),
        )
        .expect("expected a question");

        let bounded = pending
            .request
            .clone()
            .normalized()
            .expect("expected the question to survive bounding");
        assert_eq!(bounded.options.len(), 6);
        assert_eq!(bounded.id, "exec-ee0f9baa");
    }
}
