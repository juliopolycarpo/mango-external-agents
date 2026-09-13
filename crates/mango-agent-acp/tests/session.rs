//! The harness driven end to end against [`FakeAcpAgent`], including the core conformance suite.
//!
//! Everything here goes through the real transport, the real dispatch loop and the real reducer; the
//! only thing replaced is the process, which the core's `FakeLauncher` hands over as scripted pipes.
//! That is deliberate: the parts most worth testing on this dialect are the ones the reducer's unit
//! tests cannot reach — that a turn ends exactly once, that an approval round trip lands, that a
//! cancel carries its reason, and that closing twice is not an error.

use std::sync::Arc;
use std::time::Duration;

use mango_agent_acp::testing::{Approval, FakeAcpAgent};
use mango_agent_acp::{AcpHarness, AcpProfile};
use mango_external_agents::testing::{FakeLauncher, RecordingBroker};
use mango_external_agents::{
    ApprovalRouting, BrokerDecision, CancelReason, CloseReason, Configuration, DecisionSource,
    Error, EventKind, Harness, HostContext, Limits, OpenSession, PermissionLevel, Session,
    TurnRequest, TurnStream, VendorInfo,
};

const VENDOR: VendorInfo = VendorInfo {
    company: "Nobody",
    terms_url: "https://example.invalid/terms",
    privacy_url: "https://example.invalid/privacy",
    skills_are_slash_commands: false,
};

fn profile() -> Arc<AcpProfile> {
    Arc::new(AcpProfile::custom("fake", ["fake-acp", "acp"], VENDOR))
}

fn host(launcher: &FakeLauncher) -> HostContext {
    HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .build()
        .expect("expected a host")
}

/// A host whose broker answers every approval, so nothing waits for a person.
fn host_with_broker(
    launcher: &FakeLauncher,
    decision: BrokerDecision,
) -> (HostContext, Arc<RecordingBroker>) {
    let broker = Arc::new(RecordingBroker::new(decision));
    let host = HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .broker(Arc::clone(&broker) as Arc<dyn mango_external_agents::PermissionBroker>)
        .build()
        .expect("expected a host");
    (host, broker)
}

fn permissive() -> Configuration {
    Configuration {
        level: PermissionLevel::Default,
        ..Configuration::default()
    }
}

/// Reads a turn to its terminal, or fails rather than hanging.
async fn drain(turn: &mut TurnStream) -> Vec<EventKind> {
    let collected = tokio::time::timeout(Duration::from_secs(10), async {
        let mut events = Vec::new();
        while let Some(event) = turn.recv().await {
            let terminal = event.is_terminal();
            events.push(event.kind);
            if terminal {
                break;
            }
        }
        events
    })
    .await;
    collected.expect("expected the turn to end rather than hang")
}

async fn open(
    agent: FakeAcpAgent,
    configuration: Configuration,
) -> (Box<dyn Session>, FakeLauncher) {
    let launcher = FakeLauncher::new();
    launcher.push(agent.process());
    let harness = AcpHarness::new(profile());
    let session = harness
        .open_session(
            &host(&launcher),
            OpenSession::new("chat-1").with_configuration(configuration),
        )
        .await
        .expect("expected a session");
    (session, launcher)
}

#[tokio::test]
async fn a_turn_streams_the_agents_updates_and_ends_exactly_once() {
    let (session, _launcher) = open(FakeAcpAgent::new(), permissive()).await;

    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "say hello"))
        .await
        .expect("expected a turn");
    let events = drain(&mut turn).await;

    assert!(
        matches!(events.first(), Some(EventKind::SessionStarted { native_session_id, .. }) if native_session_id == "sess_fake"),
        "expected the conversation to be named first, received {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|kind| matches!(kind, EventKind::TextDelta { text } if text == "hello")),
        "received {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|kind| matches!(kind, EventKind::CommandsAvailable { .. })),
        "received {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|kind| matches!(kind, EventKind::ThreadUsage { .. })),
        "received {events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|kind| matches!(kind, EventKind::Completed | EventKind::Error { .. }))
            .count(),
        1,
        "expected exactly one terminal, received {events:?}"
    );
    assert!(
        !turn.native_turn_id.is_empty(),
        "expected the prompt's own id"
    );
}

