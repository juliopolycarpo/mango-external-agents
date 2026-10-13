//! The version floor is applied before an app-server exists.
//!
//! An older `codex app-server` creates sqlite state, an installation id and system skills under
//! `CODEX_HOME` merely by starting, so a refusal that waits for the `initialize` handshake arrives
//! after the damage. These tests drive the harness over a plain [`FakeLauncher`] and assert the
//! launch log first: what was started matters more than which error came back.

#[allow(dead_code)]
mod support;

use std::sync::Arc;
use std::time::SystemTime;

use mango_agent_codex::{CodexHarness, MINIMUM_CODEX_VERSION};
use mango_external_agents::testing::{FakeLauncher, FakeProcess};
use mango_external_agents::{
    Discovery, DiscoveryReceipt, DiscoveryReceiptMeasurements, EnvSource, Error, ExecutablePath,
    GateVerdict, Harness, HarnessId, HostContext, OpenSession, SessionQuery,
};
use support::{Transcript, workspace_path};

const OLD: &str = "0.150.0";

fn host(launcher: &Arc<FakeLauncher>) -> HostContext {
    HostContext::builder()
        .launcher(launcher.clone())
        .cwd(workspace_path())
        .client_info("mango-test", "0.0.1")
        .environment(EnvSource::from_pairs([
            ("PATH", "/usr/bin"),
            ("CODEX_HOME", "/home/user/.codex"),
        ]))
        .build()
        .expect("expected a host")
}

/// What `codex --version` prints for `version`.
fn version_line(version: &str) -> FakeProcess {
    FakeProcess::transcript([format!("codex-cli {version}")])
}

/// An app-server whose handshake names `user_agent` instead of the recorded build.
fn app_server_claiming(user_agent: &'static str) -> FakeProcess {
    Transcript::load("handshake").as_process_intercepting(move |frame| {
        (frame["method"] == "initialize").then(|| {
            vec![
                serde_json::json!({
                    "id": frame["id"],
                    "result": {"userAgent": user_agent, "platformOs": "linux"},
                })
                .to_string(),
            ]
        })
    })
}

/// The programs and first argument of every launch, in order: `["codex --version", ...]`.
fn launched(launcher: &FakeLauncher) -> Vec<String> {
    launcher
        .launches()
        .iter()
        .map(|launch| launch.argv.join(" "))
        .collect()
}

#[track_caller]
fn assert_launched(launcher: &FakeLauncher, expected: &[&str]) {
    let received = launched(launcher);
    assert_eq!(
        received, expected,
        "expected launches {expected:?} | received {received:?}"
    );
}

#[track_caller]
fn assert_version_gate(error: Option<&Error>, found: &str) {
    assert!(
        matches!(
            error,
            Some(Error::VersionGate { found: actual, minimum })
                if actual == found && minimum == MINIMUM_CODEX_VERSION
        ),
        "expected VersionGate {{ found: {found}, minimum: {MINIMUM_CODEX_VERSION} }} | received {error:?}"
    );
}

/// A receipt for a probe that saw `version`, bound to `request` the way a host binds one.
fn receipt_for(
    harness: &CodexHarness,
    host: &HostContext,
    request: OpenSession,
    version: Option<&str>,
) -> OpenSession {
    let discovery = Discovery {
        version: version.map(str::to_owned),
        gate: GateVerdict::Unknown,
        ..Discovery::not_installed()
    };
    let receipt = DiscoveryReceipt::new(HarnessId::codex(), discovery, SystemTime::now())
        .with_executable_fingerprint("executable-1")
        .with_environment_fingerprint("environment-1")
        .with_authorization_fingerprint("authorization-1")
        .bind_to_open(
            harness.descriptor(),
            host,
            &request,
            DiscoveryReceiptMeasurements::new()
                .with_executable_fingerprint("executable-1")
                .with_environment_fingerprint("environment-1")
                .with_authorization_fingerprint("authorization-1"),
        )
        .expect("expected the receipt to bind to its own request");
    request.with_discovery(receipt)
}

#[tokio::test]
async fn an_outdated_build_is_refused_without_starting_an_app_server() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(version_line(OLD));
    launcher.push(Transcript::load("turn").as_process());
    let host = host(&launcher);

    let result = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await;

    assert_launched(&launcher, &["codex --version"]);
    assert_version_gate(result.as_ref().err(), OLD);
    assert_eq!(
        launcher.live_children(),
        0,
        "expected the version probe reaped | received {} live children",
        launcher.live_children()
    );
}

#[tokio::test]
async fn a_current_build_costs_one_version_probe_and_one_app_server() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(version_line(MINIMUM_CODEX_VERSION));
    launcher.push(Transcript::load("turn").as_process());
    let host = host(&launcher);

    let result = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await;

    assert_launched(&launcher, &["codex --version", "codex app-server"]);
    assert!(
        result.is_ok(),
        "expected a session | received {:?}",
        result.as_ref().err()
    );
}

