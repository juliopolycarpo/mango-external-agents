//! Registry harnesses over named fakes. These tests require no installed vendor or product host.

use std::sync::Arc;
use std::time::Duration;

use mango_agent_acp::testing::{Approval, FakeAcpAgent};
use mango_agent_acp::{AcpHarness, AcpProfile};
use mango_agent_claude::testing::{FakeClaudeCli, Run};
use mango_agent_codex::MINIMUM_CODEX_VERSION;
use mango_agent_codex::testing::VersionAnsweringLauncher;
use mango_external_agents::testing::{FakeLauncher, FakeProcess};
use mango_external_agents::{
    CancelToken, EnvSource, EventKind, Harness, HostContext, LaunchSpec, ManagedProcess,
    ProcessLauncher, Result, VendorInfo,
};
use serde_json::{Value, json};

/// A fake installation of the three vendor CLIs, selected by the launched program.
///
/// Each vendor is answered by the fake its crate exports: `FakeClaudeCli` for `claude`,
/// `VersionAnsweringLauncher` (which answers `codex --version` at the crate's own minimum) over a
/// queue of app-server processes for `codex`, and `FakeAcpAgent` for the custom ACP profile.
struct InstalledClis {
    claude: FakeClaudeCli,
    codex_processes: Arc<FakeLauncher>,
    codex: VersionAnsweringLauncher,
    acp: FakeLauncher,
    hold_turn: bool,
}

impl InstalledClis {
    fn new(hold_turn: bool) -> Self {
        let turn = if hold_turn {
            // Claude's print mode keeps running until the host ends it.
            Run::stalling([r#"{"type":"system","subtype":"init","session_id":"claude-fake"}"#])
        } else {
            Run::replaying(include_str!("fixtures/claude-turn.jsonl"))
        };
        let codex_processes = Arc::new(FakeLauncher::new());
        Self {
            claude: FakeClaudeCli::new()
                .with_help(include_str!("fixtures/claude-help.txt"))
                .with_turn(turn),
            codex: VersionAnsweringLauncher::new(codex_processes.clone()),
            codex_processes,
            acp: FakeLauncher::new(),
            hold_turn,
        }
    }

    /// Every launch the fake installation was asked to run, in each vendor's own log.
    fn launches(&self) -> Vec<LaunchSpec> {
        let mut launches = self.claude.launches();
        launches.extend(self.codex_processes.launches());
        launches.extend(self.acp.launches());
        launches
    }

    /// Whether every child the host started has ended.
    fn children_ended(&self) -> bool {
        self.claude.all_children_ended()
            && self.codex_processes.live_children() == 0
            && self.acp.live_children() == 0
    }
}

#[async_trait::async_trait]
impl ProcessLauncher for InstalledClis {
    async fn spawn(&self, spec: LaunchSpec) -> Result<ManagedProcess> {
        match spec.program().expect("expected executable") {
            "claude" => self.claude.spawn(spec).await,
            "codex" => {
                // The version-answering launcher takes the probe itself; every other launch
                // is an app-server session and needs a process queued first.
                if !spec.argv.iter().any(|arg| arg == "--version") {
                    let cwd = spec.cwd.to_string_lossy().into_owned();
                    let hold = self.hold_turn;
                    self.codex_processes
                        .push(FakeProcess::responding(move |line| {
                            FakeCodex::answer(line, &cwd, hold)
                        }));
                }
                self.codex.spawn(spec).await
            }
            _ => {
                let agent = FakeAcpAgent::new().asking_for_approval(Approval::Once);
                let agent = if self.hold_turn {
                    agent.staying_silent()
                } else {
                    agent
                };
                let process = if spec.argv.iter().any(|arg| arg == "--version") {
                    agent.version_process()
                } else {
                    agent.process()
                };
                self.acp.push(process);
                self.acp.spawn(spec).await
            }
        }
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
            // The initialize answer carries the app-server's own version, held to the same floor
            // as `codex --version`.
            Some("initialize") => {
                json!({"userAgent":format!("unrelated-host/{MINIMUM_CODEX_VERSION}")})
            }
            // 0.8.0 asks for gateway readiness explicitly before it reads the account.
            Some("account/gatewayOAuth/read") => json!({"required":false,"status":null}),
            Some("account/read") => {
                json!({"account":{"type":"chatgpt","planType":"plus"},"requiresOpenaiAuth":true})
            }
            Some("model/list") => json!({"data":[],"nextCursor":null}),
            // The protocol workspace can omit a trailing separator retained by the launch spec.
            Some("thread/start") => {
                json!({"thread":{"id":"thread-fake","cwd":frame["params"]["cwd"]}})
            }
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
        let launcher = Arc::new(InstalledClis::new(false));
        // Exercise the trailing separator supplied by macOS and Windows on every platform.
        let workspace = std::env::temp_dir().join("independent-workspace");
        let cwd = std::path::PathBuf::from(format!(
            "{}{}",
            workspace.display(),
            std::path::MAIN_SEPARATOR
        ));
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
        assert!(launcher.children_ended());
        for launch in launcher.launches() {
            assert_eq!(launch.cwd, cwd);
            assert!(!launch.env.contains_key("HOST_SECRET"));
        }
    }
}

#[tokio::test]
async fn each_registry_harness_cancels_live_work_then_closes() {
    for harness in harnesses() {
        let launcher = Arc::new(InstalledClis::new(true));
        let workspace = std::env::temp_dir().join("independent-workspace");
        let cwd = std::path::PathBuf::from(format!(
            "{}{}",
            workspace.display(),
            std::path::MAIN_SEPARATOR
        ));
        let host = HostContext::builder()
            .launcher(launcher.clone())
            .cwd(cwd)
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
        assert!(launcher.children_ended());
    }
}
