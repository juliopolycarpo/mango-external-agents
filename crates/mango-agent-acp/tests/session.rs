//! The harness driven end to end against [`FakeAcpAgent`], including the core conformance suite.
//!
//! Everything here goes through the real transport, the real dispatch loop and the real reducer; the
//! only thing replaced is the process, which the core's `FakeLauncher` hands over as scripted pipes.
//! That is deliberate: the parts most worth testing on this dialect are the ones the reducer's unit
//! tests cannot reach — that a turn ends exactly once, that an approval round trip lands, that a
//! cancel carries its reason, and that closing twice is not an error.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime};

use mango_agent_acp::testing::{Approval, FakeAcpAgent};
use mango_agent_acp::{AcpHarness, AcpProfile, SessionModeIds};
use mango_external_agents::testing::{FakeLauncher, FakeProcess, RecordingBroker};
use mango_external_agents::{
    ApprovalRouting, BrokerDecision, CancelReason, Clock, CloseReason, Configuration,
    DecisionSource, Error, EventKind, Harness, HostContext, Limits, OpenSession, PermissionLevel,
    Session, TurnRequest, TurnStream, VendorInfo,
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

fn host_with_clock(launcher: &FakeLauncher, clock: Arc<dyn Clock>) -> HostContext {
    HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .clock(clock)
        .client_info("mea-tests", "0.1.0")
        .build()
        .expect("expected a host")
}

/// A named clock that blocks the first event stamp until a test releases it.
#[derive(Debug, Default)]
struct SessionStartedClock {
    state: Mutex<SessionStartedClockState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct SessionStartedClockState {
    blocked: bool,
    released: bool,
}

impl SessionStartedClock {
    fn wait_until_blocked(&self) {
        let mut state = self.state.lock().expect("expected the clock state");
        while !state.blocked {
            state = self.changed.wait(state).expect("expected the clock state");
        }
    }

    fn release(&self) {
        let mut state = self.state.lock().expect("expected the clock state");
        state.released = true;
        self.changed.notify_all();
    }
}

impl Clock for SessionStartedClock {
    fn now(&self) -> SystemTime {
        let mut state = self.state.lock().expect("expected the clock state");
        if !state.released {
            state.blocked = true;
            self.changed.notify_all();
            while !state.released {
                state = self.changed.wait(state).expect("expected the clock state");
            }
        }
        SystemTime::UNIX_EPOCH
    }
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
        level: Some(PermissionLevel::Default),
        ..Configuration::default()
    }
}

/// The failure a call was expected to produce.
///
/// `Result::expect_err` needs its `Ok` to be `Debug`, and neither `Box<dyn Session>` nor `TurnStream`
/// is one — a live session handle has nothing meaningful to print. This says the same thing without
/// asking for it.
#[track_caller]
fn refusal<T>(result: mango_external_agents::Result<T>) -> Error {
    match result {
        Err(error) => error,
        Ok(_) => panic!("expected a refusal, received success"),
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
        Configuration {
            level: Some(PermissionLevel::ReadOnly),
            ..Configuration::default()
        },
    )
    .await;
    assert_eq!(
        session.info().effective_configuration.level,
        Some(PermissionLevel::ReadOnly)
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

/// The agent's question is still visible for audit, but a host response cannot replace the standing
/// read-only refusal while the harness owns that decision.
#[tokio::test]
async fn a_host_cannot_override_a_standing_read_only_refusal() {
    let (session, _launcher) = open(
        FakeAcpAgent::new().asking_for_approval(Approval::Once),
        Configuration {
            level: Some(PermissionLevel::ReadOnly),
            ..Configuration::default()
        },
    )
    .await;

    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "delete the build"))
        .await
        .expect("expected a turn");
    let request = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = turn.recv().await {
            if let EventKind::ApprovalRequested { request } = event.kind {
                return request;
            }
        }
        panic!("expected the approval request before the turn ended");
    })
    .await
    .expect("expected the approval request rather than a hang");

    session
        .respond(request.allow().expect("expected an allowing option"))
        .await
        .expect("expected the losing host answer to be harmless");

    let events = drain(&mut turn).await;
    assert!(
        events.iter().any(|kind| matches!(
            kind,
            EventKind::ApprovalResolved { decision, .. }
                if decision.option_id == "reject" && decision.source == DecisionSource::AutoReview
        )),
        "expected the standing refusal to reach the agent, received {events:?}"
    );
}