/// A CLI that changed the shape of `--version` has not stopped working: the documented rule is
/// `Unknown`, which proceeds, and the handshake is the second line.
#[tokio::test]
async fn an_unparseable_version_proceeds_and_the_handshake_decides() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(FakeProcess::transcript(["codex banana"]));
    launcher.push(app_server_claiming("codex/0.147.0 (Linux)"));
    let host = host(&launcher);

    let result = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await;

    assert_launched(&launcher, &["codex --version", "codex app-server"]);
    assert_version_gate(result.as_ref().err(), "0.147.0");
}

/// An executable the launcher cannot start has no version to refuse: the app-server launch that
/// follows is the one that reports it.
#[tokio::test]
async fn a_version_probe_that_cannot_launch_leaves_the_launch_to_report_it() {
    let launcher = Arc::new(FakeLauncher::new());
    let host = host(&launcher);

    let result = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await;

    assert_launched(&launcher, &[]);
    assert!(
        matches!(
            result.as_ref().err().map(Error::cause),
            Some(Error::Launch { .. })
        ),
        "expected a launch failure from the app-server launch | received {:?}",
        result.as_ref().err()
    );
}

#[tokio::test]
async fn the_probe_and_the_app_server_run_the_executable_the_host_resolved() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(version_line(MINIMUM_CODEX_VERSION));
    launcher.push(Transcript::load("turn").as_process());
    let host = host(&launcher);
    let request = OpenSession::new("chat-1")
        .with_executable(ExecutablePath::resolved("/opt/codex/bin/codex"));

    let result = CodexHarness::new().open_session(&host, request).await;

    assert_launched(
        &launcher,
        &[
            "/opt/codex/bin/codex --version",
            "/opt/codex/bin/codex app-server",
        ],
    );
    assert!(
        result.is_ok(),
        "expected a session | received {:?}",
        result.as_ref().err()
    );
}

/// The host's own probe is the version: no second process, and an outdated one launches nothing.
#[tokio::test]
async fn a_receipt_carrying_an_outdated_version_refuses_without_any_launch() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(Transcript::load("turn").as_process());
    let host = host(&launcher);
    let harness = CodexHarness::new();
    let request = receipt_for(&harness, &host, OpenSession::new("chat-1"), Some(OLD));

    let result = harness.open_session(&host, request).await;

    assert_launched(&launcher, &[]);
    assert_version_gate(result.as_ref().err(), OLD);
}

#[tokio::test]
async fn a_receipt_carrying_a_current_version_skips_the_version_probe() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(Transcript::load("turn").as_process());
    let host = host(&launcher);
    let harness = CodexHarness::new();
    let request = receipt_for(
        &harness,
        &host,
        OpenSession::new("chat-1"),
        Some(MINIMUM_CODEX_VERSION),
    );

    let result = harness.open_session(&host, request).await;

    assert_launched(&launcher, &["codex app-server"]);
    assert!(
        result.is_ok(),
        "expected a session | received {:?}",
        result.as_ref().err()
    );
}

/// A receipt that learned no version is the host having already asked: it is `Unknown`, so the
/// open proceeds without asking again and the handshake remains the second line.
#[tokio::test]
async fn a_receipt_without_a_version_proceeds_without_probing_and_the_handshake_decides() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(app_server_claiming("codex/0.147.0 (Linux)"));
    let host = host(&launcher);
    let harness = CodexHarness::new();
    let request = receipt_for(&harness, &host, OpenSession::new("chat-1"), None);

    let result = harness.open_session(&host, request).await;

    assert_launched(&launcher, &["codex app-server"]);
    assert_version_gate(result.as_ref().err(), "0.147.0");
}

#[tokio::test]
async fn the_picker_refuses_an_outdated_build_without_starting_an_app_server() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(version_line(OLD));
    launcher.push(Transcript::load("handshake").as_process());
    let host = host(&launcher);

    let result = CodexHarness::new()
        .list_sessions(&host, SessionQuery::default())
        .await;

    assert_launched(&launcher, &["codex --version"]);
    assert_version_gate(result.as_ref().err(), OLD);
}

#[tokio::test]
async fn the_picker_on_a_current_build_costs_one_version_probe_and_one_app_server() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(version_line(MINIMUM_CODEX_VERSION));
    launcher.push(Transcript::load("handshake").as_process());
    let host = host(&launcher);

    let _ = CodexHarness::new()
        .list_sessions(&host, SessionQuery::default())
        .await;

    assert_launched(&launcher, &["codex --version", "codex app-server"]);
}
