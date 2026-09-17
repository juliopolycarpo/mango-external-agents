//! The smoke host asks the person at its terminal before granting a permission.

use std::io::{IsTerminal, Write};

use mango_external_agents::{
    BrokerDecision, PermissionBroker, PermissionEffect, PermissionRequest,
};

pub(crate) struct TerminalBroker;

#[async_trait::async_trait]
impl PermissionBroker for TerminalBroker {
    async fn decide(&self, request: &PermissionRequest) -> BrokerDecision {
        if !std::io::stdin().is_terminal() {
            return decision("");
        }
        let summary = request.title.clone();
        let prompt = allow_prompt(request);
        let (sender, receiver) = tokio::sync::oneshot::channel();
        // A detached input thread cannot keep the async runtime alive after turn expiry.
        std::thread::spawn(move || {
            eprint!("{summary}\n{prompt}");
            let _ = std::io::stderr().flush();
            let mut answer = String::new();
            let _ = std::io::stdin().read_line(&mut answer);
            let _ = sender.send(decision(&answer));
        });
        receiver.await.unwrap_or_else(|_| decision(""))
    }
}

/// What a "y" would actually grant, for the question printed above the prompt.
///
/// A "y" here is routed through [`PermissionRequest::allow`], which always prefers the narrowest
/// option on offer. Printing a hardcoded "once" regardless of what the vendor offered would
/// understate a standing grant: an option that is `policy_changing`, or whose scope the vendor
/// never stated, is a reach `is_standing` refuses to call narrow, and this prompt must not call it
/// "once" either. Only when at least one allowing option is stated, non-standing and
/// non-policy-changing does a "y" definitely mean "just this once".
fn allow_prompt(request: &PermissionRequest) -> String {
    let mut offers_allow = false;
    let mut offers_a_narrow_allow = false;
    for option in request
        .options
        .iter()
        .filter(|option| option.effect == PermissionEffect::Allow)
    {
        offers_allow = true;
        if !option.is_standing() {
            offers_a_narrow_allow = true;
        }
    }
    match (offers_allow, offers_a_narrow_allow) {
        (true, true) => String::from("Allow once? [y/N] "),
        (true, false) => String::from("Allow — this applies beyond this one request. [y/N] "),
        (false, _) => String::from("Allow? [y/N] "),
    }
}

fn decision(answer: &str) -> BrokerDecision {
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        return BrokerDecision::Allow;
    }
    BrokerDecision::Deny {
        reason: "No explicit terminal approval".into(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;
    use mango_external_agents::{
        ActivityKind, Interaction, InteractionId, InteractionKind, PermissionEffect,
        PermissionOption, PermissionScope, SessionId,
    };

    #[test]
    fn only_an_explicit_yes_grants_permission() {
        for answer in ["y", "YES\n", " yes "] {
            assert_eq!(decision(answer), BrokerDecision::Allow);
        }
        for answer in ["", "no", "perhaps", "true"] {
            assert!(matches!(decision(answer), BrokerDecision::Deny { .. }));
        }
    }

    #[tokio::test]
    async fn redirected_input_never_grants_permission() {
        if std::io::stdin().is_terminal() {
            return;
        }
        let request = permission_request(vec![
            PermissionOption::new("allow", PermissionEffect::Allow)
                .with_scope(PermissionScope::Once),
        ]);
        assert!(matches!(
            TerminalBroker.decide(&request).await,
            BrokerDecision::Deny { .. }
        ));
    }

    /// A single request with `options`, for a test that only cares about the options offered.
    fn permission_request(options: Vec<PermissionOption>) -> PermissionRequest {
        let interaction = Interaction::new(
            InteractionId::new("approval-1"),
            InteractionKind::Permission,
            SessionId::new("session-1"),
            SystemTime::now() + Duration::from_secs(60),
        );
        PermissionRequest::new(interaction, ActivityKind::Command, "write a file", options)
    }

    #[test]
    fn a_stated_once_only_allow_is_the_only_one_rendered_as_once() {
        let request = permission_request(vec![
            PermissionOption::new("allow", PermissionEffect::Allow)
                .with_scope(PermissionScope::Once),
        ]);
        assert_eq!(allow_prompt(&request), "Allow once? [y/N] ");
    }

    #[test]
    fn an_unstated_or_policy_changing_allow_is_never_rendered_as_once() {
        for option in [
            // The vendor did not say how far this reaches.
            PermissionOption::new("allow", PermissionEffect::Allow),
            // Stated narrow, but writes a standing rule regardless of its scope.
            PermissionOption::new("allow", PermissionEffect::Allow)
                .with_scope(PermissionScope::Once)
                .policy_changing(),
            // Stated, and wider than once on its own.
            PermissionOption::new("allow", PermissionEffect::Allow)
                .with_scope(PermissionScope::Session),
        ] {
            let request = permission_request(vec![option]);
            let prompt = allow_prompt(&request);
            assert!(
                !prompt.contains("once"),
                "expected a standing reach, received {prompt:?}"
            );
        }
    }

    #[test]
    fn no_allow_option_renders_a_generic_question() {
        let request = permission_request(vec![PermissionOption::new(
            "reject",
            PermissionEffect::Reject,
        )]);
        assert_eq!(allow_prompt(&request), "Allow? [y/N] ");
    }
}
