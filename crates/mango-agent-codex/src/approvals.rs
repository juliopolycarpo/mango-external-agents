//! One of the server's questions, as something a person can be asked, and their answer on its way
//! back.
//!
//! Nothing here decides. The options are the vendor's, spelled with the vendor's own ids, and the
//! only judgement this module makes is which neutral kind each one is — which is what lets a
//! host's policy answer "allow" without reading a label in a language it does not know.

use std::time::SystemTime;

use serde_json::Value;

use mango_external_agents::event::ActivityKind;
use mango_external_agents::interaction::{Interaction, InteractionId, InteractionKind};
use mango_external_agents::normalize::{self, TextLimit};
use mango_external_agents::operation::OperationRef;
use mango_external_agents::permission::{
    PermissionEffect, PermissionOption, PermissionRequest, PermissionRisk, PermissionScope,
};

use crate::protocol::approvals::{
    ApprovalDecisionValue, CommandExecutionApprovalParams, FileChangeApprovalParams,
    PermissionGrantScope, PermissionsRequestApprovalParams, PermissionsRequestApprovalResponse,
    ServerAnswer, ServerRequest,
};

/// One question, and the answers this harness will take for it.
///
/// The options are derived from the decision values the pinned schema declares, narrowed by the
/// two amendment fields the request either carries or does not. The server also writes an
/// undeclared `availableDecisions` list; `crate::protocol::approvals` says why it is not read.
#[derive(Clone, PartialEq)]
pub struct PendingApproval {
    /// What a host renders and a broker decides on.
    pub request: PermissionRequest,
    /// The wire value each option id answers with.
    decisions: Vec<(String, ServerAnswer)>,
    /// The answer that refuses without stopping the turn.
    refusal: ServerAnswer,
}

impl std::fmt::Debug for PendingApproval {
    /// The question's shape and the ids of the options it offers, never the command, reason, rule
    /// or profile behind them. The option ids are this harness's own (`accept`, `grant:turn`), so
    /// they are printed; the answers they map to are not, because an amendment answer carries the
    /// exact rule it writes.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let option_ids: Vec<&str> = self.decisions.iter().map(|(id, _)| id.as_str()).collect();
        formatter
            .debug_struct("PendingApproval")
            .field("request", &self.request)
            .field("option_ids", &option_ids)
            .field("refusal", &self.refusal)
            .finish()
    }
}

impl PendingApproval {
    /// The wire answer for one of this question's own option ids.
    ///
    /// `None` for an id this question never offered, which is what
    /// [`PermissionRequest::respond`] already refuses — checked again here because the answer to
    /// an unoffered id would otherwise be a decision nobody chose.
    #[must_use]
    pub fn decision_for(&self, option_id: &str) -> Option<ServerAnswer> {
        self.decisions
            .iter()
            .find(|(id, _)| id == option_id)
            .map(|(_, decision)| decision.clone())
    }

    /// The offered option carrying this id, for building an audit-worthy
    /// [`ApprovalDecision`](mango_external_agents::permission::ApprovalDecision) from what was
    /// really offered rather than from a bare id.
    #[must_use]
    pub fn option(&self, option_id: &str) -> Option<&PermissionOption> {
        self.request
            .options
            .iter()
            .find(|option| option.id == option_id)
    }

    /// The answer that refuses without stopping the turn.
    ///
    /// What a timed-out or cancelled question is resolved with: the turn goes on, and the agent is
    /// told no. For the two ordinary approval families this is `decline`, never `cancel` — `cancel`
    /// stops the turn, which is a different decision and not one a deadline gets to make. For a
    /// permissions grant it is a granted profile that grants nothing: `{"permissions": {}}`.
    #[must_use]
    pub fn refusal(&self) -> ServerAnswer {
        self.refusal.clone()
    }
}

/// The `networkApprovalContext` member of a raw command-approval frame.
///
/// Read from the raw params, beside the public [`CommandExecutionApprovalParams`], so that type
/// keeps the fields it has: adding a public field to it would break a struct literal built outside
/// the crate. `None` for another method, an absent member and an explicit `null`.
pub(crate) fn network_context(method: &str, params: &Value) -> Option<Value> {
    if method != crate::protocol::approvals::method::COMMAND_EXECUTION_APPROVAL {
        return None;
    }
    params
        .get("networkApprovalContext")
        .filter(|context| !context.is_null())
        .cloned()
}

/// One of the server's approvals, as a question with options.
///
/// `network_context` is [`network_context`] of the frame the request was parsed from; only a
/// command approval uses it.
///
/// `None` for a request this harness refuses; the caller answers those with a protocol error.
#[must_use]
pub(crate) fn to_request(
    request: &ServerRequest,
    network_context: Option<&Value>,
    operation: OperationRef,
    expires_at: SystemTime,
) -> Option<PendingApproval> {
    match request {
        ServerRequest::CommandExecution(params) => {
            Some(from_command(params, network_context, operation, expires_at))
        }
        ServerRequest::FileChange(params) => Some(from_file_change(params, operation, expires_at)),
        ServerRequest::Permissions(params) => Some(from_permissions(params, operation, expires_at)),
        ServerRequest::RequestUserInput(_) | ServerRequest::McpElicitation(_) => None,
        ServerRequest::Refused { .. } => None,
    }
}