/// The whole point of brokering: the question reaches the host, the host answers, and the agent
/// finishes the turn because of that answer.
#[tokio::test]
async fn an_approval_reaches_the_host_and_answering_it_finishes_the_turn() {
    let (session, _launcher) = open(
        FakeAcpAgent::new().asking_for_approval(Approval::Once),
        permissive(),
    )
    .await;

    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "delete the build"))
        .await
        .expect("expected a turn");

    // Read until the question arrives, then answer it. The fake holds the prompt response until then,
    // so a turn that ends without this would mean nothing was waiting on the answer.
    let question = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = turn.recv().await {
            if let EventKind::ApprovalRequested { request } = event.kind {
                return request;
            }
        }
        panic!("expected an approval request");
    })
    .await
    .expect("expected the question rather than a hang");

    session
        .respond(question.deny().expect("expected a refusing option"))
        .await
        .expect("expected the answer to be accepted");

    let rest = drain(&mut turn).await;
    assert!(
        rest.iter().any(|kind| matches!(
            kind,
            EventKind::ApprovalResolved {
                decision,
                ..
            } if decision.source == DecisionSource::User
        )),
        "received {rest:?}"
    );
    assert!(
        matches!(rest.last(), Some(EventKind::Completed)),
        "received {rest:?}"
    );
}

/// A host policy answers without the question reaching a person, and the audit trail says it was the
/// policy rather than a user.
#[tokio::test]
async fn a_broker_answers_without_the_question_waiting_for_a_person() {
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeAcpAgent::new()
            .asking_for_approval(Approval::Once)
            .process(),
    );
    let (host, broker) = host_with_broker(
        &launcher,
        BrokerDecision::Deny {
            reason: String::from("read-only workspace"),
        },
    );

    let session = AcpHarness::new(profile())
        .open_session(
            &host,
            OpenSession::new("chat-1").with_configuration(permissive()),
        )
        .await
        .expect("expected a session");
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "delete the build"))
        .await
        .expect("expected a turn");

    let events = drain(&mut turn).await;
    assert_eq!(
        broker.requests().len(),
        1,
        "expected the broker to be asked"
    );
    assert!(
        events.iter().any(|kind| matches!(
            kind,
            EventKind::ApprovalResolved { decision, .. } if decision.source == DecisionSource::AutoReview
        )),
        "received {events:?}"
    );
    assert!(
        matches!(events.last(), Some(EventKind::Completed)),
        "expected the turn to finish on the policy's answer, received {events:?}"
    );
}

/// The one level the library reaches by answering. Refusing grants nothing, and the host asked for it
/// — which is why there is no counterpart that allows.
#[tokio::test]
async fn a_read_only_session_refuses_every_request_without_asking_anyone() {
    let (session, _launcher) = open(
        FakeAcpAgent::new().asking_for_approval(Approval::Once),
        Configuration::default(),
    )
    .await;
    assert_eq!(
        session.info().effective_configuration.level,
        PermissionLevel::ReadOnly
    );

    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "delete the build"))
        .await
        .expect("expected a turn");
    let events = drain(&mut turn).await;

    let resolved = events
        .iter()
        .find_map(|kind| match kind {
            EventKind::ApprovalResolved { decision, .. } => Some(decision),
            _ => None,
        })
        .expect("expected the question to be resolved");
    assert_eq!(decision_option(resolved), "reject");
    assert_eq!(resolved.source, DecisionSource::AutoReview);
    // The host still saw what was asked: a standing refusal is not a reason to hide the question.
    assert!(
        events
            .iter()
            .any(|kind| matches!(kind, EventKind::ApprovalRequested { .. })),
        "received {events:?}"
    );
}

