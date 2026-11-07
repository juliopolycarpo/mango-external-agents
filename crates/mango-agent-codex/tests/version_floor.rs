//! The version floor is applied before an app-server exists.
//!
//! An older `codex app-server` creates sqlite state, an installation id and system skills under
//! `CODEX_HOME` merely by starting, so a refusal that waits for the `initialize` handshake arrives
//! after the damage. These tests drive the harness over a plain [`FakeLauncher`] and assert the
//! launch log first: what was started matters more than which error came back.

#[allow(dead_code)]
mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use mango_agent_codex::deadlines::Deadlines;
use mango_agent_codex::{CodexHarness, MINIMUM_CODEX_VERSION};
use mango_external_agents::testing::{FakeLauncher, FakeProcess};
use mango_external_agents::{
    CancelToken, Discovery, DiscoveryReceipt, DiscoveryReceiptMeasurements, EnvSource, Error,
    ExecutablePath, GateVerdict, Harness, HarnessId, HostContext, OpenSession, ProcessLauncher,
    SessionQuery,
};
use support::{Transcript, workspace_path};

// The former supported baseline must no longer start an app-server implicitly.
const OLD: &str = "0.154.0";

/// A host that refuses the version child but would otherwise permit an app-server launch.
struct RefusingVersionLauncher {
    inner: Arc<FakeLauncher>,
    error: Error,
    attempts: std::sync::Mutex<Vec<mango_external_agents::LaunchSpec>>,
}

#[async_trait::async_trait]
impl mango_external_agents::ProcessLauncher for RefusingVersionLauncher {
    async fn spawn(
        &self,
        spec: mango_external_agents::LaunchSpec,
    ) -> mango_external_agents::Result<mango_external_agents::ManagedProcess> {
        self.attempts
            .lock()
            .expect("launch attempts")
            .push(spec.clone());
        if spec.argv.get(1).map(String::as_str) == Some("--version") {
            return Err(self.error.clone());
        }
        self.inner.spawn(spec).await
    }
}

fn refusing_host(launcher: Arc<RefusingVersionLauncher>) -> HostContext {
    HostContext::builder()
        .launcher(launcher)
        .cwd(workspace_path())
        .client_info("version-refusal-test", "0.0.1")
        .limits(mango_external_agents::Limits {
            request_timeout: Duration::from_secs(2),
            ..Default::default()
        })
        .build()
        .expect("expected a refusal-test host")
}

#[tokio::test]
async fn a_refused_version_probe_never_starts_an_app_server_for_open_or_listing() {
    for operation in ["open", "list"] {
        let inner = Arc::new(FakeLauncher::new());
        inner.push(app_server_claiming(MINIMUM_CODEX_VERSION));
        let launcher = Arc::new(RefusingVersionLauncher {
            inner: inner.clone(),
            error: Error::LimitExceeded {
                subject: "host children",
                limit: 1,
                received: 2,
            }
            .with_dispatch(mango_external_agents::Dispatch::NotSubmitted),
            attempts: std::sync::Mutex::new(Vec::new()),
        });
        let host = refusing_host(launcher.clone());
        let harness = CodexHarness::new();
        let error = if operation == "open" {
            match harness
                .open_session(&host, OpenSession::new("refused-version"))
                .await
            {
                Ok(session) => {
                    session
                        .close(mango_external_agents::CloseReason::Requested)
                        .await
                        .expect("settle an unexpectedly started fake session");
                    None
                }
                Err(error) => Some(error),
            }
        } else {
            harness
                .list_sessions(&host, SessionQuery::default())
                .await
                .err()
        };
        assert_eq!(
            launcher.attempts.lock().expect("attempts").len(),
            1,
            "expected {operation} to stop at the refused version probe without an app-server launch"
        );
        let error = error.expect("expected the host's version-probe refusal");
        assert!(matches!(
            error.cause(),
            Error::LimitExceeded {
                subject: "host children",
                limit: 1,
                received: 2
            }
        ));
        assert_eq!(
            error.dispatch(),
            mango_external_agents::Dispatch::NotSubmitted
        );
        assert!(inner.launches().is_empty());
        assert_eq!(inner.live_children(), 0);
    }
}

