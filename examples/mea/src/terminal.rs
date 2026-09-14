//! The smoke host asks the person at its terminal before granting a permission.

use std::io::{IsTerminal, Write};

use mango_external_agents::{BrokerDecision, PermissionBroker, PermissionRequest};

pub(crate) struct TerminalBroker;

#[async_trait::async_trait]
impl PermissionBroker for TerminalBroker {
    async fn decide(&self, request: &PermissionRequest) -> BrokerDecision {
        if !std::io::stdin().is_terminal() {
            return decision("");
        }
        let summary = request.title.clone();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        // A detached input thread cannot keep the async runtime alive after turn expiry.
        std::thread::spawn(move || {
            eprint!("{summary}\nAllow once? [y/N] ");
            let _ = std::io::stderr().flush();
            let mut answer = String::new();
            let _ = std::io::stdin().read_line(&mut answer);
            let _ = sender.send(decision(&answer));
        });
        receiver.await.unwrap_or_else(|_| decision(""))
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
    use super::*;
    use mango_external_agents::{ActivityKind, PermissionOption, PermissionOptionKind};

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
        let request = PermissionRequest {
            id: "approval-1".into(),
            kind: ActivityKind::Command,
            title: "write a file".into(),
            detail: None,
            expires_at: std::time::SystemTime::now(),
            truncated: false,
            options: vec![PermissionOption::new(
                "allow",
                PermissionOptionKind::AllowOnce,
            )],
        };
        assert!(matches!(
            TerminalBroker.decide(&request).await,
            BrokerDecision::Deny { .. }
        ));
    }
}