fn from_command(
    params: &CommandExecutionApprovalParams,
    network_context: Option<&Value>,
    operation: OperationRef,
    expires_at: SystemTime,
) -> PendingApproval {
    let command = params.command.as_deref().unwrap_or("a command");
    let title = format!("Run {command}");

    let mut choices = base_choices();
    let mut unoffered: Vec<String> = Vec::new();
    // Offered only when the request carries the proposal, because the answer echoes the payload
    // back: an amendment option with nothing to send is an option that cannot be chosen. And only
    // when a person can read the whole rule: an option that writes a standing rule the host cannot
    // display in full is not one a person can consent to.
    if let Some(rule) = params
        .proposed_execpolicy_amendment
        .as_ref()
        .and_then(ExecRule::decode)
    {
        match rule.choice() {
            Some(choice) => choices.push(choice),
            None => unoffered.push(rule.text()),
        }
    }
    // Only the first network proposal the request lists is a candidate. See `docs/harness-codex.md`, Approvals: every
    // later one is named in the detail instead, so nothing is dropped without a trace.
    let proposals = params
        .proposed_network_policy_amendments
        .as_deref()
        .unwrap_or_default();
    for (index, rule) in proposals
        .iter()
        .enumerate()
        .filter_map(|(index, proposal)| Some((index, NetworkRule::decode(proposal)?)))
    {
        match rule.choice().filter(|_| index == 0) {
            Some(choice) => choices.push(choice),
            None => unoffered.push(rule.text()),
        }
    }

    // The requested host leads the detail, ahead of anything the agent wrote, so a long `reason`
    // cannot push it past the display bound.
    let mut lines: Vec<String> = Vec::with_capacity(4);
    if let Some(context) = network_context {
        lines.push(network_context_line(context));
    }
    lines.extend(unoffered_line(&unoffered));
    if let Some(reason) = params.reason.as_deref() {
        lines.push(reason.to_owned());
    }
    if let Some(cwd) = params.cwd.as_deref() {
        lines.push(format!("in {cwd}"));
    }
    let detail = (!lines.is_empty()).then(|| lines.join("\n\n"));

    build(
        params.approval_id.as_deref().unwrap_or(&params.item_id),
        operation,
        ActivityKind::Command,
        title,
        detail,
        choices,
        expires_at,
    )
}

/// The most proposals the detail names, so a long list cannot crowd out the agent's reason.
const UNOFFERED_LISTED: usize = 4;
/// The longest rule the detail spells out; a longer one is described, not quoted.
const UNOFFERED_RULE_CODE_POINTS: usize = 256;

/// The detail line naming proposals this prompt did not turn into an option.
fn unoffered_line(rules: &[String]) -> Option<String> {
    if rules.is_empty() {
        return None;
    }
    let mut line = String::from("Standing rules proposed but not offered as options:");
    for rule in rules.iter().take(UNOFFERED_LISTED) {
        if rule.chars().count() > UNOFFERED_RULE_CODE_POINTS {
            line.push_str("\n- a rule too long to show");
        } else {
            line.push_str("\n- ");
            line.push_str(rule);
        }
    }
    if rules.len() > UNOFFERED_LISTED {
        line.push_str(&format!("\n- and {} more", rules.len() - UNOFFERED_LISTED));
    }
    Some(line)
}

/// The values the pinned `NetworkApprovalProtocol` declares.
const NETWORK_PROTOCOLS: [&str; 4] = ["http", "https", "socks5Tcp", "socks5Udp"];

/// The host and protocol a managed-network approval asks about.
fn network_context_line(context: &Value) -> String {
    let field = |name: &str| context.get(name).and_then(Value::as_str);
    // The abbreviated form is for exactly the declared shape: `{host, protocol}` with a protocol
    // from the pinned enum. Anything wider is shown whole, so no member is hidden behind it.
    let declared = context
        .as_object()
        .is_some_and(|members| members.len() == 2);
    match (field("protocol"), field("host")) {
        (Some(protocol), Some(host)) if declared && NETWORK_PROTOCOLS.contains(&protocol) => {
            format!("Network access requested: {protocol} to {host}")
        }
        _ => format!("Network access requested: {context}"),
    }
}

/// One option of a question: the vendor decision, and the label that says what it grants.
///
/// The label is `None` for the four plain decisions, whose wording is fixed here, and carries the
/// exact rule for an amendment.
struct Choice {
    decision: ApprovalDecisionValue,
    label: Option<String>,
}

/// Whether the core would show this label whole. `normalized()` cuts a label at
/// [`TextLimit::ApprovalOptionLabel`] and strips control and bidirectional characters, so a label
/// that changes under the same bounding would tell a person less than the rule it names.
fn label_survives(label: &str) -> bool {
    !normalize::bound_text(label, TextLimit::ApprovalOptionLabel).truncated
}

/// An exec-policy proposal: the command prefix the vendor would stop asking about.
///
/// The pinned protocol declares `proposedExecpolicyAmendment` as an array of strings.
struct ExecRule(Vec<String>);

impl ExecRule {
    /// `None` for anything that is not a nonempty array of strings, which cannot be shown exactly.
    fn decode(value: &Value) -> Option<Self> {
        let words: Vec<String> = value
            .as_array()?
            .iter()
            .map(|word| word.as_str().map(str::to_owned))
            .collect::<Option<_>>()?;
        (!words.is_empty()).then_some(Self(words))
    }

    /// The prefix as compact JSON, because a space-joined prefix reads the same for `["a b"]` and
    /// `["a","b"]`.
    fn text(&self) -> String {
        Value::from(self.0.clone()).to_string()
    }

    fn choice(&self) -> Option<Choice> {
        let label = format!(
            "Allow, and always allow commands starting with {}",
            self.text()
        );
        label_survives(&label).then(|| Choice {
            decision: ApprovalDecisionValue::AcceptWithExecpolicyAmendment(Value::from(
                self.0.clone(),
            )),
            label: Some(label),
        })
    }
}

/// A network-policy proposal: allow or deny one host, from now on.
///
/// The pinned protocol declares `{host: string, action: "allow" | "deny"}`.
struct NetworkRule {
    host: String,
    action: &'static str,
}

impl NetworkRule {
    /// `None` for a shape the pin does not declare, which cannot be shown exactly.
    fn decode(value: &Value) -> Option<Self> {
        // Exactly the two declared members: the label names `{host, action}`, so a proposal that
        // carries more is one the label would misdescribe.
        if value.as_object()?.len() != 2 {
            return None;
        }
        let host = value
            .get("host")?
            .as_str()
            .filter(|host| !host.is_empty())?;
        let action = match value.get("action")?.as_str()? {
            "allow" => "allow",
            "deny" => "deny",
            _ => return None,
        };
        Some(Self {
            host: host.to_owned(),
            action,
        })
    }

    /// What the vendor proposes, in its own words: never "Allow" for a `deny`.
    fn text(&self) -> String {
        format!("{} {}", self.action, Value::from(self.host.as_str()))
    }

    fn choice(&self) -> Option<Choice> {
        // Deliberately silent on what an applied `deny` does to the request in front of the
        // person: the pinned protocol says only that the user "chose a persistent network policy
        // rule (allow/deny) for this host".
        let label = format!("Apply standing network rule: {}", self.text());
        label_survives(&label).then(|| Choice {
            decision: ApprovalDecisionValue::ApplyNetworkPolicyAmendment(
                serde_json::json!({"action": self.action, "host": self.host}),
            ),
            label: Some(label),
        })
    }
}

