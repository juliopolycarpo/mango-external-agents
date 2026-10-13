//! A floor a profile pins is applied before the agent is started.
//!
//! `AcpProfile::with_minimum_version` documents that it refuses older builds. Discovery did; a
//! session open started the agent first and never looked. These tests assert the launch log first:
//! what was started matters more than which error came back.
#![cfg(feature = "testing")]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use mango_agent_acp::testing::FakeAcpAgent;
use mango_agent_acp::{AcpHarness, AcpProfile};
use mango_external_agents::testing::{FakeLauncher, FakeProcess};
use mango_external_agents::{
    CancelToken, ConfigurationChange, ConfigurationPatch, Discovery, DiscoveryReceipt,
    DiscoveryReceiptMeasurements, Error, ExecutablePath, GateVerdict, Harness, HarnessId,
    HostContext, Limits, OpenSession, PermissionLevel, VendorInfo,
};

const VENDOR: VendorInfo = VendorInfo {
    company: "Nobody",
    terms_url: "https://example.invalid/terms",
    privacy_url: "https://example.invalid/privacy",
    skills_are_slash_commands: false,
};

const FLOOR: &str = "2.0.0";

fn profile(floor: Option<&str>) -> Arc<AcpProfile> {
    let profile = AcpProfile::custom("fake", ["fake-acp", "acp"], VENDOR);
    Arc::new(match floor {
        Some(floor) => profile.with_minimum_version(floor),
        None => profile,
    })
}

fn host(launcher: &FakeLauncher) -> HostContext {
    host_cancelled_by(launcher, &CancelToken::new())
}

fn host_cancelled_by(launcher: &FakeLauncher, cancel: &CancelToken) -> HostContext {
    HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cancel(cancel.clone())
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .build()
        .expect("expected a host")
}

fn request() -> OpenSession {
    OpenSession::new("chat-1").with_configuration(
        ConfigurationPatch::new().level(ConfigurationChange::Set(PermissionLevel::Default)),
    )
}

/// What `fake-acp --version` prints for `version`.
fn version_line(version: &str) -> FakeProcess {
    FakeProcess::transcript([format!("fake-acp {version}")])
}

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
                if actual == found && minimum == FLOOR
        ),
        "expected VersionGate {{ found: {found}, minimum: {FLOOR} }} | received {error:?}"
    );
}

/// A receipt for a probe of `executable` that saw `version`, bound to `request` as a host binds one.
fn with_receipt(
    harness: &AcpHarness,
    host: &HostContext,
    request: OpenSession,
    executable: Option<&str>,
    version: Option<&str>,
) -> OpenSession {
    let discovery = Discovery {
        executable: executable.map(PathBuf::from),
        version: version.map(str::to_owned),
        ..Discovery::not_installed()
    };
    let receipt = DiscoveryReceipt::new(
        HarnessId::new("acp:fake").expect("expected a harness id"),
        discovery,
        SystemTime::now(),
    )
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
    request().with_executable(ExecutablePath::resolved(path))
}

const RESOLVED: &str = "/opt/fake/bin/fake-acp";

/// The most a probe may hold an open: the host's `request_timeout` plus a full process stop (two
/// `kill_grace` and two `shutdown_timeout`).
fn probe_bound(limits: &Limits) -> Duration {
    limits.request_timeout + limits.kill_grace * 2 + limits.shutdown_timeout * 2
}

#[tokio::test]
async fn an_outdated_agent_is_refused_without_starting_it() {
    let launcher = FakeLauncher::new();
    launcher.push(version_line("1.9.0"));
    launcher.push(FakeAcpAgent::new().process());

    let result = AcpHarness::new(profile(Some(FLOOR)))
        .open_session(&host(&launcher), request())
        .await;

    assert_launched(&launcher, &["fake-acp --version"]);
    assert_version_gate(result.as_ref().err(), "1.9.0");
    assert_eq!(
        launcher.live_children(),
        0,
        "expected the version probe reaped | received {} live children",
        launcher.live_children()
    );
}

#[tokio::test]
async fn a_current_agent_costs_one_version_probe_and_one_agent() {
    let launcher = FakeLauncher::new();
    launcher.push(version_line("2.0.0"));
    launcher.push(FakeAcpAgent::new().process());

    let result = AcpHarness::new(profile(Some(FLOOR)))
        .open_session(&host(&launcher), request())
        .await;

    assert_launched(&launcher, &["fake-acp --version", "fake-acp acp"]);
    assert!(
        result.is_ok(),
        "expected a session | received {:?}",
        result.as_ref().err()
    );
}