fn decision_option(decision: &mango_external_agents::ApprovalDecision) -> &str {
    &decision.option_id
}

/// An agent that offers nothing to refuse with leaves nothing for a standing refusal to pick, so the
/// question has to reach a person rather than being answered with whatever was first in the list.
#[tokio::test]
async fn a_read_only_session_still_asks_when_the_agent_offered_no_way_to_refuse() {
    let (session, _launcher) = open(
        FakeAcpAgent::new().asking_for_approval(Approval::OnlyAllows),
        Configuration::default(),
    )
    .await;

    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "delete the build"))
        .await
        .expect("expected a turn");

    let asked = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = turn.recv().await {
            if matches!(event.kind, EventKind::ApprovalRequested { .. }) {
                return true;
            }
            if event.is_terminal() {
                return false;
            }
        }
        false
    })
    .await
    .expect("expected an answer rather than a hang");

    assert!(
        asked,
        "expected the question to reach the host when nothing could refuse it"
    );
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected the close to land");
}

/// ACP answers a cancelled prompt with `stop_reason: cancelled` and no reason of its own, so the
/// reason the host gave has to survive the round trip. Flattening it would report a shutdown as "you
/// stopped this turn".
#[tokio::test]
async fn a_cancelled_turn_reports_the_reason_the_host_gave_and_still_completes() {
    let (session, _launcher) = open(
        FakeAcpAgent::new().asking_for_approval(Approval::Once),
        permissive(),
    )
    .await;

    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "take your time"))
        .await
        .expect("expected a turn");

    // Cancel once the agent is mid-turn: the fake holds its prompt response open until answered or
    // cancelled, so this is the real race rather than a cancel against a finished turn.
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = turn.recv().await {
            if matches!(event.kind, EventKind::ApprovalRequested { .. }) {
                return;
            }
        }
    })
    .await
    .expect("expected the agent to get going");

    session
        .cancel(CancelReason::ConsentRevoked)
        .await
        .expect("expected the cancel to land");

    let events = drain(&mut turn).await;
    assert!(
        events.iter().any(|kind| matches!(
            kind,
            EventKind::Cancelled {
                reason: CancelReason::ConsentRevoked
            }
        )),
        "received {events:?}"
    );
    assert!(
        matches!(events.last(), Some(EventKind::Completed)),
        "expected the marker to be followed by a terminal, received {events:?}"
    );
}

/// ACP v1 runs one `session/prompt` at a time: the response *is* the turn's end, so two prompts would
/// race for one stream of updates with nothing on the wire to tell them apart.
#[tokio::test]
async fn a_second_turn_is_refused_while_one_is_in_flight() {
    let (session, _launcher) = open(
        FakeAcpAgent::new().asking_for_approval(Approval::Once),
        permissive(),
    )
    .await;

    let _first = session
        .start_turn(TurnRequest::new("turn-1", "one"))
        .await
        .expect("expected a turn");
    let error = session
        .start_turn(TurnRequest::new("turn-2", "two"))
        .await
        .expect_err("expected a refusal, received a second turn");

    assert!(
        matches!(&error, Error::Protocol { expected, .. } if expected.contains("one session/prompt")),
        "received {error:?}"
    );
}

#[tokio::test]
async fn closing_twice_is_not_an_error_and_a_turn_after_it_is_refused() {
    let (session, _launcher) = open(FakeAcpAgent::new().closing_sessions(), permissive()).await;

    session
        .close(CloseReason::Requested)
        .await
        .expect("expected the first close to land");
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected the second close to be accepted");

    let error = session
        .start_turn(TurnRequest::new("turn-1", "still there?"))
        .await
        .expect_err("expected a closed session");
    assert!(
        matches!(error, Error::Closed { subject: "session" }),
        "received {error:?}"
    );
}