fn decision_option(decision: &mango_external_agents::ApprovalDecision) -> &str {
    &decision.option_id
}

fn has_automatic_refusal(events: &[EventKind]) -> bool {
    events.iter().any(|kind| {
        matches!(
            kind,
            EventKind::ApprovalResolved { decision, .. }
                if decision.option_id == "reject" && decision.source == DecisionSource::AutoReview
        )
    })
}

/// A turn changes only the axes it names. In particular, the read-only standing refusal must remain
/// in force when a later request chooses routing but omits the level, and when the next request
/// chooses neither.
#[tokio::test]
async fn turn_overrides_stick_and_omitted_axes_preserve_the_last_accepted_restriction() {
    let (session, _launcher) = open(
        FakeAcpAgent::new().asking_for_approval(Approval::Once),
        Configuration::default(),
    )
    .await;

    let mut first = session
        .start_turn(
            TurnRequest::new("turn-1", "delete the build").with_configuration(Configuration {
                level: Some(PermissionLevel::ReadOnly),
                ..Configuration::default()
            }),
        )
        .await
        .expect("expected the explicit restriction to be accepted");
    assert!(has_automatic_refusal(&drain(&mut first).await));
    assert_eq!(
        session.configuration().await,
        Configuration {
            level: Some(PermissionLevel::ReadOnly),
            ..Configuration::default()
        }
    );

    let mut second = session
        .start_turn(
            TurnRequest::new("turn-2", "delete the build").with_configuration(Configuration {
                routing: Some(ApprovalRouting::AutoReview),
                ..Configuration::default()
            }),
        )
        .await
        .expect("expected the routing-only override to inherit the restriction");
    assert!(has_automatic_refusal(&drain(&mut second).await));

    let mut third = session
        .start_turn(TurnRequest::new("turn-3", "delete the build"))
        .await
        .expect("expected omitted settings to inherit the accepted restriction");
    assert!(has_automatic_refusal(&drain(&mut third).await));
    assert_eq!(
        session.configuration().await,
        Configuration {
            level: Some(PermissionLevel::ReadOnly),
            routing: Some(ApprovalRouting::AutoReview),
            ..Configuration::default()
        }
    );
}

