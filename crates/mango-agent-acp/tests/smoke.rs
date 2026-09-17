//! One real turn against an ACP agent installed on this machine.
//!
//! `#[ignore]`d, so CI never runs it: it spawns a third party's binary, needs whatever that binary is
//! signed in to, and costs tokens. It is how a profile earns
//! [`AcpProfile::verified`](mango_agent_acp::AcpProfile), and how the next person re-checks one when an
//! agent ships a new build.
//!
//! ```text
//! MEA_ACP_PROFILE=cursor cargo test -p mango-agent-acp --all-features \
//!     --test smoke -- --ignored --nocapture
//! ```
//!
//! It asserts only what every conformant ACP agent owes a client — a session with a native id, a turn
//! that ends exactly once, some assistant text — because anything narrower would be an assertion about
//! one agent's model rather than about this harness.

use std::sync::Arc;
use std::time::Duration;

use mango_external_agents::{
    CloseReason, ConfigurationChange, ConfigurationPatch, EnvSource, EventKind, Harness,
    HostContext, OpenSession, PermissionLevel, TurnRequest,
};

/// Which profile to drive, from the environment. `cursor` unless told otherwise.
fn profile_id() -> String {
    std::env::var("MEA_ACP_PROFILE").unwrap_or_else(|_| String::from("cursor"))
}

/// How long the agent is given to answer one short prompt.
const TURN_BUDGET: Duration = Duration::from_secs(180);

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns a real ACP agent; run explicitly with --ignored"]
async fn one_real_turn_against_an_installed_agent() {
    let id = profile_id();
    let harness = mango_agent_acp::AcpHarness::builtin(&id)
        .unwrap_or_else(|| panic!("expected a built-in profile, received {id:?}"));

    let host = HostContext::builder()
        .launcher(Arc::new(
            mango_external_agents::launcher::TokioLauncher::new(),
        ))
        .cwd(std::env::current_dir().expect("expected a working directory"))
        // The real environment, through the allowlist: this is a host, and a host is what a smoke test
        // has to be.
        .environment(EnvSource::from_process())
        .client_info("mea-smoke", env!("CARGO_PKG_VERSION"))
        .build()
        .expect("expected a host");

    let discovery = harness
        .discover(&host)
        .await
        .expect("expected a discovery, received a failure");
    eprintln!("discovery: {discovery:?}");
    assert!(
        discovery.is_usable(),
        "expected {id} to be installed and usable on this machine, received {:?}",
        discovery.gate
    );

    let session = harness
        .open_session(
            &host,
            OpenSession::new("smoke-1").with_configuration(
                // Not `ReadOnly`: a read-only session refuses every request the agent raises, and a
                // smoke test that refused its own agent's tools would prove less than it looks.
                ConfigurationPatch::new().level(ConfigurationChange::Set(PermissionLevel::Default)),
            ),
        )
        .await
        .expect("expected a session, received a failure");
    eprintln!("ids: {:?}", session.ids());
    eprintln!("capabilities: {:?}", session.capabilities());
    assert!(!session.ids().native_session_id.trim().is_empty());

    let mut turn = session
        .start_turn(TurnRequest::new(
            "smoke-turn-1",
            "Reply with exactly one word: pong",
        ))
        .await
        .expect("expected a turn, received a failure");

    let mut kinds = Vec::new();
    let read = tokio::time::timeout(TURN_BUDGET, async {
        while let Some(event) = turn.recv().await {
            eprintln!("event: {:?}", event.kind);
            let terminal = event.is_terminal();
            kinds.push(event.kind);
            if terminal {
                break;
            }
        }
    })
    .await;

    session
        .close(CloseReason::Requested)
        .await
        .expect("expected the close to land");

    assert!(
        read.is_ok(),
        "expected the turn to end within {TURN_BUDGET:?}, received {} events and no terminal",
        kinds.len()
    );
    assert!(
        matches!(kinds.first(), Some(EventKind::TurnStarted { .. })),
        "expected the turn to be named first, received {kinds:?}"
    );
    assert!(
        kinds
            .iter()
            .any(|kind| matches!(kind, EventKind::TextDelta { .. })),
        "expected the agent to say something, received {kinds:?}"
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| matches!(kind, EventKind::Completed | EventKind::Error { .. }))
            .count(),
        1,
        "expected exactly one terminal, received {kinds:?}"
    );
}