/// No built-in profile pins a floor, and a profile with none pays for no probe at all.
#[tokio::test]
async fn a_profile_without_a_floor_starts_only_the_agent() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeAcpAgent::new().process());

    let result = AcpHarness::new(profile(None))
        .open_session(&host(&launcher), request())
        .await;

    assert_launched(&launcher, &["fake-acp acp"]);
    assert!(
        result.is_ok(),
        "expected a session | received {:?}",
        result.as_ref().err()
    );
}

/// A version nobody can read is `Unknown`, which proceeds: an agent that changed the shape of
/// `--version` has not stopped working.
#[tokio::test]
async fn an_unreadable_version_proceeds_to_the_agent() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeProcess::transcript(["fake-acp nightly"]));
    launcher.push(FakeAcpAgent::new().process());

    let result = AcpHarness::new(profile(Some(FLOOR)))
        .open_session(&host(&launcher), request())
        .await;

    assert_launched(&launcher, &["fake-acp --version", "fake-acp acp"]);
    assert!(
        result.is_ok(),
        "expected a session | received {:?}",
        result.as_ref().err()
    );
}

#[tokio::test]
async fn a_receipt_for_the_launched_path_carrying_an_outdated_version_refuses_without_any_launch() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeAcpAgent::new().process());
    let harness = AcpHarness::new(profile(Some(FLOOR)));
    let host = host(&launcher);
    let request = with_receipt(
        &harness,
        &host,
        request_for(RESOLVED),
        Some(RESOLVED),
        Some("1.9.0"),
    );

    let result = harness.open_session(&host, request).await;

    assert_launched(&launcher, &[]);
    assert_version_gate(result.as_ref().err(), "1.9.0");
}

#[tokio::test]
async fn a_receipt_for_the_launched_path_carrying_a_current_version_skips_the_version_probe() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeAcpAgent::new().process());
    let harness = AcpHarness::new(profile(Some(FLOOR)));
    let host = host(&launcher);
    let request = with_receipt(
        &harness,
        &host,
        request_for(RESOLVED),
        Some(RESOLVED),
        Some("2.1.0"),
    );

    let result = harness.open_session(&host, request).await;

    assert_launched(&launcher, &["/opt/fake/bin/fake-acp acp"]);
    assert!(
        result.is_ok(),
        "expected a session | received {:?}",
        result.as_ref().err()
    );
}

/// A receipt that learned no version vouches for nothing: the probe runs, and what it finds decides.
#[tokio::test]
async fn a_receipt_without_a_version_still_probes() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeProcess::transcript(["fake-acp 1.9.0"]));
    launcher.push(FakeAcpAgent::new().process());
    let harness = AcpHarness::new(profile(Some(FLOOR)));
    let host = host(&launcher);
    let request = with_receipt(&harness, &host, request_for(RESOLVED), Some(RESOLVED), None);

    let result = harness.open_session(&host, request).await;

    assert_launched(&launcher, &["/opt/fake/bin/fake-acp --version"]);
    assert_version_gate(result.as_ref().err(), "1.9.0");
}

/// A receipt taken from a `PATH` lookup names no file, and `PATH` can resolve to another one inside
/// the receipt's window. ACP has no handshake to catch that afterwards, so the probe runs.
#[tokio::test]
async fn a_receipt_with_no_resolved_path_still_probes() {
    let launcher = FakeLauncher::new();
    launcher.push(version_line("1.9.0"));
    launcher.push(FakeAcpAgent::new().process());
    let harness = AcpHarness::new(profile(Some(FLOOR)));
    let host = host(&launcher);
    let request = with_receipt(&harness, &host, request(), None, Some("2.1.0"));

    let result = harness.open_session(&host, request).await;

    assert_launched(&launcher, &["fake-acp --version"]);
    assert_version_gate(result.as_ref().err(), "1.9.0");
}

/// The launch uses the harness's own executable when the request names none; a receipt that names
/// no path is not about that file.
#[tokio::test]
async fn a_receipt_does_not_vouch_for_the_harnesss_own_executable() {
    let launcher = FakeLauncher::new();
    launcher.push(version_line("1.9.0"));
    launcher.push(FakeAcpAgent::new().process());
    let harness =
        AcpHarness::new(profile(Some(FLOOR))).with_executable(ExecutablePath::resolved(RESOLVED));
    let host = host(&launcher);
    let request = with_receipt(&harness, &host, request(), None, Some("2.1.0"));

    let result = harness.open_session(&host, request).await;

    assert_launched(&launcher, &["/opt/fake/bin/fake-acp --version"]);
    assert_version_gate(result.as_ref().err(), "1.9.0");
}

/// A receipt for another path is refused by the core before anything is launched.
#[tokio::test]
async fn a_receipt_for_another_path_is_refused_without_any_launch() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeAcpAgent::new().process());
    let harness = AcpHarness::new(profile(Some(FLOOR)));
    let host = host(&launcher);
    let request = with_receipt(
        &harness,
        &host,
        request_for(RESOLVED),
        Some(RESOLVED),
        Some("2.1.0"),
    )
    .with_executable(ExecutablePath::resolved("/usr/local/bin/fake-acp"));

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