/// A close must not park behind a host that stopped reading its own stream. The channel is sized to
/// exactly what one turn emits before the agent asks, so it is full and unread when `close` runs.
#[tokio::test(start_paused = true)]
async fn closing_returns_even_with_a_full_turn_channel_nobody_is_reading() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeAcpAgent::new().process());
    let host = HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .limits(Limits {
            turn_channel_capacity: 1,
            ..Limits::default()
        })
        .build()
        .expect("expected a host");

    let session = AcpHarness::new(profile())
        .open_session(
            &host,
            OpenSession::new("chat-1").with_configuration(permissive()),
        )
        .await
        .expect("expected a session");
    let turn = session
        .start_turn(TurnRequest::new("turn-1", "fill the channel"))
        .await
        .expect("expected a turn");

    // Held, not dropped: a dropped receiver closes the sink and `emit` returns instead of parking,
    // which would make this test pass without testing anything.
    let mut held = turn;
    let closed = tokio::time::timeout(
        Duration::from_secs(30),
        session.close(CloseReason::Shutdown),
    )
    .await
    .expect("expected close to return rather than park on a full channel");
    closed.expect("expected the close to succeed");

    // And the stream the host abandoned ends, rather than staying open for a turn nothing is driving.
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        while held.recv().await.is_some() {}
    })
    .await;
    assert!(ended.is_ok(), "expected the abandoned stream to end");
}

/// Listing is the agent's to offer. An agent that never advertised `session/list` gets the trait's
/// typed refusal rather than a request it would answer with "method not found".
#[tokio::test]
async fn session_listing_follows_what_the_agent_advertised() {
    let (silent, _launcher) = open(FakeAcpAgent::new(), permissive()).await;
    assert!(!silent.info().capabilities.session_listing);
    let error = silent
        .list_sessions(Default::default())
        .await
        .expect_err("expected a refusal");
    assert!(
        matches!(error, Error::NotSupported { .. }),
        "received {error:?}"
    );

    let (listing, _launcher) = open(FakeAcpAgent::new().listing_sessions(), permissive()).await;
    assert!(listing.info().capabilities.session_listing);
    let page = listing
        .list_sessions(Default::default())
        .await
        .expect("expected a page");
    assert_eq!(page.sessions.len(), 1);
    assert_eq!(page.sessions[0].native_session_id, "sess_old");
    assert_eq!(page.sessions[0].title.as_deref(), Some("Yesterday"));
}

/// The library never sends `authenticate`: signing in is the agent's own flow, in the user's own
/// terminal. All it does with `-32000` is say which command a person should run.
#[tokio::test]
async fn a_signed_out_agent_yields_the_profiles_own_login_command_and_no_authenticate_call() {
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeAcpAgent::new()
            .refusing_new_session(-32_000, "sign in first")
            .process(),
    );
    let signed_out = Arc::new(
        AcpProfile::custom("fake", ["fake-acp", "acp"], VENDOR).with_login_hint("fake-acp login"),
    );

    let error = AcpHarness::new(signed_out)
        .open_session(&host(&launcher), OpenSession::new("chat-1"))
        .await
        .err()
        .expect("expected a refusal, received a session");

    assert!(
        matches!(&error, Error::AuthRequired { login_hint } if login_hint == "fake-acp login"),
        "received {error:?}"
    );
    assert!(
        !launcher
            .written()
            .iter()
            .any(|line| line.contains("authenticate")),
        "expected no authenticate request, received {:?}",
        launcher.written()
    );
}

/// The host owns files and terminals, so the handshake declines both. An agent reading this knows to
/// use its own tools, which is what the activity events then describe.
#[tokio::test]
async fn the_handshake_declines_the_filesystem_and_terminal_and_names_the_host() {
    let (session, launcher) = open(FakeAcpAgent::new(), permissive()).await;
    let initialize = launcher
        .written()
        .into_iter()
        .find(|line| line.contains("\"initialize\""))
        .expect("expected an initialize request");
    let sent: serde_json::Value =
        serde_json::from_str(&initialize).expect("expected valid JSON-RPC");
    let capabilities = &sent["params"]["clientCapabilities"];

    assert_eq!(sent["params"]["protocolVersion"], 1);
    assert_eq!(sent["params"]["clientInfo"]["name"], "mea-tests");
    assert_eq!(capabilities["fs"]["readTextFile"], false);
    assert_eq!(capabilities["fs"]["writeTextFile"], false);
    assert_eq!(capabilities["terminal"], false);
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected the close to land");
}

