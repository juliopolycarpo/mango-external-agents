//! Schema-shaped account responses over the captured handshake; not vendor account captures.

use super::{Transcript, last_model_page, profiles_answer, replay_limits, workspace_path};
use mango_agent_codex::CodexHarness;
use mango_agent_codex::account::AccountFingerprintKey;
use mango_external_agents::testing::{FakeLauncher, FakeProcess};
use mango_external_agents::{AuthMode, AuthState, EnvSource, ExitStatus, HostContext};
use serde_json::json;
use std::sync::Arc;

/// Named fake process traffic for a 0.166.0 account response, without a login or real account.
fn account_kind_host(kind: &'static str, requires_auth: bool) -> (HostContext, Arc<FakeLauncher>) {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(
        FakeProcess::transcript(["codex-cli 0.166.0"]).with_exit(ExitStatus {
            code: Some(0),
            signal: None,
        }),
    );
    launcher.push(
        Transcript::load("handshake").as_process_intercepting(move |frame| {
            if frame["method"] == "account/read" {
                return Some(vec![json!({"id": frame["id"], "result": {
                "account": {"type": kind, "email": "account-canary@example.invalid",
                    "workspaceName": "workspace-canary"}, "requiresOpenaiAuth": requires_auth
            }}).to_string()]);
            }
            profiles_answer(frame, &json!([])).or_else(|| last_model_page(frame))
        }),
    );
    let host = HostContext::builder()
        .launcher(launcher.clone())
        .cwd(workspace_path())
        .client_info("account-kind-test", "0.0.0")
        .environment(EnvSource::empty())
        .limits(replay_limits())
        .build()
        .expect("expected an account-kind fake host");
    (host, launcher)
}

#[tokio::test]
async fn discovery_accepts_gcp_and_keeps_future_account_auth_unknown() {
    for (kind, requires_auth, expected) in [
        (
            "gcp",
            false,
            AuthState::LoggedIn {
                mode: AuthMode::Other(String::from("gcp")),
            },
        ),
        ("futureCloud", false, AuthState::Unknown),
        ("futureCloud", true, AuthState::Unknown),
    ] {
        let (host, launcher) = account_kind_host(kind, requires_auth);
        let key = AccountFingerprintKey::new(b"host-local-test-key").expect("test key");
        let found = CodexHarness::new()
            .discover_with_account(&host, &key)
            .await
            .expect("expected account decoding not to lose discovery");
        assert_eq!(found.discovery.auth, expected, "account kind {kind}");
        assert!(
            found.account.is_none(),
            "expected no ChatGPT identity for {kind}"
        );
        assert!(!format!("{found:?}").contains("account-canary"));
        assert!(!format!("{found:?}").contains("workspace-canary"));
        assert_eq!(launcher.live_children(), 0);
        assert_eq!(
            launcher.launches().len(),
            2,
            "version probe plus app-server"
        );
        assert!(
            !launcher
                .written()
                .iter()
                .any(|line| line.contains("account/login"))
        );
    }
}