/// A shutdown that arrives while a silent binary holds the probe open does not start the agent.
#[tokio::test(start_paused = true)]
async fn a_shutdown_during_a_hung_probe_starts_no_agent() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeProcess::responding(|_| Vec::new()));
    launcher.push(FakeAcpAgent::new().process());
    let cancel = CancelToken::new();
    let host = host_cancelled_by(&launcher, &cancel);
    let shutdown = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        shutdown.cancel();
    });

    let result = AcpHarness::new(profile(Some(FLOOR)))
        .open_session(&host, request())
        .await;

    assert_launched(&launcher, &["fake-acp --version"]);
    assert!(
        matches!(
            result.as_ref().err().map(Error::cause),
            Some(Error::Cancelled { .. })
        ),
        "expected Cancelled | received {:?}",
        result.as_ref().err()
    );
    assert_eq!(
        launcher.live_children(),
        0,
        "expected the hung probe reaped | received {} live children",
        launcher.live_children()
    );
}

/// The documented rule for a probe that never answers: it is `Unknown`, the open proceeds, and the
/// wait is the host's `request_timeout` plus the process stop, no more.
#[tokio::test(start_paused = true)]
async fn a_hung_probe_alone_proceeds_within_the_stated_bound() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeProcess::responding(|_| Vec::new()));
    launcher.push(FakeAcpAgent::new().process());
    let host = host(&launcher);
    let limits = *host.limits();

    let started = tokio::time::Instant::now();
    let result = AcpHarness::new(profile(Some(FLOOR)))
        .open_session(&host, request())
        .await;
    let waited = started.elapsed();

    assert_launched(&launcher, &["fake-acp --version", "fake-acp acp"]);
    assert!(
        result.is_ok(),
        "expected a session | received {:?}",
        result.as_ref().err()
    );
    assert!(
        waited >= limits.request_timeout && waited <= probe_bound(&limits),
        "expected a wait between {:?} and {:?} | received {waited:?}",
        limits.request_timeout,
        probe_bound(&limits)
    );
}

/// A probe that never answers and one that prints only a banner both end at the host's
/// `request_timeout`, as an unknown version, with no probe child left running.
#[tokio::test(start_paused = true)]
async fn a_probe_that_times_out_is_reaped_and_reports_unknown() {
    let silent = FakeProcess::responding(|_| Vec::new());
    let banner_only =
        FakeProcess::responding(|_| Vec::new()).with_greeting(["welcome, no version"]);
    for (label, process) in [("silent", silent), ("banner only", banner_only)] {
        let launcher = FakeLauncher::new();
        launcher.push(process);

        let discovery = AcpHarness::new(profile(Some(FLOOR)))
            .discover(&host(&launcher))
            .await
            .expect("expected a discovery");

        assert_eq!(
            discovery.gate,
            GateVerdict::Unknown,
            "expected a {label} probe to gate as unknown | received {:?}",
            discovery.gate
        );
        assert_eq!(
            launcher.live_children(),
            0,
            "expected the {label} probe child reaped | received {} live",
            launcher.live_children()
        );
    }
}

/// Dropping an open that is waiting on the probe still stops and reaps the probe child.
#[tokio::test(start_paused = true)]
async fn dropping_an_open_during_the_probe_reaps_the_probe_child() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeProcess::responding(|_| Vec::new()));
    launcher.push(FakeAcpAgent::new().process());
    let host = host(&launcher);
    let harness = AcpHarness::new(profile(Some(FLOOR)));

    let abandoned = tokio::time::timeout(
        Duration::from_secs(1),
        harness.open_session(&host, request()),
    )
    .await;
    assert!(
        abandoned.is_err(),
        "expected the open to still be waiting on the probe"
    );
    for _ in 0..100 {
        if launcher.live_children() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert_launched(&launcher, &["fake-acp --version"]);
    assert_eq!(
        launcher.live_children(),
        0,
        "expected the abandoned probe child reaped | received {} live",
        launcher.live_children()
    );
}

#[tokio::test]
async fn the_picker_refuses_an_outdated_agent_without_starting_it() {
    let launcher = FakeLauncher::new();
    launcher.push(version_line("1.9.0"));
    launcher.push(FakeAcpAgent::new().listing_sessions().process());

    let result = AcpHarness::new(profile(Some(FLOOR)))
        .list_sessions(&host(&launcher), Default::default())
        .await;

    assert_launched(&launcher, &["fake-acp --version"]);
    assert_version_gate(result.as_ref().err(), "1.9.0");
}
