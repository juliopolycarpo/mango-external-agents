//! Registry harnesses over named fakes. These tests require no installed vendor or product host.

use std::sync::Arc;
use std::time::Duration;

use mango_agent_acp::testing::{Approval, FakeAcpAgent};
use mango_agent_acp::{AcpHarness, AcpProfile};
use mango_external_agents::testing::{FakeLauncher, FakeProcess};
use mango_external_agents::{
    ByteSink, CancelToken, EnvSource, EventKind, Harness, HostContext, LaunchSpec, ManagedProcess,
    ProcessLauncher, Result, VendorInfo,
};
use serde_json::{Value, json};

/// A fake installation selected through launch argv, recording the actual authorized launch spec.
#[derive(Default)]
struct InstalledClis {
    launches: FakeLauncher,
    hold_turn: bool,
}

#[async_trait::async_trait]
impl ProcessLauncher for InstalledClis {
    async fn spawn(&self, spec: LaunchSpec) -> Result<ManagedProcess> {
        let program = spec.program().expect("expected executable");
        let process = if spec.argv.iter().any(|arg| arg == "--version") {
            let banner = match program {
                "claude" => "2.1.270 (Claude Code)",
                "codex" => "codex-cli 0.154.0",
                _ => "fake-acp 1.2.3",
            };
            FakeProcess::transcript([banner])
        } else if spec.argv.iter().any(|arg| arg == "--help") {
            FakeProcess::transcript(include_str!("fixtures/claude-help.txt").lines())
        } else if spec.argv.iter().any(|arg| arg == "auth") {
            FakeProcess::transcript([
                r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty"}"#,
            ])
        } else if program == "claude" {
            if self.hold_turn {
                FakeProcess::responding(SilentClaude::answer).with_greeting([
                    r#"{"type":"system","subtype":"init","session_id":"claude-fake"}"#,
                ])
            } else {
                FakeProcess::transcript(include_str!("fixtures/claude-turn.jsonl").lines())
            }
        } else if program == "codex" {
            let cwd = spec.cwd.to_string_lossy().into_owned();
            let hold = self.hold_turn;
            FakeProcess::responding(move |line| FakeCodex::answer(line, &cwd, hold))
        } else {
            let agent = FakeAcpAgent::new().asking_for_approval(Approval::Once);
            if self.hold_turn {
                agent.staying_silent().process()
            } else {
                agent.process()
            }
        };
        self.launches.push(process);
        let is_held_claude =
            program == "claude" && self.hold_turn && spec.argv.iter().any(|arg| arg == "--print");
        let mut child = self.launches.spawn(spec).await?;
        // Claude consumes stdin before running. EOF must leave the fake child alive until cancel.
        if is_held_claude {
            child.stdin = child
                .stdin
                .map(|inner| Box::new(KeepAliveInput(inner)) as Box<dyn ByteSink>);
        }
        Ok(child)
    }
}

/// A live Claude process that produces no further stdout until the host terminates it.
struct SilentClaude;
impl SilentClaude {
    fn answer(_line: &str) -> Vec<String> {
        Vec::new()
    }
}

/// Input EOF without ending the process, as Claude's print mode does.
struct KeepAliveInput(Box<dyn ByteSink>);
#[async_trait::async_trait]
impl ByteSink for KeepAliveInput {
    async fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.0.write_all(bytes).await
    }
    async fn close(&mut self) -> Result<()> {
        Ok(())
    }
}

/// A small app-server peer with an approval round trip and a native interrupt response.
struct FakeCodex;
impl FakeCodex {
    fn answer(line: &str, cwd: &str, hold: bool) -> Vec<String> {
        let frame: Value = serde_json::from_str(line).expect("expected JSON-RPC");
        let id = frame["id"].clone();
        let method = frame["method"].as_str();
        if method.is_none() {
            assert_eq!(
                frame["result"]["decision"], "decline",
                "expected host denial"
            );
            return if hold {
                Vec::new()
            } else {
                vec![Self::terminal("completed")]
            };
        }
        if id.is_null() {
            return Vec::new();
        }
        let result = match method {
            Some("initialize") => json!({"userAgent":"unrelated-host/0.154.0"}),
            Some("account/read") => {
                json!({"account":{"type":"chatgpt","planType":"plus"},"requiresOpenaiAuth":true})
            }
            Some("model/list") => json!({"data":[],"nextCursor":null}),
            Some("thread/start") => json!({"thread":{"id":"thread-fake","cwd":cwd}}),
            Some("turn/start") => json!({"turn":{"id":"turn-fake","status":"inProgress"}}),
            _ => json!({}),
        };
        let mut replies = vec![json!({"id":id,"result":result}).to_string()];
        if method == Some("turn/start") {
            replies.push(json!({"method":"item/agentMessage/delta","params":{
                "threadId":"thread-fake","turnId":"turn-fake","itemId":"message-1","delta":"hello"
            }}).to_string());
            replies.push(json!({"id":"ask-1","method":"item/commandExecution/requestApproval","params":{
                "threadId":"thread-fake","turnId":"turn-fake","itemId":"command-1","command":"echo hello","cwd":cwd
            }}).to_string());
        }
        if method == Some("turn/interrupt") {
            replies.push(Self::terminal("interrupted"));
        }
        replies
    }

