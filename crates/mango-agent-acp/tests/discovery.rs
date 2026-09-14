//! Discovery and launch routing when installed agents share a command name.
#![cfg(feature = "testing")]

#[path = "discovery/policy.rs"]
mod policy;

use std::sync::Arc;

use mango_agent_acp::AcpHarness;
use mango_agent_acp::testing::FakeAcpAgent;
use mango_external_agents::testing::FakeLauncher;
use mango_external_agents::{
    CloseReason, Error, GateVerdict, Harness, HostContext, LaunchSpec, ManagedProcess,
    ProcessLauncher, Result,
};

/// A program available from one entry on the fixture `PATH`.
#[derive(Clone, Copy)]
struct InstalledProgram {
    command: &'static str,
    version: &'static str,
}

/// One directory of programs available to the fixture launcher.
#[derive(Clone, Copy)]
struct PathEntry {
    directory: &'static str,
    programs: &'static [InstalledProgram],
}

const CURSOR_BIN: PathEntry = PathEntry {
    directory: "/opt/cursor/bin",
    programs: &[
        InstalledProgram {
            command: "agent",
            version: "2026.09.10",
        },
        InstalledProgram {
            command: "cursor-agent",
            version: "2026.09.10",
        },
    ],
};

const GROK_BIN: PathEntry = PathEntry {
    directory: "/opt/grok/bin",
    programs: &[
        InstalledProgram {
            command: "agent",
            version: "0.1.0",
        },
        InstalledProgram {
            command: "grok",
            version: "0.1.0",
        },
    ],
};

#[derive(Clone, Copy)]
enum InstallOrder {
    CursorThenGrok,
    GrokThenCursor,
}

/// Models stable aliases and the two PATH-resolution orders for the shared `agent` command.
struct InstalledAgents {
    path: Vec<PathEntry>,
    launches: FakeLauncher,
}

impl InstalledAgents {
    fn with_path(path: impl IntoIterator<Item = PathEntry>) -> Self {
        Self {
            path: path.into_iter().collect(),
            launches: FakeLauncher::new(),
        }
    }

    fn both(order: InstallOrder) -> Self {
        match order {
            InstallOrder::CursorThenGrok => Self::with_path([CURSOR_BIN, GROK_BIN]),
            InstallOrder::GrokThenCursor => Self::with_path([GROK_BIN, CURSOR_BIN]),
        }
    }

    fn resolve(&self, command: &str) -> Option<ResolvedProgram> {
        self.path.iter().find_map(|entry| {
            entry
                .programs
                .iter()
                .find(|program| program.command == command)
                .map(|program| ResolvedProgram {
                    directory: entry.directory,
                    version: program.version,
                })
        })
    }
}

#[derive(Clone, Copy)]
struct ResolvedProgram {
    directory: &'static str,
    version: &'static str,
}

#[async_trait::async_trait]
impl ProcessLauncher for InstalledAgents {
    async fn spawn(&self, spec: LaunchSpec) -> Result<ManagedProcess> {
        let program = spec.argv.first().map_or("", String::as_str);
        let Some(resolved) = self.resolve(program) else {
            return Err(Error::Launch {
                program: program.to_owned(),
                message: format!(
                    "expected an executable on the fixture PATH, received {program:?}"
                ),
            });
        };
        let agent = FakeAcpAgent::new().printing_version(resolved.version);
        // Named anywhere in the argv, not only second: a profile may put its own flags — Grok's
        // `--no-auto-update` — ahead of `--version`, and a fixture that only looked at `argv[1]`
        // would hand such a probe a full ACP agent and wait out the request timeout.
        let process = if spec.argv.iter().any(|arg| arg == "--version") {
            agent.version_process()
        } else {
            agent.process()
        };
        self.launches.push(process);
        self.launches.spawn(spec).await
    }
}

