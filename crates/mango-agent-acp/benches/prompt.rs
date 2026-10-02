//! Real ACP prompt preflight, serialization, writer, and turn drain against the named fake agent.
//! Session opening and input construction are outside the clock; final cleanup is included.

mod support;

use std::sync::Arc;

use mango_agent_acp::testing::FakeAcpAgent;
use mango_agent_acp::{AcpHarness, AcpProfile};
use mango_external_agents::testing::FakeLauncher;
use mango_external_agents::{
    CloseReason, EventKind, Harness, HostContext, Limits, OpenSession, Session, TurnRequest,
    VendorInfo,
};
use support::{Bench, Unit};

/// Prepares a fresh session and requests at the unchanged default byte budget.
fn prepare(
    runtime: &tokio::runtime::Runtime,
    bytes: usize,
    turns: usize,
) -> (Box<dyn Session>, FakeLauncher, Vec<TurnRequest>) {
    let launcher = FakeLauncher::new();
    launcher.push(FakeAcpAgent::new().process());
    let host = HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("prompt-benchmark", "1.0")
        .limits(Limits::default())
        .build()
        .expect("benchmark host");
    let profile = AcpProfile::custom(
        "fake",
        ["fake-acp", "acp"],
        VendorInfo {
            company: "Fixture",
            terms_url: "https://example.invalid/terms",
            privacy_url: "https://example.invalid/privacy",
            skills_are_slash_commands: false,
        },
    );
    let session = runtime
        .block_on(AcpHarness::new(Arc::new(profile)).open_session(&host, OpenSession::new("bench")))
        .expect("benchmark session");
    let requests = (0..turns)
        .map(|index| TurnRequest::new(format!("turn-{index}"), "a".repeat(bytes)))
        .collect();
    (session, launcher, requests)
}

/// Completes every submitted prompt through the actual writer and verifies child cleanup.
async fn submit_and_drain(
    session: Box<dyn Session>,
    launcher: FakeLauncher,
    requests: Vec<TurnRequest>,
) -> usize {
    let expected = requests.len();
    let mut completed = 0;
    for request in requests {
        let mut stream = session.start_turn(request).await.expect("accepted prompt");
        let mut terminal = None;
        while let Some(event) = stream.recv().await {
            if event.is_terminal() {
                terminal = Some(event.kind);
            }
        }
        assert!(
            matches!(terminal, Some(EventKind::Completed)),
            "expected a completed prompt, received {terminal:?}"
        );
        completed += 1;
    }
    session
        .close(CloseReason::Shutdown)
        .await
        .expect("benchmark cleanup");
    assert_eq!(launcher.live_children(), 0);
    assert_eq!(completed, expected);
    completed
}

fn main() {
    let bench = Bench::new("acp/prompt");
    let runtime = support::runtime();
    for (name, bytes, turns) in [
        ("prompt/4KiB", 4 * 1024, 100),
        ("prompt/1MiB", 1024 * 1024, 16),
    ] {
        bench.run(
            name,
            Unit::new(turns as u64, "turn"),
            || prepare(&runtime, bytes, turns),
            |(session, launcher, requests)| {
                runtime.block_on(submit_and_drain(session, launcher, requests))
            },
        );
    }
}