/// Refused before a session exists: negotiating down would mean sending v1 messages to an agent that
/// answered something else.
#[tokio::test]
async fn an_agent_answering_another_protocol_version_is_refused() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeAcpAgent::new().with_protocol_version(2).process());

    let error = AcpHarness::new(profile())
        .open_session(&host(&launcher), OpenSession::new("chat-1"))
        .await
        .err()
        .expect("expected a refusal, received a session");
    assert!(
        matches!(&error, Error::Protocol { expected, .. } if expected.contains("protocol version 1")),
        "received {error:?}"
    );
}

/// Refused, never downgraded. Running a full-access request under "ask every time" would be the safe
/// direction; running a read-only one under it would not, and a harness that silently picked either
/// would be deciding something only a person can.
#[tokio::test]
async fn a_level_this_profile_cannot_reach_is_refused_rather_than_downgraded() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeAcpAgent::new().process());

    let error = AcpHarness::new(profile())
        .open_session(
            &host(&launcher),
            OpenSession::new("chat-1").with_configuration(Configuration {
                level: PermissionLevel::FullAccess,
                routing: ApprovalRouting::User,
                ..Configuration::default()
            }),
        )
        .await
        .err()
        .expect("expected a refusal, received a session");
    assert!(
        matches!(error, Error::HostConfiguration { .. }),
        "received {error:?}"
    );
}

/// A probe learns "not installed" from the launcher refusing, because the library does not search
/// `PATH` on its own initiative.
#[tokio::test]
async fn a_probe_reads_the_version_the_agent_printed_and_never_claims_a_login_state() {
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeAcpAgent::new()
            .printing_version("fake-acp 4.5.6 (linux)")
            .version_process(),
    );

    let discovery = AcpHarness::new(profile())
        .discover(&host(&launcher))
        .await
        .expect("expected a discovery");
    assert_eq!(discovery.version.as_deref(), Some("4.5.6"));
    assert_eq!(discovery.gate, mango_external_agents::GateVerdict::Usable);
    assert_eq!(discovery.auth, mango_external_agents::AuthState::Unknown);
    assert!(
        discovery
            .capabilities
            .within(&AcpHarness::new(profile()).descriptor().capabilities),
        "expected the probe to stay inside the ceiling"
    );

    let empty = FakeLauncher::new();
    let missing = AcpHarness::new(profile())
        .discover(&host(&empty))
        .await
        .expect("expected a discovery");
    assert_eq!(
        missing.gate,
        mango_external_agents::GateVerdict::NotInstalled
    );
}

/// The contract a host is entitled to assume, run against the real harness.
#[tokio::test]
async fn the_harness_passes_the_core_conformance_suite() {
    let launcher = FakeLauncher::new();
    // One child per `open_session` the suite performs, plus the version probe it discovers with.
    launcher.push(
        FakeAcpAgent::new()
            .printing_version("fake-acp 1.2.3")
            .version_process(),
    );
    launcher.push(
        FakeAcpAgent::new()
            .asking_for_approval(Approval::Once)
            .closing_sessions()
            .process(),
    );

    let report = mango_external_agents::testing::conformance::run(
        &AcpHarness::new(profile()),
        &host(&launcher),
        mango_external_agents::testing::conformance::Options::default(),
    )
    .await;

    report.assert_passed();
    assert!(
        report
            .checks
            .iter()
            .any(|check| check.name == "an approval can be answered"
                && check.outcome == mango_external_agents::testing::conformance::Outcome::Passed),
        "expected the approval round trip to be proved rather than skipped, received {:#?}",
        report.skipped()
    );
}