fn host(launcher: Arc<dyn ProcessLauncher>) -> HostContext {
    HostContext::builder()
        .launcher(launcher)
        .cwd(std::env::temp_dir())
        .client_info("discovery-tests", "0.1.0")
        .build()
        .expect("expected a host")
}

#[tokio::test]
async fn cursor_discovery_does_not_mistake_grok_for_cursor() {
    let launcher = Arc::new(InstalledAgents::with_path([GROK_BIN]));
    let discovery = AcpHarness::builtin("cursor")
        .expect("expected Cursor profile")
        .discover(&host(launcher))
        .await
        .expect("expected discovery");
    assert_eq!(
        discovery.gate,
        GateVerdict::NotInstalled,
        "expected missing Cursor when only Grok owns agent"
    );
}

#[tokio::test]
async fn both_install_orders_discover_and_launch_distinct_agents() {
    for (order, shared_directory, shared_version) in [
        (
            InstallOrder::CursorThenGrok,
            "/opt/cursor/bin",
            "2026.09.10",
        ),
        (InstallOrder::GrokThenCursor, "/opt/grok/bin", "0.1.0"),
    ] {
        let launcher = Arc::new(InstalledAgents::both(order));
        let shared_agent = launcher
            .resolve("agent")
            .expect("expected the shared command on the fixture PATH");
        assert_eq!(shared_agent.directory, shared_directory);
        assert_eq!(shared_agent.version, shared_version);
        let context = host(launcher.clone());
        for (id, version, argv) in [
            ("cursor", "2026.09.10", vec!["cursor-agent", "acp"]),
            (
                "grok",
                "0.1.0",
                vec!["grok", "--no-auto-update", "agent", "stdio"],
            ),
        ] {
            let harness = AcpHarness::builtin(id).expect("expected a built-in profile");
            let discovery = harness
                .discover(&context)
                .await
                .expect("expected discovery");
            assert_eq!(
                discovery.version.as_deref(),
                Some(version),
                "expected {id}'s version"
            );
            let session = harness
                .open_session(&context, mango_external_agents::OpenSession::new(id))
                .await
                .expect("expected an ACP session");
            session
                .close(CloseReason::Requested)
                .await
                .expect("expected close");
            let calls = launcher.launches.launches();
            assert_eq!(calls.last().expect("expected launch").argv, argv);
            assert!(calls.iter().all(|call| call.argv[0] != "agent"));
        }
    }
}

#[tokio::test]
async fn host_executable_is_shared_by_discovery_and_sessions_until_request_overrides_it() {
    use mango_external_agents::{ExecutablePath, OpenSession};

    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(
        FakeAcpAgent::new()
            .printing_version("2026.09.10")
            .version_process(),
    );
    launcher.push(FakeAcpAgent::new().process());
    launcher.push(FakeAcpAgent::new().process());
    let context = host(launcher.clone());
    let path = std::env::temp_dir()
        .join("installed cursor")
        .join("cursor-agent");
    let override_path = std::env::temp_dir()
        .join("other cursor")
        .join("cursor-agent");
    let harness = AcpHarness::builtin("cursor")
        .expect("expected Cursor profile")
        .with_executable(ExecutablePath::resolved(path.clone()));
    let discovery = harness
        .discover(&context)
        .await
        .expect("expected discovery");
    assert_eq!(discovery.executable.as_ref(), Some(&path));
    for request in [
        OpenSession::new("default"),
        OpenSession::new("override")
            .with_executable(ExecutablePath::resolved(override_path.clone())),
    ] {
        let session = harness
            .open_session(&context, request)
            .await
            .expect("expected session");
        session
            .close(CloseReason::Requested)
            .await
            .expect("expected close");
    }
    let calls = launcher.launches();
    assert_eq!(
        calls[0].argv,
        [path.to_string_lossy().as_ref(), "--version"]
    );
    assert_eq!(calls[1].argv, [path.to_string_lossy().as_ref(), "acp"]);
    assert_eq!(
        calls[2].argv,
        [override_path.to_string_lossy().as_ref(), "acp"]
    );
}