/// An agent that offers nothing to refuse with leaves nothing for a standing refusal to pick, so the
/// question has to reach a person rather than being answered with whatever was first in the list.
#[tokio::test]
async fn a_read_only_session_still_asks_when_the_agent_offered_no_way_to_refuse() {
    let (session, _launcher) = open(
        FakeAcpAgent::new().asking_for_approval(Approval::OnlyAllows),
        Configuration {
            level: Some(PermissionLevel::ReadOnly),
            ..Configuration::default()
        },
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

/// Cancellation can reach the agent before its permission request reaches this client. That late
/// request still has to receive ACP's `Cancelled` outcome, otherwise an agent waiting inside the
/// tool call cannot finish the prompt it was cancelling.
#[tokio::test]
async fn cancellation_withdraws_a_permission_that_arrives_after_cancel() {
    let (session, launcher) = open(
        FakeAcpAgent::new().asking_for_approval(Approval::Once),
        Configuration::default(),
    )
    .await;

    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "take your time"))
        .await
        .expect("expected a turn");
    session
        .cancel(CancelReason::Requested)
        .await
        .expect("expected the immediate cancel to land");

    let withdrawal = answers_reaching_the_agent(&launcher, 1).await;
    assert!(
        withdrawal.iter().any(|answer| answer.contains("cancelled")),
        "expected the late permission request to be withdrawn, received {withdrawal:?}"
    );

    let events = drain(&mut turn).await;
    assert!(
        events.iter().any(|kind| matches!(
            kind,
            EventKind::Cancelled {
                reason: CancelReason::Requested
            }
        )),
        "expected the cancelled turn marker, received {events:?}"
    );
    assert!(
        matches!(events.last(), Some(EventKind::Completed)),
        "expected the marker to be followed by completion, received {events:?}"
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

/// `SessionStarted` is emitted after the turn handle is installed. If close wins while that event is
/// stalled, releasing the event must not let the starter submit a detached `session/prompt`.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_close_between_handle_installation_and_prompt_write_refuses_the_detached_turn() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeAcpAgent::new().process());
    let clock = Arc::new(SessionStartedClock::default());
    let host = host_with_clock(&launcher, Arc::clone(&clock) as Arc<dyn Clock>);
    let opened = AcpHarness::new(profile())
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let session: Arc<dyn Session> = Arc::from(opened);

    let starting = {
        let session = Arc::clone(&session);
        tokio::spawn(async move { session.start_turn(TurnRequest::new("turn-1", "one")).await })
    };
    clock.wait_until_blocked();

    session
        .close(CloseReason::ConsentRevoked)
        .await
        .expect("expected the close to land while the start event was stalled");
    clock.release();

    let error = refusal(
        starting
            .await
            .expect("expected the start task to return a result"),
    );
    assert!(
        matches!(error, Error::Closed { subject: "session" }),
        "received {error:?}"
    );
    assert!(
        !launcher
            .written()
            .iter()
            .any(|line| line.contains("\"session/prompt\"")),
        "a closed session must not submit a detached prompt, received {:?}",
        launcher.written()
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

/// Narrowing a turn below a mode-bearing session level looks harmless and is not. The agent stays in
/// the mode `open_session` set, so it raises no permission request at all and the standing refusal has
/// nothing to answer — the turn would run with full access while the harness reported `ReadOnly`.
#[tokio::test]
async fn a_turn_cannot_narrow_below_the_mode_the_session_was_opened_under() {
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeAcpAgent::new()
            .with_modes(["bypassPermissions", "plan"])
            .process(),
    );
    let moded = Arc::new(
        AcpProfile::custom("fake", ["fake-acp", "acp"], VENDOR).with_modes(SessionModeIds {
            full_access: Some("bypassPermissions"),
            ..SessionModeIds::UNKNOWN
        }),
    );

    let session = AcpHarness::new(moded)
        .open_session(
            &host(&launcher),
            OpenSession::new("chat-1").with_configuration(Configuration {
                level: Some(PermissionLevel::FullAccess),
                ..Configuration::default()
            }),
        )
        .await
        .expect("expected a session");

    let error = refusal(
        session
            .start_turn(
                TurnRequest::new("turn-1", "just read").with_configuration(Configuration {
                    level: Some(PermissionLevel::ReadOnly),
                    ..Configuration::default()
                }),
            )
            .await,
    );
    assert!(
        matches!(&error, Error::Protocol { expected, .. } if expected.contains("bypassPermissions")),
        "received {error:?}"
    );
}

/// `close` must not drop a half-sent terminal. A timeout that abandoned the `emit` would send nothing —
/// `mpsc::Sender::send` is cancel-safe — so a host that stopped reading and then closed would get a
/// stream that just ends, with no `Cancelled` and no `Completed`, which the core's conformance rules
/// refuse.
#[tokio::test]
async fn closing_a_turn_nobody_is_reading_still_delivers_exactly_one_terminal() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeAcpAgent::new().never_finishing_turns().process());
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
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "fill the channel"))
        .await
        .expect("expected a turn");

    // The channel holds one event and nobody has read it, so `close`'s terminal cannot be sent yet.
    session
        .close(CloseReason::Shutdown)
        .await
        .expect("expected the close to return");

    // Now read. The terminal has to arrive rather than having been dropped with the abandoned future.
    let events = drain(&mut turn).await;
    assert_eq!(
        events
            .iter()
            .filter(|kind| matches!(kind, EventKind::Completed | EventKind::Error { .. }))
            .count(),
        1,
        "expected exactly one terminal, received {events:?}"
    );
    assert!(
        events.iter().any(|kind| matches!(
            kind,
            EventKind::Cancelled {
                reason: CancelReason::Shutdown
            }
        )),
        "expected the close's own reason, received {events:?}"
    );
}