    fn terminal(status: &str) -> String {
        json!({"method":"turn/completed","params":{
            "threadId":"thread-fake","turn":{"id":"turn-fake","status":status}
        }})
        .to_string()
    }
}

/// Choose a concrete harness without any product identifier, RPC port or recovery store.
fn harnesses() -> Vec<Box<dyn Harness>> {
    let vendor = VendorInfo {
        company: "Fake",
        terms_url: "https://example.invalid/terms",
        privacy_url: "https://example.invalid/privacy",
        skills_are_slash_commands: false,
    };
    vec![
        Box::new(AcpHarness::new(Arc::new(AcpProfile::custom(
            "independent",
            ["fake-acp"],
            vendor,
        )))),
        Box::new(mango_agent_claude::ClaudeHarness::new()),
        Box::new(mango_agent_codex::CodexHarness::new()),
    ]
}

#[tokio::test]
async fn each_registry_harness_discovers_opens_sends_reads_responds_and_closes() {
    for harness in harnesses() {
        let launcher = Arc::new(InstalledClis::default());
        let cwd = std::env::temp_dir();
        let host = HostContext::builder()
            .launcher(launcher.clone())
            .cwd(&cwd)
            .environment(EnvSource::from_pairs([(
                "HOST_SECRET",
                "never-forward-this",
            )]))
            .client_info("unrelated-host", "1.0.0")
            .build()
            .expect("expected host");
        let mut text = false;
        let mut approval = false;
        let mut completed = 0;
        tokio::time::timeout(
            Duration::from_secs(10),
            independent_agent_host::run(harness.as_ref(), &host, CancelToken::new(), |event| {
                text |= matches!(event.kind, EventKind::TextDelta { .. });
                approval |= matches!(event.kind, EventKind::ApprovalRequested { .. });
                completed += usize::from(matches!(event.kind, EventKind::Completed));
            }),
        )
        .await
        .expect("expected bounded run")
        .expect("expected successful run");
        assert!(
            text,
            "expected assistant text from {:?}",
            harness.descriptor().identity
        );
        assert_eq!(completed, 1);
        if harness.descriptor().identity.id.as_str() != "claude" {
            assert!(approval);
        }
        assert_eq!(launcher.launches.live_children(), 0);
        for launch in launcher.launches.launches() {
            assert_eq!(launch.cwd, cwd);
            assert!(!launch.env.contains_key("HOST_SECRET"));
        }
    }
}

#[tokio::test]
async fn each_registry_harness_cancels_live_work_then_closes() {
    for harness in harnesses() {
        let launcher = Arc::new(InstalledClis {
            hold_turn: true,
            ..InstalledClis::default()
        });
        let host = HostContext::builder()
            .launcher(launcher.clone())
            .cwd(std::env::temp_dir())
            .client_info("unrelated-host", "1.0.0")
            .build()
            .expect("expected host");
        let cancel = CancelToken::new();
        let mut cancelled = false;
        let mut terminal = false;
        tokio::time::timeout(
            Duration::from_secs(10),
            independent_agent_host::run(harness.as_ref(), &host, cancel.clone(), |event| {
                if matches!(event.kind, EventKind::TurnStarted { .. }) {
                    cancel.cancel();
                }
                cancelled |= matches!(event.kind, EventKind::Cancelled { .. });
                terminal |= event.is_terminal();
            }),
        )
        .await
        .expect("expected bounded cancellation")
        .expect("expected cancel and close");
        assert!(cancelled && terminal);
        assert_eq!(launcher.launches.live_children(), 0);
    }
}
