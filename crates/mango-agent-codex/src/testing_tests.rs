use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use mango_external_agents::process::{LaunchSpec, ManagedProcess, ProcessLauncher};
use mango_external_agents::testing::{FakeLauncher, FakeProcess};
use mango_external_agents::{CancelReason, Dispatch, Error, Harness, HostContext, OpenSession};

use super::{MINIMUM_CODEX_VERSION, VersionAnsweringLauncher};

fn spec(argv: &[&str]) -> LaunchSpec {
    LaunchSpec {
        argv: argv.iter().map(|argument| (*argument).to_owned()).collect(),
        cwd: PathBuf::from("host-selected-test-directory"),
        env: BTreeMap::from([(String::from("HOST_SETTING"), String::from("unchanged"))]),
        stdin: true,
        hide_window: true,
    }
}

async fn output(mut process: ManagedProcess) -> String {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut bytes = Vec::new();
        while let Some(chunk) = process.stdout.next_chunk().await.expect("fake stdout") {
            bytes.extend(chunk);
        }
        String::from_utf8(bytes).expect("expected the fake's UTF-8 transcript")
    })
    .await
    .expect("expected the finite fake transcript within the deadline")
}

#[tokio::test]
async fn both_default_constructors_answer_the_floor_without_consuming_the_inner_child() {
    let inner = Arc::new(FakeLauncher::new());
    inner.push(FakeProcess::transcript(["app-server transcript"]));
    let launchers = [
        VersionAnsweringLauncher::over(inner.clone()),
        Arc::new(VersionAnsweringLauncher::new(inner.clone())),
    ];
    for launcher in &launchers {
        let process = launcher
            .spawn(spec(&["codex", "--version"]))
            .await
            .expect("version probe");
        assert_eq!(
            output(process).await,
            format!("codex-cli {MINIMUM_CODEX_VERSION}\n")
        );
        assert!(inner.launches().is_empty());
    }
    let launch = spec(&["codex", "app-server"]);
    let process = launchers[0]
        .spawn(launch.clone())
        .await
        .expect("app-server");
    assert_eq!(output(process).await, "app-server transcript\n");
    assert_eq!(inner.launches(), vec![launch]);
}

#[tokio::test]
async fn a_configured_executable_and_banner_also_keep_the_plain_codex_probe() {
    let inner = Arc::new(FakeLauncher::new());
    let launcher = VersionAnsweringLauncher::new(inner.clone())
        .with_executable("/host/bin/custom-codex")
        .with_version_line("wrapper-changed β banner");
    for program in ["/host/bin/custom-codex", "codex"] {
        let process = launcher
            .spawn(spec(&[program, "--version"]))
            .await
            .expect("probe");
        assert_eq!(output(process).await, "wrapper-changed β banner\n");
    }
    assert!(inner.launches().is_empty());
}

#[tokio::test]
async fn other_programs_paths_and_argument_shapes_delegate_the_complete_launch_spec() {
    for argv in [
        vec!["claude", "--version"],
        vec!["acp-agent", "--version"],
        vec!["/unconfigured/codex", "--version"],
        vec!["codex.exe", "--version"],
        vec!["codex", "app-server"],
        vec!["codex", "--version", "extra"],
        vec!["codex", "--help", "--version"],
        vec!["codex"],
        vec![],
    ] {
        let inner = Arc::new(FakeLauncher::new());
        inner.push(FakeProcess::transcript(["delegated banner"]));
        let launcher =
            VersionAnsweringLauncher::new(inner.clone()).with_executable("/host/bin/custom-codex");
        let launch = spec(&argv);
        let process = launcher
            .spawn(launch.clone())
            .await
            .expect("delegated process");
        assert_eq!(output(process).await, "delegated banner\n");
        assert_eq!(inner.launches(), vec![launch]);
    }
}

#[tokio::test]
async fn delegated_process_io_and_control_still_belong_to_the_inner_launcher() {
    let inner = Arc::new(FakeLauncher::new());
    inner.push(FakeProcess::responding(|line| vec![format!("echo:{line}")]));
    let launcher = VersionAnsweringLauncher::new(inner.clone());
    let mut process = launcher
        .spawn(spec(&["codex", "app-server"]))
        .await
        .expect("process");
    assert_eq!(inner.live_children(), 1);
    process
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"host request\n")
        .await
        .expect("write");
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        process.stdout.next_chunk(),
    )
    .await
    .expect("expected the delegated echo within the deadline")
    .expect("read")
    .expect("response");
    assert_eq!(response, b"echo:host request\n");
    assert_eq!(inner.written(), vec![String::from("host request")]);
    process
        .control
        .kill(CancelReason::Requested)
        .await
        .expect("host-owned cleanup");
    assert_eq!(inner.live_children(), 0);
    assert!(
        process
            .stdout
            .next_chunk()
            .await
            .expect("closed stdout")
            .is_none()
    );
}

/// A named host launcher that returns its typed refusal without converting it to text.
struct RefusingLauncher {
    error: Error,
}

#[async_trait::async_trait]
impl ProcessLauncher for RefusingLauncher {
    async fn spawn(&self, _spec: LaunchSpec) -> mango_external_agents::Result<ManagedProcess> {
        Err(self.error.clone())
    }
}

#[tokio::test]
async fn a_delegated_error_preserves_dispatch_cause_and_recovery_control() {
    let children = FakeLauncher::new();
    children.push(FakeProcess::responding(|_| Vec::new()));
    let process = children
        .spawn(spec(&["owned-fake-child"]))
        .await
        .expect("child");
    let control = process.control.clone();
    let refusal = Error::CleanupRequired {
        control: control.clone(),
        source: Box::new(Error::LimitExceeded {
            subject: "host children",
            limit: 1,
            received: 2,
        }),
    }
    .with_dispatch(Dispatch::NotSubmitted);
    let launcher = VersionAnsweringLauncher::new(Arc::new(RefusingLauncher { error: refusal }));
    let error = launcher
        .spawn(spec(&["claude", "--version"]))
        .await
        .err()
        .expect("host refusal");
    assert_eq!(error.dispatch(), Dispatch::NotSubmitted);
    assert!(matches!(
        error.cause(),
        Error::LimitExceeded {
            subject: "host children",
            limit: 1,
            received: 2
        }
    ));
    assert!(Arc::ptr_eq(
        &error.cleanup_control().expect("recovery control"),
        &control
    ));
    control
        .kill(CancelReason::Requested)
        .await
        .expect("reconcile the test-owned child");
    assert_eq!(children.live_children(), 0);
}

#[tokio::test]
async fn a_configured_older_banner_exercises_the_real_pre_spawn_version_refusal() {
    let inner = Arc::new(FakeLauncher::new());
    let launcher =
        VersionAnsweringLauncher::new(inner.clone()).with_version_line("codex-cli 0.1.0");
    let host = HostContext::builder()
        .launcher(Arc::new(launcher))
        .cwd(std::env::temp_dir())
        .client_info("version-helper-test", "0.0.0")
        .build()
        .expect("test host");
    let error = crate::CodexHarness::new()
        .open_session(&host, OpenSession::new("helper-test"))
        .await
        .err()
        .expect("expected the older build refused");
    assert!(
        matches!(error.cause(), Error::VersionGate { found, minimum }
        if found == "0.1.0" && minimum == MINIMUM_CODEX_VERSION)
    );
    assert!(
        inner.launches().is_empty(),
        "expected no app-server launch before version refusal"
    );
}