#[tokio::test]
async fn a_host_policy_refusal_is_not_reported_as_an_absent_executable() {
    for account in [false, true] {
        let launcher = Arc::new(RefusingVersionLauncher {
            inner: Arc::new(FakeLauncher::new()),
            error: Error::LimitExceeded {
                subject: "host children",
                limit: 1,
                received: 2,
            }
            .with_dispatch(mango_external_agents::Dispatch::NotSubmitted),
            attempts: std::sync::Mutex::new(Vec::new()),
        });
        let host = refusing_host(launcher.clone());
        let harness = CodexHarness::new();
        let error = if account {
            let key = mango_agent_codex::account::AccountFingerprintKey::new(b"synthetic-host-key")
                .expect("test fingerprint key");
            harness.discover_with_account(&host, &key).await.err()
        } else {
            harness.discover(&host).await.err()
        }
        .expect("expected a typed host refusal, not an absent-executable discovery");
        assert!(matches!(
            error.cause(),
            Error::LimitExceeded {
                subject: "host children",
                limit: 1,
                received: 2
            }
        ));
        assert_eq!(
            error.dispatch(),
            mango_external_agents::Dispatch::NotSubmitted
        );
        assert_eq!(launcher.attempts.lock().expect("attempts").len(), 1);
    }
}

/// A failed probe can still own cleanup; no public entrypoint may replace that with absence.
#[tokio::test]
async fn a_version_probe_spawn_failure_preserves_its_recovery_control_in_every_entrypoint() {
    let owner = FakeLauncher::new();
    owner.push(FakeProcess::responding(|_| Vec::new()));
    let child = owner
        .spawn(mango_external_agents::LaunchSpec {
            argv: vec![String::from("owned-recovery-fixture")],
            cwd: workspace_path(),
            env: Default::default(),
            stdin: false,
            hide_window: true,
        })
        .await
        .expect("expected a test-owned recovery control");
    let control = child.control.clone();
    for operation in ["open", "list", "discover", "account"] {
        let inner = Arc::new(FakeLauncher::new());
        let launcher = Arc::new(RefusingVersionLauncher {
            inner: inner.clone(),
            error: Error::CleanupRequired {
                control: control.clone(),
                source: Box::new(Error::Launch {
                    program: String::from("codex"),
                    message: String::from("synthetic launch cleanup incomplete"),
                }),
            }
            .with_dispatch(mango_external_agents::Dispatch::AcceptanceUnknown),
            attempts: std::sync::Mutex::new(Vec::new()),
        });
        let host = refusing_host(launcher.clone());
        let harness = CodexHarness::new();
        let error = match operation {
            "open" => harness
                .open_session(&host, OpenSession::new("recovery"))
                .await
                .err(),
            "list" => harness
                .list_sessions(&host, SessionQuery::default())
                .await
                .err(),
            "discover" => harness.discover(&host).await.err(),
            _ => {
                let key = mango_agent_codex::account::AccountFingerprintKey::new(b"synthetic-key")
                    .expect("test fingerprint key");
                harness.discover_with_account(&host, &key).await.err()
            }
        }
        .expect("expected the probe's original cleanup failure");
        assert!(matches!(error.cause(), Error::Launch { message, .. }
            if message == "synthetic launch cleanup incomplete"));
        assert_eq!(
            error.dispatch(),
            mango_external_agents::Dispatch::AcceptanceUnknown
        );
        assert!(Arc::ptr_eq(
            &error.cleanup_control().expect("recovery control"),
            &control
        ));
        assert_eq!(launcher.attempts.lock().expect("attempts").len(), 1);
        assert!(inner.launches().is_empty());
    }
    control
        .kill(mango_external_agents::CancelReason::Requested)
        .await
        .expect("settle the test-owned recovery child");
    assert_eq!(owner.live_children(), 0);
}