/// `close` awaits `session/close` for up to its grace period, and a turn still live across that wait is
/// a window in which the agent can ask for permission. Nothing may grant one while the session is being
/// torn down — least of all under `ConsentRevoked`, where the machine's owner has just withdrawn the
/// permission to run the agent at all.
#[tokio::test]
async fn a_question_raised_during_the_close_handshake_is_withdrawn_rather_than_granted() {
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeAcpAgent::new()
            .closing_sessions()
            // Asks on `session/close`, which is exactly the window `close` awaits in.
            .asking_when_closing()
            // And never answers its prompt, so the turn is still live when `close` runs. Draining to a
            // terminal first would end the turn and take the handler down its no-turn path, which is
            // how the first draft of this test passed with the fix reverted.
            .never_finishing_turns()
            .process(),
    );
    // A policy that would allow, to prove the level is not what saves this.
    let (host, broker) = host_with_broker(&launcher, BrokerDecision::Allow);

    let session = AcpHarness::new(profile())
        .open_session(
            &host,
            OpenSession::new("chat-1").with_configuration(permissive()),
        )
        .await
        .expect("expected a session");
    let _turn = session
        .start_turn(TurnRequest::new("turn-1", "do the thing"))
        .await
        .expect("expected a turn");

    session
        .close(CloseReason::ConsentRevoked)
        .await
        .expect("expected the close to land");

    let answers = outcome_lines(&launcher);
    assert!(
        answers.iter().all(|line| line.contains("cancelled")),
        "expected every answer during teardown to be a withdrawal, received {answers:?}"
    );
    assert!(
        broker.requests().is_empty(),
        "expected no policy decision during teardown, received {:?}",
        broker.requests()
    );
}

/// The level decides whether the broker is asked at all, and this is why. Under `ReadOnly` the library
/// must never reach `broker_response`: a policy answering `Allow` there becomes an allowing option id
/// on the wire, so the one level that exists to grant nothing would grant.
///
/// Reachable only when the agent offers no way to refuse — otherwise the standing refusal answers
/// first — which is why the earlier read-only tests missed it: one built its host without a broker, the
/// other used an agent that offered a refusal.
#[tokio::test]
async fn a_read_only_session_never_lets_a_broker_allow_even_when_it_cannot_refuse() {
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeAcpAgent::new()
            .asking_for_approval(Approval::OnlyAllows)
            .process(),
    );
    let (host, broker) = host_with_broker(&launcher, BrokerDecision::Allow);

    let session = AcpHarness::new(profile())
        .open_session(
            &host,
            OpenSession::new("chat-1").with_configuration(Configuration {
                level: Some(PermissionLevel::ReadOnly),
                ..Configuration::default()
            }),
        )
        .await
        .expect("expected a session");
    assert_eq!(
        session.info().effective_configuration.level,
        Some(PermissionLevel::ReadOnly)
    );

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
    assert!(
        broker.requests().is_empty(),
        "expected a read-only session never to consult the broker, received {:?}",
        broker.requests()
    );
    // And nothing was answered on the agent's behalf.
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert!(
        outcome_lines(&launcher).is_empty(),
        "expected no answer to reach the agent, received {:?}",
        outcome_lines(&launcher)
    );
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected the close to land");
}

/// ACP v1 states twice that a client sending `session/cancel` MUST answer every pending
/// `session/request_permission` with the `Cancelled` outcome. An agent whose permission await is not
/// itself cancellation-aware never returns from its tool call otherwise, so `session/prompt` never
/// answers, the turn emits no terminal at all, and the turn slot stays occupied for the session's life.
#[tokio::test]
async fn cancelling_withdraws_every_question_the_agent_is_waiting_on() {
    // Answers its prompt only once the question is settled, which is what a real agent does — so a
    // harness that cancelled without withdrawing would hang here rather than fail an assertion.
    let (session, launcher) = open(
        FakeAcpAgent::new().asking_for_approval(Approval::Once),
        permissive(),
    )
    .await;

    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "take your time"))
        .await
        .expect("expected a turn");
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = turn.recv().await {
            if matches!(event.kind, EventKind::ApprovalRequested { .. }) {
                return;
            }
        }
    })
    .await
    .expect("expected the question to arrive");

    session
        .cancel(CancelReason::Requested)
        .await
        .expect("expected the cancel to land");

    let withdrawal = answers_reaching_the_agent(&launcher, 1).await;
    assert!(
        withdrawal[0].contains("cancelled"),
        "expected the cancel to withdraw the question, received {withdrawal:?}"
    );

    // And the turn still ends, which is what the withdrawal buys.
    let events = drain(&mut turn).await;
    assert!(
        events
            .iter()
            .any(|kind| matches!(kind, EventKind::Completed | EventKind::Error { .. })),
        "expected the turn to end, received {events:?}"
    );

    // The slot is free again, so the session is still usable.
    session
        .start_turn(TurnRequest::new("turn-2", "carry on"))
        .await
        .expect("expected the session to still take a turn");
}