fn from_file_change(
    params: &FileChangeApprovalParams,
    operation: OperationRef,
    expires_at: SystemTime,
) -> PendingApproval {
    let title = match params.grant_root.as_deref() {
        Some(root) => format!("Write under {root}"),
        None => String::from("Apply file changes"),
    };
    build(
        &params.item_id,
        operation,
        ActivityKind::FileChange,
        title,
        params.reason.clone(),
        base_choices(),
        expires_at,
    )
}

/// The four decisions both approval families declare, in the order a prompt reads best.
fn base_choices() -> Vec<Choice> {
    [
        ApprovalDecisionValue::Accept,
        ApprovalDecisionValue::AcceptForSession,
        ApprovalDecisionValue::Decline,
        ApprovalDecisionValue::Cancel,
    ]
    .into_iter()
    .map(|decision| Choice {
        decision,
        label: None,
    })
    .collect()
}

fn build(
    id: &str,
    operation: OperationRef,
    kind: ActivityKind,
    title: String,
    detail: Option<String>,
    choices: Vec<Choice>,
    expires_at: SystemTime,
) -> PendingApproval {
    let options: Vec<PermissionOption> = choices
        .iter()
        .map(|choice| option_for(&choice.decision, choice.label.as_deref()))
        .collect();
    let decisions: Vec<(String, ServerAnswer)> = choices
        .into_iter()
        .map(|choice| {
            (
                choice.decision.option_id().to_owned(),
                ServerAnswer::Approval(choice.decision),
            )
        })
        .collect();
    // The question, as the vendor names it: its own callback id where it has one, and the item it
    // gates otherwise. Not the JSON-RPC request id, which names the frame rather than the thing
    // being asked about — a host that stored one could not line it up with anything it had
    // already been told.
    //
    // The distinction matters where one command raises two questions. Upstream says several
    // callbacks can share a parent item, so keying by the item would have the second question
    // overwrite the first: the first host to answer would be answering for both, and the waiter it
    // displaced would sit out the deadline and decline a question somebody had already allowed.
    let interaction = Interaction::new(
        InteractionId::new(id),
        InteractionKind::Permission,
        operation.session_id.clone(),
        expires_at,
    )
    .during(operation);
    let request = PermissionRequest::new(interaction, kind, title, options);
    let request = match detail {
        Some(detail) => request.with_detail(detail),
        None => request,
    };
    PendingApproval {
        request,
        decisions,
        refusal: ServerAnswer::Approval(ApprovalDecisionValue::Decline),
    }
}

/// The vendor asks whether the client will grant this permission profile.
///
/// A completely displayable profile offers a turn grant, a session grant, or a denial. Other
/// profiles offer only denial. A grant echoes the requested profile unchanged.
fn from_permissions(
    params: &PermissionsRequestApprovalParams,
    operation: OperationRef,
    expires_at: SystemTime,
) -> PendingApproval {
    // A grant is available only if the complete profile survives the host's display bounds.
    // Check the same normalization used by PermissionRequest, including character stripping.
    // Reasons and cwd follow the profile so their truncation cannot conceal granted authority.
    let profile = format!("Grants {}", params.permissions);
    let profile_visible = !normalize::bound_text(&profile, TextLimit::Detail).truncated;
    let title = if profile_visible {
        "Grant the requested permissions"
    } else {
        "Deny permissions that cannot be displayed completely"
    };
    let mut lines: Vec<String> = Vec::with_capacity(3);
    lines.push(profile);
    if let Some(reason) = params.reason.as_deref() {
        lines.push(reason.to_owned());
    }
    if let Some(cwd) = params.cwd.as_deref() {
        lines.push(format!("in {cwd}"));
    }
    let detail = (!lines.is_empty()).then(|| lines.join("\n\n"));

    let mut options = vec![
        PermissionOption::new("grant:turn", PermissionEffect::Allow)
            .with_label("Grant for this turn")
            .with_scope(PermissionScope::Turn),
        PermissionOption::new("grant:session", PermissionEffect::Allow)
            .with_label("Grant for this session")
            .with_scope(PermissionScope::Session),
        PermissionOption::new("deny", PermissionEffect::Reject)
            .with_label("Deny")
            .with_scope(PermissionScope::Once),
    ];
    let mut decisions: Vec<(String, ServerAnswer)> = vec![
        (
            String::from("grant:turn"),
            ServerAnswer::Permissions {
                option_id: "grant:turn",
                response: PermissionsRequestApprovalResponse::grant(
                    params.permissions.clone(),
                    PermissionGrantScope::Turn,
                ),
            },
        ),
        (
            String::from("grant:session"),
            ServerAnswer::Permissions {
                option_id: "grant:session",
                response: PermissionsRequestApprovalResponse::grant(
                    params.permissions.clone(),
                    PermissionGrantScope::Session,
                ),
            },
        ),
        (
            String::from("deny"),
            ServerAnswer::Permissions {
                option_id: "deny",
                response: PermissionsRequestApprovalResponse::deny(),
            },
        ),
    ];
    let refusal = decisions[2].1.clone();
    if !profile_visible {
        options.retain(|option| option.effect == PermissionEffect::Reject);
        decisions.retain(|(id, _)| id == "deny");
    }

    // Named by the item it gates, the same convention `build` uses for the two ordinary approval
    // families — not the JSON-RPC request id, which names the frame rather than the thing asked.
    let interaction = Interaction::new(
        InteractionId::new(&params.item_id),
        InteractionKind::Permission,
        operation.session_id.clone(),
        expires_at,
    )
    .during(operation);
    // `ActivityKind::Other`: a permissions grant is not tied to one command or file change, so
    // none of the other kinds describe it any better.
    let request = PermissionRequest::new(interaction, ActivityKind::Other, title, options);
    let request = match detail {
        Some(detail) => request.with_detail(detail),
        None => request,
    };
    PendingApproval {
        request,
        decisions,
        refusal,
    }
}