fn host(launcher: &Arc<FakeLauncher>) -> HostContext {
    host_cancelled_by(launcher, &CancelToken::new())
}

fn host_cancelled_by(launcher: &Arc<FakeLauncher>, cancel: &CancelToken) -> HostContext {
    HostContext::builder()
        .launcher(launcher.clone())
        .cancel(cancel.clone())
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

/// A receipt for a probe of `executable` that saw `version`, bound to `request` as a host binds one.
fn receipt_for(
    harness: &CodexHarness,
    host: &HostContext,
    request: OpenSession,
    executable: Option<&str>,
    version: Option<&str>,
) -> OpenSession {
    let discovery = Discovery {
        executable: executable.map(PathBuf::from),
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

/// A request for the executable at `path`, so a receipt for the same path can vouch for it.
fn request_for(path: &str) -> OpenSession {
    OpenSession::new("chat-1").with_executable(ExecutablePath::resolved(path))
}

const RESOLVED: &str = "/opt/codex/bin/codex";

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

/// The failed version launch itself is returned; there is no second launch to report it again.
#[tokio::test]
async fn a_version_probe_that_cannot_launch_returns_that_error_without_a_second_attempt() {
    let inner = Arc::new(FakeLauncher::new());
    let launcher = Arc::new(RefusingVersionLauncher {
        inner: inner.clone(),
        error: Error::Launch {
            program: String::from("codex"),
            message: String::from("synthetic executable unavailable"),
        },
        attempts: std::sync::Mutex::new(Vec::new()),
    });
    let host = refusing_host(launcher.clone());

    let result = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await;

    assert_eq!(launcher.attempts.lock().expect("attempts").len(), 1);
    assert_launched(&inner, &[]);
    assert!(
        matches!(
            result.as_ref().err().map(Error::cause),
            Some(Error::Launch { message, .. }) if message == "synthetic executable unavailable"
        ),
        "expected the original version-probe launch failure | received {:?}",
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

/// The host's own probe is the version, for the exact executable it probed: no second process, and
/// an outdated one launches nothing.
#[tokio::test]
async fn a_receipt_for_the_launched_path_carrying_an_outdated_version_refuses_without_any_launch() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(Transcript::load("turn").as_process());
    let host = host(&launcher);
    let harness = CodexHarness::new();
    let request = receipt_for(
        &harness,
        &host,
        request_for(RESOLVED),
        Some(RESOLVED),
        Some(OLD),
    );

    let result = harness.open_session(&host, request).await;

    assert_launched(&launcher, &[]);
    assert_version_gate(result.as_ref().err(), OLD);
}

#[tokio::test]
async fn a_receipt_for_the_launched_path_carrying_a_current_version_skips_the_version_probe() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(Transcript::load("turn").as_process());
    let host = host(&launcher);
    let harness = CodexHarness::new();
    let request = receipt_for(
        &harness,
        &host,
        request_for(RESOLVED),
        Some(RESOLVED),
        Some(MINIMUM_CODEX_VERSION),
    );

    let result = harness.open_session(&host, request).await;

    assert_launched(&launcher, &["/opt/codex/bin/codex app-server"]);
    assert!(
        result.is_ok(),
        "expected a session | received {:?}",
        result.as_ref().err()
    );
}

/// A receipt that learned no version vouches for nothing: the probe runs, and what it finds decides.
#[tokio::test]
async fn a_receipt_without_a_version_still_probes() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(version_line(OLD));
    launcher.push(Transcript::load("turn").as_process());
    let host = host(&launcher);
    let harness = CodexHarness::new();
    let request = receipt_for(&harness, &host, request_for(RESOLVED), Some(RESOLVED), None);

    let result = harness.open_session(&host, request).await;

    assert_launched(&launcher, &["/opt/codex/bin/codex --version"]);
    assert_version_gate(result.as_ref().err(), OLD);
}

/// A receipt taken from a `PATH` lookup names no file, and `PATH` can resolve to another one inside
/// the receipt's window. It cannot vouch for a launch, so the probe runs.
#[tokio::test]
async fn a_receipt_with_no_resolved_path_still_probes() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(version_line(OLD));
    launcher.push(Transcript::load("turn").as_process());
    let host = host(&launcher);
    let harness = CodexHarness::new();
    let request = receipt_for(
        &harness,
        &host,
        OpenSession::new("chat-1"),
        None,
        Some(MINIMUM_CODEX_VERSION),
    );

    let result = harness.open_session(&host, request).await;

    assert_launched(&launcher, &["codex --version"]);
    assert_version_gate(result.as_ref().err(), OLD);
}

/// The launch uses the harness's own executable when the request names none; a receipt that names
/// no path is not about that file.
#[tokio::test]
async fn a_receipt_does_not_vouch_for_the_harnesss_own_executable() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(version_line(OLD));
    launcher.push(Transcript::load("turn").as_process());
    let host = host(&launcher);
    let harness = CodexHarness::new().with_executable(ExecutablePath::resolved(RESOLVED));
    let request = receipt_for(
        &harness,
        &host,
        OpenSession::new("chat-1"),
        None,
        Some(MINIMUM_CODEX_VERSION),
    );

    let result = harness.open_session(&host, request).await;

    assert_launched(&launcher, &["/opt/codex/bin/codex --version"]);
    assert_version_gate(result.as_ref().err(), OLD);
}