/// A host that drops a `TurnStream` closes the sink; it does not finish the `session/prompt` that is
/// still in flight. Freeing the turn slot on a failed emit would put a second prompt on a wire that
/// cannot tell two turns apart, and would hand turn 1's completion task turn 2's handle to terminate.
#[tokio::test]
async fn a_dropped_turn_stream_does_not_free_the_slot_while_the_prompt_is_in_flight() {
    let (session, _launcher) =
        open(FakeAcpAgent::new().never_finishing_turns(), permissive()).await;

    let turn = session
        .start_turn(TurnRequest::new("turn-1", "one"))
        .await
        .expect("expected a turn");
    // Dropped mid-turn: this agent never answers its prompt, so prompt 1 is genuinely in flight and
    // the only thing that could free the slot is a handler reacting to the closed sink.
    drop(turn);
    // Let the agent's next frame hit the closed sink.
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }

    let error = refusal(session.start_turn(TurnRequest::new("turn-2", "two")).await);
    assert!(
        matches!(&error, Error::Protocol { expected, .. } if expected.contains("one session/prompt")),
        "received {error:?}"
    );
}

/// A rejected concurrent turn has not run, so its overrides cannot become the defaults a later turn
/// inherits. The configuration is committed only after ACP's one-prompt slot accepts the turn.
#[tokio::test]
async fn a_rejected_concurrent_turn_does_not_change_the_inherited_configuration() {
    let (session, _launcher) =
        open(FakeAcpAgent::new().never_finishing_turns(), permissive()).await;

    let _first = session
        .start_turn(TurnRequest::new("turn-1", "one"))
        .await
        .expect("expected the first prompt to occupy ACP's only slot");

    let error = refusal(
        session
            .start_turn(
                TurnRequest::new("turn-2", "two").with_configuration(Configuration {
                    level: Some(PermissionLevel::ReadOnly),
                    ..Configuration::default()
                }),
            )
            .await,
    );
    assert!(
        matches!(&error, Error::Protocol { expected, .. } if expected.contains("one session/prompt")),
        "received {error:?}"
    );
    assert_eq!(
        session.configuration().await,
        permissive(),
        "a rejected turn must not replace the last accepted settings"
    );

    session
        .close(CloseReason::Requested)
        .await
        .expect("expected the session to close");
}

/// A question cannot outlive the turn it belongs to. Answering one afterwards would emit into a
/// finished sink and tell the agent "allow" about a turn it has stopped running, so the turn's own end
/// withdraws every question still parked — not just `close`.
#[tokio::test]
async fn a_question_still_parked_when_the_turn_ends_is_withdrawn_by_the_turn() {
    // The fake answers its prompt without waiting, so the turn ends with the question still open —
    // which is what a misbehaving agent does, and what a cancel produces on a well-behaved one.
    let agent = FakeAcpAgent::new().with_updates(vec![serde_json::json!({
        "sessionUpdate": "agent_message_chunk",
        "content": { "type": "text", "text": "working" }
    })]);
    let (session, launcher) = open(agent.asking_without_waiting(), permissive()).await;

    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "delete the build"))
        .await
        .expect("expected a turn");

    let events = drain(&mut turn).await;
    let question = events
        .iter()
        .find_map(|kind| match kind {
            EventKind::ApprovalRequested { request } => Some(request.clone()),
            _ => None,
        })
        .expect("expected the question to reach the host");

    // Awaited rather than read once: the answer reaches the agent through the transport actor, so
    // reading immediately would report an empty list for a write that simply had not flushed — which
    // would make this pass for the wrong reason in both directions.
    let withdrawal = answers_reaching_the_agent(&launcher, 1).await;
    assert!(
        withdrawal[0].contains("cancelled"),
        "expected the turn's end to withdraw the question, received {withdrawal:?}"
    );

    // Answering now must not reach the agent: the turn it belonged to is over. It is accepted rather
    // than refused, for the same reason a second `close` is.
    session
        .respond(question.deny().expect("expected a refusing option"))
        .await
        .expect("expected the late answer to be accepted");

    // Given every chance to arrive, and it must not.
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    let answers = outcome_lines(&launcher);
    assert_eq!(
        answers.len(),
        1,
        "expected the late answer never to reach the agent, received {answers:?}"
    );
}

