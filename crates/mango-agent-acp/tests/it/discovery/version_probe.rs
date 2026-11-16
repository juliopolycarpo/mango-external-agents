//! The version probe reads a bounded amount of output and reports what it finds.
use super::*;
use mango_external_agents::testing::FakeProcess;

/// The most output the probe reads before it gives up on finding a version.
const PROBE_CAP_BYTES: usize = 64 * 1024;

/// A child that prints `banner_bytes` of version-less banner lines, then `tail`, and keeps its
/// stdout open like a hung agent, so only the probe's own kill can end it.
fn banner_then(banner_bytes: usize, tail: &str) -> FakeProcess {
    let line = "welcome to the agent, no version here";
    let lines = std::iter::repeat_n(line.to_owned(), banner_bytes / (line.len() + 1))
        .chain(std::iter::once(tail.to_owned()));
    FakeProcess::responding(|_| Vec::new()).with_greeting(lines)
}

async fn probe(process: FakeProcess) -> (mango_external_agents::Discovery, FakeLauncher) {
    let launcher = FakeLauncher::new();
    launcher.push(process);
    let discovery = AcpHarness::builtin("cursor")
        .expect("expected Cursor profile")
        .discover(&host(Arc::new(launcher.clone())))
        .await
        .expect("expected discovery to succeed for a working agent");
    (discovery, launcher)
}

#[tokio::test]
async fn a_version_after_several_banner_lines_is_still_read() {
    let process = FakeProcess::responding(|_| Vec::new()).with_greeting([
        "Cursor Agent",
        "Copyright (c) the vendor",
        "",
        "cursor-agent 2026.09.10 (linux-x64)",
    ]);
    let (discovery, launcher) = probe(process).await;
    assert_eq!(
        discovery.version.as_deref(),
        Some("2026.09.10"),
        "expected the version after the banner | received {:?}",
        discovery.version
    );
    assert_eq!(launcher.live_children(), 0, "expected the child reaped");
}

#[tokio::test]
async fn a_version_within_the_cap_after_a_large_banner_is_still_read() {
    let (discovery, _) = probe(banner_then(PROBE_CAP_BYTES / 2, "cursor-agent 2026.09.10")).await;
    assert_eq!(discovery.version.as_deref(), Some("2026.09.10"));
}

#[tokio::test]
async fn output_over_the_cap_reports_unknown_and_reaps_the_child() {
    let (discovery, launcher) =
        probe(banner_then(PROBE_CAP_BYTES * 2, "cursor-agent 2026.09.10")).await;
    assert_eq!(
        discovery.version, None,
        "expected no version once the probe passed {PROBE_CAP_BYTES} bytes | received {:?}",
        discovery.version
    );
    assert_eq!(
        discovery.gate,
        GateVerdict::Unknown,
        "expected an unreadable version to gate as unknown, not refuse a working agent"
    );
    assert_eq!(
        launcher.live_children(),
        0,
        "expected the probe child killed | received {} live",
        launcher.live_children()
    );
}

#[tokio::test]
async fn one_line_over_the_cap_reports_unknown_and_reaps_the_child() {
    let process = FakeProcess::responding(|_| Vec::new()).with_greeting([
        "x".repeat(PROBE_CAP_BYTES * 2),
        String::from("cursor-agent 2026.09.10"),
    ]);
    let (discovery, launcher) = probe(process).await;
    assert_eq!(
        discovery.version, None,
        "expected no version from a line longer than {PROBE_CAP_BYTES} bytes | received {:?}",
        discovery.version
    );
    assert_eq!(launcher.live_children(), 0, "expected the child reaped");
}