/// What one vendor decision means, in the neutral vocabulary a policy can answer in.
///
/// `rule` is the exact standing rule an amendment writes, already worded; the plain decisions
/// ignore it.
fn option_for(decision: &ApprovalDecisionValue, rule: Option<&str>) -> PermissionOption {
    match decision {
        ApprovalDecisionValue::Accept => {
            PermissionOption::new(decision.option_id(), PermissionEffect::Allow)
                .with_label("Allow once")
                .with_scope(PermissionScope::Once)
        }
        ApprovalDecisionValue::AcceptForSession => {
            PermissionOption::new(decision.option_id(), PermissionEffect::Allow)
                .with_label("Allow for this session")
                .with_scope(PermissionScope::Session)
        }
        ApprovalDecisionValue::Decline => {
            PermissionOption::new(decision.option_id(), PermissionEffect::Reject)
                .with_label("Deny")
                .with_scope(PermissionScope::Once)
        }
        // Not `Reject`. The core's contract for that effect is "refuse this one thing; the turn
        // goes on", and `cancel` stops the turn — so a broker answering `deny()` must never be
        // handed this one, and a person choosing it must know it is different. Codex does not say
        // how far a cancel reaches, so no scope is claimed for it either.
        ApprovalDecisionValue::Cancel => {
            PermissionOption::new(decision.option_id(), PermissionEffect::Other)
                .with_label("Deny and stop the turn")
                .with_risk(PermissionRisk::Destructive)
        }
        // Both amendments genuinely write a standing rule the vendor applies to later requests on
        // its own — an execpolicy or network policy amendment, not a one-off grant — so they are
        // marked `policy_changing` rather than left to guess at a scope Codex never states.
        ApprovalDecisionValue::AcceptWithExecpolicyAmendment(_) => {
            PermissionOption::new(decision.option_id(), PermissionEffect::Other)
                .with_label(rule.unwrap_or("Allow, and always allow commands like this"))
                .with_risk(PermissionRisk::Destructive)
                .policy_changing()
        }
        ApprovalDecisionValue::ApplyNetworkPolicyAmendment(_) => {
            PermissionOption::new(decision.option_id(), PermissionEffect::Other)
                .with_label(rule.unwrap_or("Apply the proposed standing network rule"))
                .with_risk(PermissionRisk::Destructive)
                .policy_changing()
        }
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
            None,
            operation(),
            mango_external_agents::Limits::default()
                .approval_expires_at(std::time::SystemTime::UNIX_EPOCH)
                .expect("default approval deadline"),
        )
        .expect("expected a question a person can be asked")
    }

    use super::to_request as build_request;
    use crate::protocol::approvals::{ApprovalDecisionValue, ServerRequest, method};
    use mango_external_agents::event::{ActivityKind, SessionId, TurnId};
    use mango_external_agents::operation::{AttemptId, OperationRef};
    use mango_external_agents::permission::PermissionEffect;
    use serde_json::json;
    use std::time::{Duration, SystemTime};

    /// The session, turn and attempt a captured server request is answered under.
    fn operation() -> OperationRef {
        OperationRef::new(
            SessionId::new("chat-1"),
            TurnId::new("turn-1"),
            AttemptId::default(),
        )
    }

    fn to_request(request: &ServerRequest, now: SystemTime) -> Option<super::PendingApproval> {
        build_request(
            request,
            None,
            operation(),
            mango_external_agents::Limits::default()
                .approval_expires_at(now)
                .ok()?,
        )
    }

    /// A command approval built the way the session builds it: the raw frame supplies the network
    /// context, and the parsed request supplies everything else.
    fn command_pending(extra: serde_json::Value) -> super::PendingApproval {
        let mut params = json!({
            "threadId": "thread-1", "turnId": "turn-1", "itemId": "exec-1",
            "startedAtMs": 1_u64, "command": "curl example.com", "cwd": "/workspace",
        });
        if let (Some(base), Some(extra)) = (params.as_object_mut(), extra.as_object()) {
            base.extend(extra.clone());
        }
        let context = super::network_context(method::COMMAND_EXECUTION_APPROVAL, &params);
        let request = ServerRequest::parse(method::COMMAND_EXECUTION_APPROVAL, params);
        build_request(
            &request,
            context.as_ref(),
            operation(),
            mango_external_agents::Limits::default()
                .approval_expires_at(now())
                .expect("default approval deadline"),
        )
        .expect("expected a question")
    }

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_789_283_381)
    }

    /// The last instant this platform's `SystemTime` can represent after the Unix epoch.
    fn latest_system_time() -> SystemTime {
        let mut seconds = 0_u64;
        for bit in (0..u64::BITS).rev() {
            let candidate = seconds | (1_u64 << bit);
            if SystemTime::UNIX_EPOCH
                .checked_add(Duration::from_secs(candidate))
                .is_some()
            {
                seconds = candidate;
            }
        }
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
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
            one.request.id(),
            two.request.id(),
            "expected two callbacks on one item to be told apart, received {} twice",
            one.request.id()
        );
    }

    /// And where the vendor raises no callback of its own, the item is the question.
    #[test]
    fn a_command_with_no_callback_of_its_own_is_named_by_the_item_it_gates() {
        let asked = command_approval("exec-1", None);
        assert_eq!(asked.request.id().as_str(), "exec-1");
    }

    #[test]
    fn a_command_approval_is_a_question_about_the_activity_a_host_already_shows() {
        let pending = to_request(&command_request(json!({})), now()).expect("expected a question");

        assert_eq!(pending.request.id().as_str(), "exec-ee0f9baa");
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
        assert_eq!(
            pending.request.expires_at(),
            mango_external_agents::Limits::default()
                .approval_expires_at(now())
                .expect("default approval deadline")
        );
    }

    #[test]
    fn a_host_clock_that_cannot_represent_an_approval_expiry_is_refused_without_panicking() {
        let result = std::panic::catch_unwind(|| {
            to_request(&command_request(json!({})), latest_system_time())
        });

        assert!(
            result.is_ok(),
            "expected an unrepresentable host-clock deadline to be refused without panicking"
        );
        assert_eq!(
            result.expect("the panic check above passed"),
            None,
            "expected an unrepresentable host-clock deadline to be refused"
        );
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
        assert_eq!(cancel.effect, PermissionEffect::Other);
        assert!(cancel.is_destructive());

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
            Some(super::ServerAnswer::Approval(
                ApprovalDecisionValue::AcceptWithExecpolicyAmendment(json!([
                    "/bin/bash",
                    "-lc",
                    "printf 'mango' > mango.txt"
                ]))
            )),
            "expected the request's own payload to travel back with the answer"
        );
    }

    #[test]
    fn a_network_amendment_carries_the_first_proposal_the_request_made() {
        let with = to_request(
            &command_request(json!({
                "proposedNetworkPolicyAmendments": [{"host": "example.com", "action": "allow"}]
            })),
            now(),
        )
        .expect("expected a question");

        assert_eq!(
            with.decision_for("applyNetworkPolicyAmendment"),
            Some(super::ServerAnswer::Approval(
                ApprovalDecisionValue::ApplyNetworkPolicyAmendment(
                    json!({"host": "example.com", "action": "allow"})
                )
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
            Some(super::ServerAnswer::Approval(
                ApprovalDecisionValue::Decline
            ))
        );
    }

    /// The offered option behind an id, for building an audit decision from what was really
    /// offered rather than from a bare string nobody can trace back to its reach or its risk.
    #[test]
    fn the_offered_option_behind_an_id_carries_its_own_reach_and_risk() {
        let pending = to_request(&command_request(json!({})), now()).expect("expected a question");

        let accept = pending
            .option("accept")
            .expect("expected the accept option");
        assert_eq!(accept.effect, PermissionEffect::Allow);

        assert!(
            pending.option("acceptEverythingForever").is_none(),
            "expected no option for an id nobody offered"
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
                "proposedNetworkPolicyAmendments": [{"host": "example.com", "action": "allow"}]
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
        assert_eq!(bounded.id().as_str(), "exec-ee0f9baa");
    }

    /// A permissions grant offers exactly three options, and each echoes the request's own
    /// profile back rather than inventing, widening or narrowing one.
    #[test]
    fn a_permissions_grant_offers_three_options_that_echo_the_requested_profile() {
        let request = ServerRequest::parse(
            method::PERMISSIONS_APPROVAL,
            json!({
                "threadId": "t", "turnId": "u", "itemId": "perm-1",
                "cwd": "/workspace", "reason": "needs filesystem access",
                "permissions": {"fs": {"read": true}},
            }),
        );
        let pending = to_request(&request, now()).expect("expected a question");

        assert_eq!(pending.request.id().as_str(), "perm-1");
        assert_eq!(pending.request.options.len(), 3);
        assert!(
            pending
                .request
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("/workspace")),
            "expected the working directory in the detail, received {:?}",
            pending.request.detail
        );

        let turn = pending
            .decision_for("grant:turn")
            .expect("expected a turn-scoped grant");
        assert_eq!(
            turn,
            super::ServerAnswer::Permissions {
                option_id: "grant:turn",
                response: crate::protocol::approvals::PermissionsRequestApprovalResponse::grant(
                    json!({"fs": {"read": true}}),
                    crate::protocol::approvals::PermissionGrantScope::Turn,
                ),
            }
        );

        let session = pending
            .decision_for("grant:session")
            .expect("expected a session-scoped grant");
        assert_eq!(
            session,
            super::ServerAnswer::Permissions {
                option_id: "grant:session",
                response: crate::protocol::approvals::PermissionsRequestApprovalResponse::grant(
                    json!({"fs": {"read": true}}),
                    crate::protocol::approvals::PermissionGrantScope::Session,
                ),
            }
        );

        assert_eq!(
            pending.refusal(),
            pending
                .decision_for("deny")
                .expect("expected a deny option")
        );
        assert_eq!(pending.refusal().to_wire(), json!({"permissions": {}}));
    }

    /// The profile a grant hands over leads the detail, so bounding cannot hide it.
    ///
    /// A detail is cut from its end. With the profile behind the agent's own words, an agent that
    /// writes a long enough `reason` decides what a host sees of the authority it is about to be
    /// offered — which is the concealment this ordering exists to stop.
    #[test]
    fn a_permissions_detail_leads_with_the_profile_however_long_the_reason_is() {
        let request = ServerRequest::parse(
            method::PERMISSIONS_APPROVAL,
            json!({
                "threadId": "t", "turnId": "u", "itemId": "perm-1",
                "cwd": "/workspace", "reason": "y".repeat(8_192),
                "permissions": {"fs": {"read": true}},
            }),
        );
        let pending = to_request(&request, now()).expect("expected a question");
        let detail = pending
            .request
            .detail
            .as_deref()
            .expect("expected a detail carrying the requested profile");
        assert!(
            detail.starts_with(r#"Grants {"fs":{"read":true}}"#),
            "expected the profile at the head of the detail, received {:?}",
            &detail[..detail.len().min(64)]
        );

        let bounded = pending
            .request
            .normalized()
            .expect("expected the request to bound");
        let bounded_detail = bounded
            .detail
            .as_deref()
            .expect("expected the bounded detail to survive");
        assert!(
            bounded_detail.contains(r#"{"fs":{"read":true}}"#),
            "expected the profile to survive bounding, received {:?}",
            &bounded_detail[..bounded_detail.len().min(64)]
        );
        assert!(
            bounded.truncated,
            "expected the over-long reason to be reported as cut"
        );
    }

    #[test]
    fn a_permissions_profile_that_cannot_be_displayed_completely_offers_only_denial() {
        for path in [
            "a".repeat(4_096),
            String::from("/workspace/\u{202e}private"),
        ] {
            let request = ServerRequest::parse(
                method::PERMISSIONS_APPROVAL,
                json!({
                    "threadId": "t", "turnId": "u", "itemId": "perm-1",
                    "permissions": {
                        "fileSystem": {"read": [path]},
                        "network": {"enabled": true},
                    },
                }),
            );
            let pending = to_request(&request, now()).expect("expected a denial prompt");
            let bounded = pending
                .request
                .clone()
                .normalized()
                .expect("bounded prompt");
            assert!(
                bounded.truncated,
                "expected the profile display to be changed"
            );
            assert_eq!(
                bounded
                    .options
                    .iter()
                    .map(|option| option.id.as_str())
                    .collect::<Vec<_>>(),
                ["deny"],
                "expected denial only when normalization hides part of the granted authority"
            );
            assert!(pending.decision_for("grant:turn").is_none());
            assert!(pending.decision_for("grant:session").is_none());
            assert!(
                bounded.allow().is_err(),
                "expected a broker grant to be refused"
            );
            assert!(
                bounded.deny().is_ok(),
                "expected the denial to remain usable"
            );
            assert_eq!(pending.refusal().to_wire(), json!({"permissions": {}}));
        }
    }

    #[test]
    fn a_permissions_profile_at_the_display_limit_still_offers_grants() {
        let empty = json!({"fileSystem": {"read": [""]}, "network": {"enabled": true}});
        let overhead = format!("Grants {empty}").chars().count();
        let request = ServerRequest::parse(
            method::PERMISSIONS_APPROVAL,
            json!({
                "threadId": "t", "turnId": "u", "itemId": "perm-1",
                "reason": "r".repeat(8_192),
                "permissions": {
                    "fileSystem": {"read": ["é".repeat(
                        mango_external_agents::normalize::TextLimit::Detail.max_code_points() - overhead
                    )]},
                    "network": {"enabled": true},
                },
            }),
        );
        let pending = to_request(&request, now()).expect("expected a permission prompt");
        assert!(pending.decision_for("grant:turn").is_some());
        assert!(pending.decision_for("grant:session").is_some());
        let bounded = pending.request.normalized().expect("bounded prompt");
        assert!(bounded.truncated, "expected the reason to be removed");
        assert!(
            bounded
                .detail
                .expect("profile detail")
                .ends_with(r#""network":{"enabled":true}}"#)
        );
    }

    /// `deny` is offered as an ordinary refusal, not the standing-rule vocabulary the two ordinary
    /// approval families reserve for `cancel`.
    #[test]
    fn a_permissions_denial_is_an_ordinary_refusal() {
        let request = ServerRequest::parse(
            method::PERMISSIONS_APPROVAL,
            json!({"threadId": "t", "turnId": "u", "itemId": "perm-1", "permissions": {}}),
        );
        let pending = to_request(&request, now()).expect("expected a question");
        let deny = pending
            .option("deny")
            .expect("expected a deny option among the three offered");
        assert_eq!(deny.effect, PermissionEffect::Reject);
    }

    /// The label of one offered option, or a failure naming what was offered instead.
    fn label_of(pending: &super::PendingApproval, id: &str) -> String {
        pending
            .request
            .options
            .iter()
            .find(|option| option.id == id)
            .and_then(|option| option.label.clone())
            .unwrap_or_else(|| {
                panic!(
                    "expected an offered option {id:?} with a label | received: {:?}",
                    pending.request.options
                )
            })
    }

    fn offers(pending: &super::PendingApproval, id: &str) -> bool {
        pending.request.options.iter().any(|option| option.id == id)
    }

    fn detail_of(pending: &super::PendingApproval) -> String {
        pending.request.detail.clone().unwrap_or_default()
    }

    /// The host cannot consent to a standing rule it cannot read: the label carries the exact
    /// prefix, and survives the core's own bounding unchanged.
    #[test]
    fn an_exec_amendment_option_names_the_exact_prefix_it_writes() {
        let pending = to_request(
            &command_request(json!({"proposedExecpolicyAmendment": ["git", "status"]})),
            now(),
        )
        .expect("expected a question");

        let label = label_of(&pending, "acceptWithExecpolicyAmendment");
        assert!(
            label.contains(r#"["git","status"]"#),
            "expected the exact command prefix in the label | received: {label:?}"
        );
        let bounded = pending.request.normalized().expect("expected bounding");
        assert!(
            !bounded.truncated,
            "expected the label to survive core bounding whole | received: {:?}",
            bounded.options
        );
    }

    /// A space-joined prefix would read the same for one argument and for two.
    #[test]
    fn two_prefixes_that_differ_only_in_argument_boundaries_are_labelled_differently() {
        let one = to_request(
            &command_request(json!({"proposedExecpolicyAmendment": ["rm -rf", "x"]})),
            now(),
        )
        .expect("expected a question");
        let two = to_request(
            &command_request(json!({"proposedExecpolicyAmendment": ["rm", "-rf", "x"]})),
            now(),
        )
        .expect("expected a question");
        assert_ne!(
            label_of(&one, "acceptWithExecpolicyAmendment"),
            label_of(&two, "acceptWithExecpolicyAmendment"),
            "expected the argument boundaries to be visible in the label"
        );
    }

    #[test]
    fn a_network_amendment_option_names_the_host_and_the_action() {
        let pending = to_request(
            &command_request(json!({
                "proposedNetworkPolicyAmendments": [
                    {"host": "hidden-policy-domain.example", "action": "allow"}
                ]
            })),
            now(),
        )
        .expect("expected a question");

        let label = label_of(&pending, "applyNetworkPolicyAmendment");
        assert!(
            label.contains("hidden-policy-domain.example") && label.contains("allow"),
            "expected the host and the action in the label | received: {label:?}"
        );
        assert_eq!(
            pending.decision_for("applyNetworkPolicyAmendment"),
            Some(super::ServerAnswer::Approval(
                ApprovalDecisionValue::ApplyNetworkPolicyAmendment(
                    json!({"action": "allow", "host": "hidden-policy-domain.example"})
                )
            )),
            "expected the labelled rule to be the one sent back"
        );
    }

    #[test]
    fn a_deny_amendment_is_never_labelled_as_an_allow() {
        let pending = to_request(
            &command_request(json!({
                "proposedNetworkPolicyAmendments": [{"host": "blocked.example", "action": "deny"}]
            })),
            now(),
        )
        .expect("expected a question");

        let label = label_of(&pending, "applyNetworkPolicyAmendment");
        assert!(
            label.contains("deny") && label.contains("blocked.example"),
            "expected the deny action and the host in the label | received: {label:?}"
        );
        assert!(
            !label.to_lowercase().contains("allow"),
            "expected no allow wording on a deny rule | received: {label:?}"
        );
    }

    /// An option whose rule the host cannot see whole is an option nobody can consent to.
    #[test]
    fn an_amendment_too_long_to_display_whole_is_not_offered() {
        let long_prefix = json!(["curl", "a".repeat(200)]);
        let long_host = "h".repeat(200);
        let pending = to_request(
            &command_request(json!({
                "proposedExecpolicyAmendment": long_prefix,
                "proposedNetworkPolicyAmendments": [{"host": long_host, "action": "allow"}]
            })),
            now(),
        )
        .expect("expected a question");

        assert!(
            !offers(&pending, "acceptWithExecpolicyAmendment")
                && !offers(&pending, "applyNetworkPolicyAmendment"),
            "expected neither oversized amendment to be offered | received: {:?}",
            pending.request.options
        );
        assert!(
            pending
                .decision_for("acceptWithExecpolicyAmendment")
                .is_none()
                && pending
                    .decision_for("applyNetworkPolicyAmendment")
                    .is_none(),
            "expected no wire answer for an option that was not offered"
        );
        // The four plain choices remain, and the request still bounds.
        assert_eq!(pending.request.options.len(), 4);
        pending.request.normalized().expect("expected bounding");
    }

    #[test]
    fn an_amendment_whose_text_the_core_would_strip_is_not_offered() {
        let pending = to_request(
            &command_request(json!({
                "proposedNetworkPolicyAmendments": [
                    {"host": "safe.example\u{202e}evil", "action": "allow"}
                ]
            })),
            now(),
        )
        .expect("expected a question");
        assert!(
            !offers(&pending, "applyNetworkPolicyAmendment"),
            "expected a bidirectional control in the host to refuse the option | received: {:?}",
            pending.request.options
        );
    }

    /// The pin declares these shapes; anything else cannot be labelled exactly.
    #[test]
    fn a_proposal_outside_the_declared_shape_is_not_offered() {
        let pending = to_request(
            &command_request(json!({
                "proposedExecpolicyAmendment": ["git", 1],
                "proposedNetworkPolicyAmendments": [{"host": "example.com", "allow": true}]
            })),
            now(),
        )
        .expect("expected a question");
        assert_eq!(
            pending.request.options.len(),
            4,
            "expected only the four plain options | received: {:?}",
            pending.request.options
        );

        for bad in [json!("git status"), json!([]), json!({"prefix": ["git"]})] {
            let pending = to_request(
                &command_request(json!({"proposedExecpolicyAmendment": bad})),
                now(),
            )
            .expect("expected a question");
            assert!(
                !offers(&pending, "acceptWithExecpolicyAmendment"),
                "expected {bad} not to be offered as a prefix"
            );
        }
        let pending = to_request(
            &command_request(json!({
                "proposedNetworkPolicyAmendments": [{"host": "example.com", "action": "maybe"}]
            })),
            now(),
        )
        .expect("expected a question");
        assert!(
            !offers(&pending, "applyNetworkPolicyAmendment"),
            "expected an action outside allow|deny not to be offered"
        );
    }

    /// Deliberate: only the first network proposal is an option, because every network option
    /// shares one id and the broker audit path resolves an option by that id. Later proposals are
    /// named in the detail, not dropped without a trace.
    #[test]
    fn later_network_proposals_are_named_in_the_detail_not_offered() {
        let pending = to_request(
            &command_request(json!({
                "proposedNetworkPolicyAmendments": [
                    {"host": "first.example", "action": "allow"},
                    {"host": "second.example", "action": "deny"}
                ]
            })),
            now(),
        )
        .expect("expected a question");

        let label = label_of(&pending, "applyNetworkPolicyAmendment");
        assert!(
            label.contains("first.example"),
            "expected the first proposal to be the option | received: {label:?}"
        );
        assert_eq!(
            pending
                .request
                .options
                .iter()
                .filter(|option| option.id == "applyNetworkPolicyAmendment")
                .count(),
            1,
            "expected exactly one network amendment option"
        );
        let detail = detail_of(&pending);
        assert!(
            detail.contains("not offered") && detail.contains(r#"deny "second.example""#),
            "expected the later proposal to be named in the detail | received: {detail:?}"
        );
        assert!(
            !detail.contains("first.example"),
            "expected the offered proposal not to be repeated as unoffered | received: {detail:?}"
        );
    }

    /// "First" is the request's own first entry: a malformed lead proposal does not promote the one
    /// behind it, whose place in the vendor's ordering says it is not the preferred rule.
    #[test]
    fn a_malformed_first_proposal_does_not_promote_the_one_behind_it() {
        let pending = to_request(
            &command_request(json!({
                "proposedNetworkPolicyAmendments": [
                    {"host": "first.example", "action": "maybe"},
                    {"host": "second.example", "action": "allow"}
                ]
            })),
            now(),
        )
        .expect("expected a question");
        assert!(
            !offers(&pending, "applyNetworkPolicyAmendment"),
            "expected no network option when the first listed proposal is malformed | received: {:?}",
            pending.request.options
        );
        assert!(
            detail_of(&pending).contains(r#"allow "second.example""#),
            "expected the well-formed later proposal to be named | received: {:?}",
            detail_of(&pending)
        );
    }

    #[test]
    fn a_proposal_too_long_to_be_an_option_is_named_in_the_detail() {
        let pending = to_request(
            &command_request(json!({
                "proposedNetworkPolicyAmendments": [{"host": "h".repeat(200), "action": "allow"}]
            })),
            now(),
        )
        .expect("expected a question");
        assert!(
            !offers(&pending, "applyNetworkPolicyAmendment"),
            "expected the oversized proposal not to be an option"
        );
        let detail = detail_of(&pending);
        assert!(
            detail.contains(&"h".repeat(200)),
            "expected the whole rule in the detail | received: {detail:?}"
        );
    }

    #[test]
    fn a_managed_network_approval_shows_the_host_it_asks_about() {
        let pending = command_pending(json!({
            "networkApprovalContext": {"host": "api.example.com", "protocol": "https"},
            "reason": "y".repeat(8_192),
        }));

        let bounded = pending.request.normalized().expect("expected bounding");
        let detail = bounded.detail.unwrap_or_default();
        assert!(
            detail.starts_with("Network access requested: https to api.example.com"),
            "expected the requested host at the head of the bounded detail | received: {:?}",
            &detail[..detail.len().min(96)]
        );
    }

    /// A rule with a member the pin does not declare cannot be shown as received: the label would
    /// name `{host, action}` while the vendor holds more.
    #[test]
    fn a_network_proposal_with_an_undeclared_member_is_not_offered() {
        let pending = to_request(
            &command_request(json!({
                "proposedNetworkPolicyAmendments": [
                    {"host": "example.com", "action": "allow", "port": 8080}
                ]
            })),
            now(),
        )
        .expect("expected a question");
        assert!(
            !offers(&pending, "applyNetworkPolicyAmendment"),
            "expected a member outside {{host, action}} to refuse the option | received: {:?}",
            pending.request.options
        );
    }

    /// The abbreviated rendering is for the declared shape only; anything wider is shown whole.
    #[test]
    fn a_network_context_wider_than_the_declared_shape_is_shown_as_it_arrived() {
        for context in [
            json!({"host": "example.com", "protocol": "https", "port": 8443}),
            json!({"host": "example.com", "protocol": "ftp"}),
        ] {
            let pending = command_pending(json!({"networkApprovalContext": context}));
            let detail = detail_of(&pending);
            assert!(
                detail.contains(&context.to_string()),
                "expected the whole context in the detail | received: {detail:?}"
            );
        }
    }

    /// A host that debug-logs a question must not log the command, the reason, the working
    /// directory, or the standing rule an amendment option would write: the pending approval holds
    /// the wire answers, which carry the amendment payloads.
    #[test]
    fn a_pending_approval_does_not_print_the_content_it_holds() {
        let pending = to_request(
            &command_request(json!({
                "command": "CANARY-command", "cwd": "CANARY-cwd", "reason": "CANARY-reason",
                "proposedExecpolicyAmendment": ["CANARY-prefix"],
                "proposedNetworkPolicyAmendments": [{"host": "CANARY-host", "action": "allow"}],
            })),
            now(),
        )
        .expect("expected a question");
        assert!(
            pending
                .decision_for("acceptWithExecpolicyAmendment")
                .is_some(),
            "expected the amendments to be offered, so the canaries are really held"
        );
        for (form, text) in [
            ("{:?}", format!("{pending:?}")),
            ("{:#?}", format!("{pending:#?}")),
        ] {
            for (field, canary) in [
                ("command", "CANARY-command"),
                ("cwd", "CANARY-cwd"),
                ("reason", "CANARY-reason"),
                ("execpolicy amendment", "CANARY-prefix"),
                ("network amendment host", "CANARY-host"),
            ] {
                assert!(
                    !text.contains(canary),
                    "expected PendingApproval {form} to omit the {field} canary {canary:?} | received: {text}"
                );
            }
        }
        let text = format!("{pending:?}");
        for named in [
            "PendingApproval",
            "acceptWithExecpolicyAmendment",
            "decline",
        ] {
            assert!(
                text.contains(named),
                "expected PendingApproval {{:?}} to name {named:?} | received: {text}"
            );
        }
    }

    /// The context is read from the raw frame and only for a command approval; an absent member and
    /// an explicit `null` are the same thing.
    #[test]
    fn the_network_context_is_read_from_a_command_frame_only() {
        let with = json!({"networkApprovalContext": {"host": "a.example", "protocol": "http"}});
        assert_eq!(
            super::network_context(method::COMMAND_EXECUTION_APPROVAL, &with),
            Some(json!({"host": "a.example", "protocol": "http"})),
            "expected the declared member of a command frame to be read"
        );
        assert_eq!(
            super::network_context(method::FILE_CHANGE_APPROVAL, &with),
            None,
            "expected no context from a file-change frame"
        );
        for absent in [json!({}), json!({"networkApprovalContext": null})] {
            assert_eq!(
                super::network_context(method::COMMAND_EXECUTION_APPROVAL, &absent),
                None,
                "expected {absent} to carry no context"
            );
        }
    }

    /// Whatever the network context looks like, a person is shown it rather than nothing.
    #[test]
    fn a_network_context_of_an_undeclared_shape_is_shown_as_it_arrived() {
        let pending = command_pending(json!({
            "networkApprovalContext": {"target": "api.example.com"}
        }));
        let detail = detail_of(&pending);
        assert!(
            detail.contains(r#"{"target":"api.example.com"}"#),
            "expected the raw context in the detail | received: {detail:?}"
        );
    }

    /// Naming the rule changes nothing about how a refusal, a broker or the audit reads the
    /// options: the refusal stays `decline`, an amendment is a policy-changing `Other`, and nothing
    /// selects one for a broker.
    #[test]
    fn labelling_an_amendment_leaves_the_refusal_and_the_option_classes_unchanged() {
        let pending = to_request(
            &command_request(json!({
                "proposedExecpolicyAmendment": ["git", "status"],
                "proposedNetworkPolicyAmendments": [{"host": "example.com", "action": "allow"}]
            })),
            now(),
        )
        .expect("expected a question");

        assert_eq!(
            pending.refusal(),
            super::ServerAnswer::Approval(ApprovalDecisionValue::Decline)
        );
        for id in [
            "acceptWithExecpolicyAmendment",
            "applyNetworkPolicyAmendment",
        ] {
            let option = pending.option(id).expect("expected an amendment option");
            assert_eq!(option.effect, PermissionEffect::Other, "{id}");
            assert!(option.is_destructive(), "{id} expected destructive");
            assert!(option.policy_changing, "{id} expected policy_changing");
        }
        assert_eq!(
            pending.request.allow().expect("expected a grant").option_id,
            "accept",
            "expected a broker's allow never to select a standing rule"
        );
        assert_eq!(
            pending
                .request
                .deny()
                .expect("expected a refusal")
                .option_id,
            "decline"
        );
    }

    /// How often a realistic prefix keeps its option. The numbers in `docs/harness-codex.md` are
    /// produced by this table; a change to the label wording that flips a row should be a
    /// conscious edit to both.
    #[test]
    fn realistic_prefixes_mostly_keep_their_option() {
        let survives = |prefix: serde_json::Value| {
            let pending = to_request(
                &command_request(json!({"proposedExecpolicyAmendment": prefix})),
                now(),
            )
            .expect("expected a question");
            offers(&pending, "acceptWithExecpolicyAmendment")
        };
        let kept = [
            json!(["git", "status"]),
            json!(["cargo", "test", "--workspace"]),
            json!(["rm", "-rf", "/tmp/mango"]),
            json!(["/usr/bin/python3", "-m", "pytest", "tests/unit"]),
            json!(["curl", "-s", "https://api.example.com/v1/status"]),
            json!([
                "sed",
                "-n",
                "1,200p",
                "crates/mango-agent-codex/src/approvals.rs"
            ]),
        ];
        let dropped = [
            json!([
                "/bin/bash",
                "-lc",
                "cd /workspace && cargo test --workspace --all-features -- --nocapture 2>&1 | tail -n 80"
            ]),
            json!([
                "curl",
                "-sS",
                "-X",
                "POST",
                "-H",
                "Content-Type: application/json",
                "-d",
                "{\"query\":\"select 1\"}",
                "https://api.example.com/v1/query"
            ]),
        ];
        for prefix in kept {
            assert!(
                survives(prefix.clone()),
                "expected a short prefix to keep its option | prefix: {prefix}"
            );
        }
        for prefix in dropped {
            assert!(
                !survives(prefix.clone()),
                "expected a long prefix to lose its option | prefix: {prefix}"
            );
        }
    }
}