/// A receipt for another path is refused by the core before anything is launched.
#[tokio::test]
async fn a_receipt_for_another_path_is_refused_without_any_launch() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(Transcript::load("turn").as_process());
    let host = host(&launcher);
    let harness = CodexHarness::new();
    let request = receipt_for(
        &harness,
        &host,
        request_for(RESOLVED),
        Some(RESOLVED),
        Some(MINIMUM_CODEX_VERSION),
    )
    .with_executable(ExecutablePath::resolved("/usr/local/bin/codex"));

    let result = harness.open_session(&host, request).await;

    assert_launched(&launcher, &[]);
    assert!(
        matches!(
            result.as_ref().err().map(Error::cause),
            Some(Error::HostConfiguration { .. })
        ),
        "expected a host configuration refusal | received {:?}",
        result.as_ref().err()
    );
}

/// A shutdown that arrives while a silent binary holds the probe open does not start an app-server.
#[tokio::test(start_paused = true)]
async fn a_shutdown_during_a_hung_probe_starts_no_app_server() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(FakeProcess::responding(|_| Vec::new()));
    launcher.push(Transcript::load("turn").as_process());
    let cancel = CancelToken::new();
    let host = host_cancelled_by(&launcher, &cancel);
    let shutdown = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        shutdown.cancel();
    });

    let result = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await;

    assert_launched(&launcher, &["codex --version"]);
    assert!(
        matches!(
            result.as_ref().err().map(Error::cause),
            Some(Error::Cancelled { .. })
        ),
        "expected Cancelled | received {:?}",
        result.as_ref().err()
    );
    assert!(
        result
            .as_ref()
            .err()
            .is_some_and(|error| error.dispatch().is_safe_to_replay()),
        "expected a cancellation before any agent started to be safe to replay | received {:?}",
        result.as_ref().err().map(|error| error.dispatch())
    );
    assert_eq!(
        launcher.live_children(),
        0,
        "expected the hung probe reaped | received {} live children",
        launcher.live_children()
    );
}

/// A host already shut down gets no app-server even when the probe answered at once.
#[tokio::test]
async fn a_host_that_was_shut_down_starts_no_app_server() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(version_line(MINIMUM_CODEX_VERSION));
    launcher.push(Transcript::load("turn").as_process());
    let cancel = CancelToken::new();
    cancel.cancel();
    let host = host_cancelled_by(&launcher, &cancel);

    let result = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await;

    assert_launched(&launcher, &["codex --version"]);
    assert!(
        matches!(
            result.as_ref().err().map(Error::cause),
            Some(Error::Cancelled { .. })
        ),
        "expected Cancelled | received {:?}",
        result.as_ref().err()
    );
    assert!(
        result
            .as_ref()
            .err()
            .is_some_and(|error| error.dispatch().is_safe_to_replay()),
        "expected a cancellation before any agent started to be safe to replay | received {:?}",
        result.as_ref().err().map(|error| error.dispatch())
    );
}