/// Every answer to a `session/request_permission` that actually reached the agent.
fn outcome_lines(launcher: &FakeLauncher) -> Vec<String> {
    launcher
        .written()
        .into_iter()
        .filter(|line| line.contains("\"outcome\""))
        .collect()
}

/// Waits until `count` answers have reached the agent, or fails rather than reading a stale empty list.
async fn answers_reaching_the_agent(launcher: &FakeLauncher, count: usize) -> Vec<String> {
    let waited = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let answers = outcome_lines(launcher);
            if answers.len() >= count {
                return answers;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    waited.unwrap_or_else(|_| {
        panic!(
            "expected {count} answer(s) to reach the agent, received {:?}",
            outcome_lines(launcher)
        )
    })
}

/// An agent that accepts the pipe and never answers would otherwise hold `open_session` open for the
/// life of the process, which looks to a host like a hung machine rather than a misbehaving agent.
/// `Limits::request_timeout` is the number the host already set for exactly this.
#[tokio::test(start_paused = true)]
async fn a_handshake_the_agent_never_answers_ends_on_the_hosts_own_deadline() {
    let launcher = FakeLauncher::new();
    // Accepts every line and answers nothing, which is the shape that hangs.
    launcher.push(FakeProcess::responding(|_| Vec::new()));
    let host = HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .limits(Limits {
            request_timeout: Duration::from_secs(5),
            ..Limits::default()
        })
        .build()
        .expect("expected a host");

    // Wrapped so an *unbounded* handshake fails red rather than hanging: with every task parked the
    // paused clock auto-advances, this fires, and nextest reports a failure instead of a slow test.
    let opened = tokio::time::timeout(
        Duration::from_secs(600),
        AcpHarness::new(profile()).open_session(&host, OpenSession::new("chat-1")),
    )
    .await
    .expect("expected the handshake to give up on its own deadline");

    let error = refusal(opened);
    let Error::Timeout { operation, after } = &error else {
        panic!("received {error:?}");
    };
    assert!(operation.contains("initialize"), "received {operation:?}");
    assert_eq!(*after, Duration::from_secs(5));
}

/// The same hole `open_session` refuses, one layer down. A turn asking for a level the profile cannot
/// reach would otherwise set no mode, refuse no request, and run as `Default` while the host believed
/// it had granted more.
#[tokio::test]
async fn a_turn_asking_for_a_level_this_profile_cannot_reach_is_refused() {
    let (session, _launcher) = open(FakeAcpAgent::new(), permissive()).await;

    let error = refusal(
        session
            .start_turn(
                TurnRequest::new("turn-1", "do everything").with_configuration(Configuration {
                    level: Some(PermissionLevel::FullAccess),
                    ..Configuration::default()
                }),
            )
            .await,
    );
    assert!(
        matches!(error, Error::HostConfiguration { .. }),
        "received {error:?}"
    );
}

/// A launcher that records whether the library ended each child it handed out.
///
/// Needed because `FakeLauncher` exposes no per-child handle, so nothing outside the library can
/// otherwise see a `kill` — and an assertion that cannot see one is an assertion that passes with the
/// teardown disabled.
#[derive(Clone)]
struct KillRecordingLauncher {
    inner: FakeLauncher,
    killed: Arc<std::sync::atomic::AtomicUsize>,
}

impl KillRecordingLauncher {
    fn new(inner: FakeLauncher) -> Self {
        Self {
            inner,
            killed: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    fn kills(&self) -> usize {
        self.killed.load(std::sync::atomic::Ordering::Acquire)
    }
}

#[async_trait::async_trait]
impl mango_external_agents::ProcessLauncher for KillRecordingLauncher {
    async fn spawn(
        &self,
        spec: mango_external_agents::LaunchSpec,
    ) -> mango_external_agents::Result<mango_external_agents::ManagedProcess> {
        let process = self.inner.spawn(spec).await?;
        Ok(mango_external_agents::ManagedProcess {
            control: Arc::new(RecordingControl {
                inner: process.control,
                killed: Arc::clone(&self.killed),
            }),
            ..process
        })
    }
}

struct RecordingControl {
    inner: Arc<dyn mango_external_agents::ProcessControl>,
    killed: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl mango_external_agents::ProcessControl for RecordingControl {
    fn pid(&self) -> Option<u32> {
        self.inner.pid()
    }

    fn stderr_tail(&self) -> String {
        self.inner.stderr_tail()
    }

    async fn wait(&self) -> mango_external_agents::Result<mango_external_agents::ExitStatus> {
        self.inner.wait().await
    }

    async fn kill(&self, reason: CancelReason) -> mango_external_agents::Result<()> {
        self.killed
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        self.inner.kill(reason).await
    }
}

fn recording_host(launcher: &KillRecordingLauncher) -> HostContext {
    HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .build()
        .expect("expected a host")
}

/// A failed `open_session` must not leave an agent running with nothing driving it. The dispatch loop
/// winds down only when the shutdown channel drops, so a child left behind would outlive the call
/// that started it by however long that took to reach it.
#[tokio::test]
async fn a_refused_handshake_ends_the_child_rather_than_leaving_it_running() {
    let inner = FakeLauncher::new();
    inner.push(FakeAcpAgent::new().with_protocol_version(2).process());
    let launcher = KillRecordingLauncher::new(inner);

    refusal(
        AcpHarness::new(profile())
            .open_session(&recording_host(&launcher), OpenSession::new("chat-1"))
            .await,
    );

    assert_eq!(
        launcher.kills(),
        1,
        "expected the refused handshake to end its child"
    );
}

/// The same guarantee on the other error path: a session the agent opened, refused on our side because
/// the profile named a mode the agent never advertised.
#[tokio::test]
async fn a_mode_the_agent_never_advertised_is_refused_and_ends_the_child() {
    let inner = FakeLauncher::new();
    inner.push(FakeAcpAgent::new().with_modes(["default"]).process());
    let launcher = KillRecordingLauncher::new(inner);

    let insists = Arc::new(
        AcpProfile::custom("fake", ["fake-acp", "acp"], VENDOR).with_modes(SessionModeIds {
            read_only: Some("plan"),
            ..SessionModeIds::UNKNOWN
        }),
    );
    let error = refusal(
        AcpHarness::new(insists)
            .open_session(
                &recording_host(&launcher),
                OpenSession::new("chat-1").with_configuration(Configuration {
                    level: Some(PermissionLevel::ReadOnly),
                    ..Configuration::default()
                }),
            )
            .await,
    );

    assert!(
        matches!(&error, Error::Protocol { expected, .. } if expected.contains("plan")),
        "received {error:?}"
    );
    assert_eq!(
        launcher.kills(),
        1,
        "expected the refused session to end its child"
    );
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

    let error = refusal(
        AcpHarness::new(signed_out)
            .open_session(&host(&launcher), OpenSession::new("chat-1"))
            .await,
    );

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

    let error = refusal(
        AcpHarness::new(profile())
            .open_session(&host(&launcher), OpenSession::new("chat-1"))
            .await,
    );
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

    let error = refusal(
        AcpHarness::new(profile())
            .open_session(
                &host(&launcher),
                OpenSession::new("chat-1").with_configuration(Configuration {
                    level: Some(PermissionLevel::FullAccess),
                    routing: Some(ApprovalRouting::User),
                    ..Configuration::default()
                }),
            )
            .await,
    );
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

#[path = "session/expiry.rs"]
mod expiry;
