//! A floor a profile pins is applied before the agent is started.
//!
//! `AcpProfile::with_minimum_version` documents that it refuses older builds. Discovery did; a
//! session open started the agent first and never looked. These tests assert the launch log first:
//! what was started matters more than which error came back.
#![cfg(feature = "testing")]

use std::sync::Arc;
use std::time::SystemTime;

use mango_agent_acp::testing::FakeAcpAgent;
use mango_agent_acp::{AcpHarness, AcpProfile};
use mango_external_agents::testing::{FakeLauncher, FakeProcess};
use mango_external_agents::{
    ConfigurationChange, ConfigurationPatch, Discovery, DiscoveryReceipt,
    DiscoveryReceiptMeasurements, Error, Harness, HarnessId, HostContext, OpenSession,
    PermissionLevel, VendorInfo,
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
    HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
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

/// A receipt for a probe that saw `version`, bound to `request` the way a host binds one.
fn with_receipt(
    harness: &AcpHarness,
    host: &HostContext,
    request: OpenSession,
    version: Option<&str>,
) -> OpenSession {
    let discovery = Discovery {
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
async fn a_receipt_carrying_an_outdated_version_refuses_without_any_launch() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeAcpAgent::new().process());
    let harness = AcpHarness::new(profile(Some(FLOOR)));
    let host = host(&launcher);
    let request = with_receipt(&harness, &host, request(), Some("1.9.0"));

    let result = harness.open_session(&host, request).await;

    assert_launched(&launcher, &[]);
    assert_version_gate(result.as_ref().err(), "1.9.0");
}

#[tokio::test]
async fn a_receipt_carrying_a_current_version_skips_the_version_probe() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeAcpAgent::new().process());
    let harness = AcpHarness::new(profile(Some(FLOOR)));
    let host = host(&launcher);
    let request = with_receipt(&harness, &host, request(), Some("2.1.0"));

    let result = harness.open_session(&host, request).await;

    assert_launched(&launcher, &["fake-acp acp"]);
    assert!(
        result.is_ok(),
        "expected a session | received {:?}",
        result.as_ref().err()
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