/// The documented rule for a probe that never answers: it is `Unknown`, the open proceeds, and the
/// wait is the host's `request_timeout` plus the process stop, no more.
#[tokio::test(start_paused = true)]
async fn a_hung_probe_alone_proceeds_within_the_stated_bound() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(FakeProcess::responding(|_| Vec::new()));
    launcher.push(Transcript::load("turn").as_process());
    let host = host(&launcher);
    let limits = *host.limits();
    let bound = limits.request_timeout + Deadlines::from_limits(&limits).process_stop();

    let started = tokio::time::Instant::now();
    let result = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await;
    let waited = started.elapsed();

    assert_launched(&launcher, &["codex --version", "codex app-server"]);
    assert!(
        result.is_ok(),
        "expected a session | received {:?}",
        result.as_ref().err()
    );
    assert!(
        waited >= limits.request_timeout && waited <= bound,
        "expected a wait between {:?} and {bound:?} | received {waited:?}",
        limits.request_timeout
    );
    assert_eq!(launcher.live_children(), 1, "expected only the app-server");
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

/// Starting an executable establishes its presence even if its banner cannot establish a version.
#[tokio::test]
async fn an_installed_unparseable_or_empty_version_is_unknown_not_absent() {
    for banner in ["codex-cli unknown", "", "a changed banner"] {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::transcript([banner]));
        let found = CodexHarness::new()
            .discover(&host(&launcher))
            .await
            .expect("discovery");
        assert_eq!(
            found.gate,
            GateVerdict::Unknown,
            "installed banner {banner:?}"
        );
        assert_eq!(found.version, None);
        assert_eq!(launcher.live_children(), 0);
        assert_launched(&launcher, &["codex --version"]);
    }
    let launcher = Arc::new(FakeLauncher::new());
    let found = CodexHarness::new()
        .discover(&host(&launcher))
        .await
        .expect("discovery");
    assert_eq!(found.gate, GateVerdict::NotInstalled);
}

/// A named source whose installed process cannot supply a readable version stream.
struct UnreadableVersion;
#[async_trait::async_trait]
impl mango_external_agents::ByteSource for UnreadableVersion {
    async fn next_chunk(&mut self) -> mango_external_agents::Result<Option<Vec<u8>>> {
        Err(Error::Link {
            peer: String::from("version stdout"),
            message: String::from("a read failure (BrokenPipe)"),
        })
    }
}
struct UnreadableVersionLauncher(Arc<FakeLauncher>);
#[async_trait::async_trait]
impl mango_external_agents::ProcessLauncher for UnreadableVersionLauncher {
    async fn spawn(
        &self,
        spec: mango_external_agents::LaunchSpec,
    ) -> mango_external_agents::Result<mango_external_agents::ManagedProcess> {
        let mut child = self.0.spawn(spec).await?;
        child.stdout = Box::new(UnreadableVersion);
        Ok(child)
    }
}

#[tokio::test]
async fn an_installed_unreadable_or_silent_probe_is_unknown_and_reaped() {
    for unreadable in [true, false] {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::responding(|_| Vec::new()));
        let port: Arc<dyn mango_external_agents::ProcessLauncher> = if unreadable {
            Arc::new(UnreadableVersionLauncher(Arc::clone(&launcher)))
        } else {
            launcher.clone()
        };
        let host = HostContext::builder()
            .launcher(port)
            .cwd(workspace_path())
            .client_info("test", "0.0.1")
            .limits(mango_external_agents::Limits {
                request_timeout: Duration::from_millis(1),
                ..Default::default()
            })
            .build()
            .expect("host");
        let found = CodexHarness::new()
            .discover(&host)
            .await
            .expect("discovery");
        assert_eq!(found.gate, GateVerdict::Unknown);
        assert_eq!(launcher.live_children(), 0);
    }
}
