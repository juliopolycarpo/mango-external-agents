//! The smoke host asks the person at its terminal before granting a permission.

use mango_external_agents::{
    BrokerDecision, PermissionBroker, PermissionEffect, PermissionRequest,
};

pub(crate) struct TerminalBroker {
    // A trait object so a test can state whether anyone is at the keyboard; the real
    // `TerminalInput` reads process state (`stdin().is_terminal()`) that a test cannot set.
    input: Box<dyn crate::ask::QuestionInput>,
}

impl TerminalBroker {
    pub(crate) fn new(input: impl crate::ask::QuestionInput + 'static) -> Self {
        Self {
            input: Box::new(input),
        }
    }
}

impl Default for TerminalBroker {
    fn default() -> Self {
        Self::new(crate::ask::TerminalInput::new())
    }
}

#[async_trait::async_trait]
impl PermissionBroker for TerminalBroker {
    async fn decide(&self, request: &PermissionRequest) -> BrokerDecision {
        let Some(answer) = self.input.read_line(&render_prompt(request)).await else {
            return decision_for(request, "");
        };
        decision_for(request, &answer)
    }
}

/// The title and prompt a person must see before they choose a permission option.
fn render_prompt(request: &PermissionRequest) -> String {
    let detail = request
        .detail
        .as_deref()
        .map_or(String::new(), |detail| format!("\n{detail}"));
    format!("{}{detail}\n{}", request.title, allow_prompt(request))
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
        (false, _) => String::from("Only refusal is available. Press Enter to continue. "),
    }
}

/// Turns a typed line into an approval only when the request offers an allowing option.
fn decision_for(request: &PermissionRequest, answer: &str) -> BrokerDecision {
    if request
        .options
        .iter()
        .any(|option| option.effect == PermissionEffect::Allow)
    {
        return decision(answer);
    }
    decision("")
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
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::ask::QuestionInput;
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

    /// A keyboard whose answer is fixed by the test: `None` is redirected input (nobody there to
    /// type), `Some(line)` is a person at a terminal typing `line`. Records the prompts it saw.
    struct FakeKeyboard {
        typed: Option<&'static str>,
        prompts: Arc<Mutex<Vec<String>>>,
    }

    impl FakeKeyboard {
        fn new(typed: Option<&'static str>) -> (Self, Arc<Mutex<Vec<String>>>) {
            let prompts = Arc::new(Mutex::new(Vec::new()));
            let keyboard = Self {
                typed,
                prompts: Arc::clone(&prompts),
            };
            (keyboard, prompts)
        }
    }

    #[async_trait::async_trait]
    impl QuestionInput for FakeKeyboard {
        async fn read_line(&self, prompt: &str) -> Option<String> {
            self.prompts
                .lock()
                .expect("expected an unpoisoned prompt log")
                .push(prompt.to_owned());
            self.typed.map(str::to_owned)
        }
    }

    fn once_allow_request() -> PermissionRequest {
        permission_request(vec![
            PermissionOption::new("allow", PermissionEffect::Allow)
                .with_scope(PermissionScope::Once),
        ])
    }

    #[tokio::test]
    async fn redirected_input_never_grants_permission() {
        let (keyboard, prompts) = FakeKeyboard::new(None);
        let decision = TerminalBroker::new(keyboard)
            .decide(&once_allow_request())
            .await;
        assert!(
            matches!(decision, BrokerDecision::Deny { .. }),
            "expected Deny with redirected input | received {decision:?}"
        );
        assert_eq!(
            prompts.lock().expect("expected prompt log").len(),
            1,
            "expected the broker to ask the input exactly once"
        );
    }

    /// A terminal-looking input still grants only on an explicit yes: a bare Enter or an
    /// unrecognised word is a refusal, and the same broker does grant on "yes".
    #[tokio::test]
    async fn a_person_at_the_terminal_grants_only_by_typing_yes() {
        for typed in ["", "\n", "no", "perhaps", "true"] {
            let (keyboard, _) = FakeKeyboard::new(Some(typed));
            let decision = TerminalBroker::new(keyboard)
                .decide(&once_allow_request())
                .await;
            assert!(
                matches!(decision, BrokerDecision::Deny { .. }),
                "expected Deny for typed {typed:?} | received {decision:?}"
            );
        }
        let (keyboard, _) = FakeKeyboard::new(Some("yes\n"));
        let decision = TerminalBroker::new(keyboard)
            .decide(&once_allow_request())
            .await;
        assert_eq!(decision, BrokerDecision::Allow);
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
    fn a_permission_prompt_includes_the_detail_that_defines_its_authority() {
        let request = permission_request(vec![
            PermissionOption::new("allow", PermissionEffect::Allow)
                .with_scope(PermissionScope::Once),
        ])
        .with_detail("read files: [/workspace/src]\nnetwork: enabled");

        let prompt = render_prompt(&request);
        assert!(
            prompt.contains("read files: [/workspace/src]\nnetwork: enabled"),
            "expected the full permission detail in the prompt, received {prompt:?}"
        );
    }

    #[test]
    fn no_allow_option_explains_that_only_refusal_is_available() {
        let request = permission_request(vec![PermissionOption::new(
            "reject",
            PermissionEffect::Reject,
        )]);
        assert_eq!(
            allow_prompt(&request),
            "Only refusal is available. Press Enter to continue. "
        );
        assert!(
            matches!(decision_for(&request, "yes"), BrokerDecision::Deny { .. }),
            "a typed yes must not become an approval when no allow option exists"
        );
    }
}
