//! The host's `CancelToken` ends an ACP session: before the child is launched, while it opens, and
//! while a turn or an approval is in flight. Every teardown asserts the published status and the
//! number of live children, because a status alone can be `Closed` over a child nobody reaped.

use super::*;

use mango_external_agents::testing::Announcer;

/// A host whose shutdown token the test owns, with an idle bound no test could reach: a turn that
/// ends inside it ended because the host cancelled, not because the agent went quiet.
fn cancellable_host(launcher: &FakeLauncher, cancel: &CancelToken) -> HostContext {
    HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .cancel(cancel.clone())
        .limits(Limits {
            kill_grace: Duration::from_millis(10),
            shutdown_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(600),
            ..Limits::default()
        })
        .build()
        .expect("expected a host")
}

/// An agent that never answers `session/prompt`, ignores `session/cancel`, and keeps streaming
/// updates until the test stops it: the shape that idle expiry can never end.
struct ChatteringAgent {
    announcer: Announcer,
    stop: Arc<AtomicBool>,
}

impl ChatteringAgent {
    fn new() -> Self {
        Self {
            announcer: Announcer::new(),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    fn process(&self) -> FakeProcess {
        FakeAcpAgent::new()
            .with_updates(Vec::new())
            .staying_silent()
            .process()
            .announcing(self.announcer.clone())
    }

    /// Streams one update every 5 ms until [`ChatteringAgent::hang_up`] is called.
    fn start_chattering(&self) -> tokio::task::JoinHandle<()> {
        let announcer = self.announcer.clone();
        let stop = Arc::clone(&self.stop);
        tokio::spawn(async move {
            while !stop.load(Ordering::Acquire) {
                announcer.announce(
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": "sess_fake",
                            "update": {
                                "sessionUpdate": "agent_message_chunk",
                                "content": { "type": "text", "text": "still working" }
                            }
                        }
                    })
                    .to_string(),
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    }

    fn hang_up(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// Reads a turn to its terminal, reporting what arrived rather than hanging when there is none.
async fn events_to_terminal(turn: &mut TurnStream) -> Vec<EventKind> {
    let mut seen = Vec::new();
    let ended = tokio::time::timeout(Duration::from_secs(8), async {
        while let Some(event) = turn.recv().await {
            let terminal = event.is_terminal();
            seen.push(event.kind);
            if terminal {
                return true;
            }
        }
        false
    })
    .await;
    assert!(
        matches!(ended, Ok(true)),
        "expected a terminal event after the host cancelled | received: {seen:?}"
    );
    seen
}

/// Polls until every child is gone, naming the last count seen when they are not.
async fn assert_children_reaped(launcher: &FakeLauncher) {
    let mut last = launcher.live_children();
    let reaped = tokio::time::timeout(Duration::from_secs(5), async {
        while last != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            last = launcher.live_children();
        }
    })
    .await;
    assert!(
        reaped.is_ok(),
        "expected live children: 0 | received: {last}"
    );
}

async fn assert_settles_closed(session: &dyn Session) {
    let mut lifecycle = session.subscribe();
    let status = status_once_settled(&mut lifecycle).await;
    assert_eq!(
        status,
        SessionStatus::Closed,
        "expected session status: Closed | received: {status:?}"
    );
}

/// The turn ended as a host shutdown: the cancel marker, then the terminal.
fn assert_shutdown_terminal(events: &[EventKind]) {
    let marked = events.iter().any(|kind| {
        matches!(
            kind,
            EventKind::Cancelled {
                reason: CancelReason::Shutdown
            }
        )
    });
    assert!(
        marked && matches!(events.last(), Some(EventKind::Completed)),
        "expected events: Cancelled(Shutdown) then Completed | received: {events:?}"
    );
}

/// A cancelled host must not get a child it would then have to reap.
#[tokio::test]
async fn opening_with_a_cancelled_token_launches_nothing() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeAcpAgent::new().process());
    let cancel = CancelToken::new();
    cancel.cancel();

    let opened = AcpHarness::new(profile())
        .open_session(
            &cancellable_host(&launcher, &cancel),
            OpenSession::new("gone"),
        )
        .await;

    assert_eq!(
        launcher.launches().len(),
        0,
        "expected launches: 0 | received: {}",
        launcher.launches().len()
    );
    assert_eq!(
        launcher.live_children(),
        0,
        "expected live children: 0 | received: {}",
        launcher.live_children()
    );
    let error = refusal(opened);
    assert!(
        matches!(
            error.cause(),
            Error::Cancelled {
                reason: CancelReason::Shutdown
            }
        ),
        "expected error: Cancelled(Shutdown) | received: {error:?}"
    );
    assert_eq!(
        error.dispatch(),
        Dispatch::NotSubmitted,
        "expected dispatch: NotSubmitted | received: {:?}",
        error.dispatch()
    );
}

/// A cancel that lands while the handshake waits on an agent that never answers refuses the open
/// and reaps the child instead of leaving the caller to wait out the request timeout.
#[tokio::test]
async fn cancelling_while_the_handshake_is_pending_refuses_the_open_and_reaps() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeProcess::responding(|_| Vec::new()));
    let cancel = CancelToken::new();
    let host = cancellable_host(&launcher, &cancel);

    let harness = AcpHarness::new(profile());
    let opening = harness.open_session(&host, OpenSession::new("handshake"));
    let canceller = async {
        // The child exists once the launch is recorded; the handshake is then parked on it.
        while launcher.launches().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        cancel.cancel();
    };
    let (opened, ()) = tokio::time::timeout(Duration::from_secs(8), async {
        tokio::join!(opening, canceller)
    })
    .await
    .expect("expected the open to end after the host cancelled, received a pending open");

    let error = refusal(opened);
    assert!(
        matches!(error.cause(), Error::Cancelled { .. }),
        "expected error: Cancelled | received: {error:?}"
    );
    assert_children_reaped(&launcher).await;
}

/// An open session with no turn reaches `Closed` with its child reaped once the host cancels.
#[tokio::test]
async fn cancelling_an_idle_session_closes_it_and_reaps_the_child() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeAcpAgent::new().process());
    let cancel = CancelToken::new();
    let session = AcpHarness::new(profile())
        .open_session(
            &cancellable_host(&launcher, &cancel),
            OpenSession::new("idle"),
        )
        .await
        .expect("expected a session");

    cancel.cancel();

    assert_settles_closed(session.as_ref()).await;
    assert_children_reaped(&launcher).await;
}

/// A silent agent would otherwise hold the turn until idle expiry, 600 s here.
#[tokio::test]
async fn cancelling_mid_prompt_ends_the_turn_and_reaps_the_child() {
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeAcpAgent::new()
            .with_updates(Vec::new())
            .staying_silent()
            .process(),
    );
    let cancel = CancelToken::new();
    let session = AcpHarness::new(profile())
        .open_session(
            &cancellable_host(&launcher, &cancel),
            OpenSession::new("prompt").with_configuration(permissive()),
        )
        .await
        .expect("expected a session");
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "work"))
        .await
        .expect("expected a turn");

    cancel.cancel();

    assert_shutdown_terminal(&events_to_terminal(&mut turn).await);
    assert_settles_closed(session.as_ref()).await;
    assert_children_reaped(&launcher).await;
}

/// An agent that keeps sending updates resets idle expiry forever, so only the token ends it.
#[tokio::test]
async fn cancelling_a_turn_whose_agent_keeps_updating_ends_it() {
    let agent = ChatteringAgent::new();
    let launcher = FakeLauncher::new();
    launcher.push(agent.process());
    let cancel = CancelToken::new();
    let session = AcpHarness::new(profile())
        .open_session(
            &cancellable_host(&launcher, &cancel),
            OpenSession::new("chatter").with_configuration(permissive()),
        )
        .await
        .expect("expected a session");
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "work"))
        .await
        .expect("expected a turn");
    let chatter = agent.start_chattering();
    tokio::time::sleep(Duration::from_millis(50)).await;

    cancel.cancel();

    let events = events_to_terminal(&mut turn).await;
    agent.hang_up();
    let _ = chatter.await;
    assert_shutdown_terminal(&events);
    assert_settles_closed(session.as_ref()).await;
    assert_children_reaped(&launcher).await;
}

/// A question the host has not answered is settled exactly once, before the terminal, and the
/// child goes with it.
#[tokio::test]
async fn cancelling_with_an_approval_pending_settles_it_and_reaps_the_child() {
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeAcpAgent::new()
            .with_updates(Vec::new())
            .asking_for_approval(Approval::Once)
            .process(),
    );
    let cancel = CancelToken::new();
    let session = AcpHarness::new(profile())
        .open_session(
            &cancellable_host(&launcher, &cancel),
            OpenSession::new("approval").with_configuration(permissive()),
        )
        .await
        .expect("expected a session");
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "work"))
        .await
        .expect("expected a turn");
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), turn.recv())
            .await
            .unwrap_or_else(|_| panic!("expected an approval request | received: {events:?}"))
            .unwrap_or_else(|| {
                panic!("expected an approval request | received a closed stream: {events:?}")
            });
        let asked = matches!(event.kind, EventKind::ApprovalRequested { .. });
        events.push(event.kind);
        if asked {
            break;
        }
    }

    cancel.cancel();

    events.extend(events_to_terminal(&mut turn).await);
    let resolved = events
        .iter()
        .filter(|kind| matches!(kind, EventKind::ApprovalResolved { .. }))
        .count();
    assert_eq!(
        resolved, 1,
        "expected approval resolutions: 1 | received: {events:?}"
    );
    // The reason the watcher records is what turns the withdrawn question's teardown into a
    // `Cancelled(Shutdown)` terminal rather than a link failure.
    assert_shutdown_terminal(&events);
    assert_settles_closed(session.as_ref()).await;
    assert_children_reaped(&launcher).await;
}

/// A launcher whose children remember the reason every kill request carried, so a host's
/// reason-sensitive cleanup hook can be shown the reason that actually ended the open.
#[derive(Clone)]
struct ReasonRecordingLauncher {
    inner: FakeLauncher,
    reasons: Arc<Mutex<Vec<CancelReason>>>,
}

impl ReasonRecordingLauncher {
    fn new(inner: FakeLauncher) -> Self {
        Self {
            inner,
            reasons: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn kill_reasons(&self) -> Vec<CancelReason> {
        self.reasons
            .lock()
            .expect("expected recorded kill reasons")
            .clone()
    }
}

#[async_trait::async_trait]
impl ProcessLauncher for ReasonRecordingLauncher {
    async fn spawn(
        &self,
        spec: mango_external_agents::LaunchSpec,
    ) -> mango_external_agents::Result<ManagedProcess> {
        let process = self.inner.spawn(spec).await?;
        Ok(ManagedProcess {
            control: Arc::new(ReasonRecordingControl {
                inner: process.control,
                reasons: Arc::clone(&self.reasons),
            }),
            ..process
        })
    }
}

struct ReasonRecordingControl {
    inner: Arc<dyn ProcessControl>,
    reasons: Arc<Mutex<Vec<CancelReason>>>,
}

#[async_trait::async_trait]
impl ProcessControl for ReasonRecordingControl {
    fn pid(&self) -> Option<u32> {
        self.inner.pid()
    }

    fn stderr_tail(&self) -> String {
        self.inner.stderr_tail()
    }

    async fn wait(&self) -> mango_external_agents::Result<ExitStatus> {
        self.inner.wait().await
    }

    async fn kill(&self, reason: CancelReason) -> mango_external_agents::Result<()> {
        self.reasons
            .lock()
            .expect("expected recorded kill reasons")
            .push(reason);
        self.inner.kill(reason).await
    }
}

/// The host's cleanup hook is told the host's own reason, not an ordinary request.
#[tokio::test]
async fn a_handshake_cancel_reaches_the_launcher_as_a_shutdown() {
    let inner = FakeLauncher::new();
    inner.push(FakeProcess::responding(|_| Vec::new()));
    let launcher = ReasonRecordingLauncher::new(inner.clone());
    let cancel = CancelToken::new();
    let host = HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .cancel(cancel.clone())
        .build()
        .expect("expected a host");

    let harness = AcpHarness::new(profile());
    let opening = harness.open_session(&host, OpenSession::new("handshake-reason"));
    let canceller = async {
        while inner.launches().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        cancel.cancel();
    };
    let (opened, ()) = tokio::time::timeout(Duration::from_secs(8), async {
        tokio::join!(opening, canceller)
    })
    .await
    .expect("expected the open to end after the host cancelled, received a pending open");

    let _ = refusal(opened);
    assert_children_reaped(&inner).await;
    assert_eq!(
        launcher.kill_reasons(),
        vec![CancelReason::Shutdown],
        "expected kill reasons: [Shutdown] | received: {:?}",
        launcher.kill_reasons()
    );
}

/// Cancels an open while the agent holds one setup request, then expects the open to end and the
/// child to go, without waiting out the request timeout.
async fn assert_cancel_ends_a_held_setup_request(
    method: &'static str,
    profile: Arc<AcpProfile>,
    request: OpenSession,
) {
    let agent = HeldRequestAgent::new(method);
    let launcher = FakeLauncher::new();
    launcher.push(agent.process());
    let cancel = CancelToken::new();
    let host = cancellable_host(&launcher, &cancel);

    let harness = AcpHarness::new(profile);
    let opening = harness.open_session(&host, request);
    let canceller = async {
        agent.wait_until_entered().await;
        cancel.cancel();
    };
    let (opened, ()) = tokio::time::timeout(Duration::from_secs(8), async {
        tokio::join!(opening, canceller)
    })
    .await
    .expect("expected the open to end after the host cancelled, received a pending open");

    let error = refusal(opened);
    assert!(
        matches!(error.cause(), Error::Cancelled { .. }),
        "expected error: Cancelled | received: {error:?}"
    );
    assert_children_reaped(&launcher).await;
}

/// The watcher is not running while the profile's mode is being set, so the open observes the
/// token itself.
#[tokio::test]
async fn cancelling_while_the_profile_mode_is_pending_refuses_the_open_and_reaps() {
    let profile = Arc::new(
        AcpProfile::custom("fake", ["fake-acp", "acp"], VENDOR).with_modes(SessionModeIds {
            read_only: Some("plan"),
            ..SessionModeIds::UNKNOWN
        }),
    );
    assert_cancel_ends_a_held_setup_request(
        "session/set_mode",
        profile,
        OpenSession::new("mode").with_configuration(at_level(PermissionLevel::ReadOnly)),
    )
    .await;
}

/// The same for a catalog-backed setting, which goes through `session/set_config_option`.
#[tokio::test]
async fn cancelling_while_a_setting_is_pending_refuses_the_open_and_reaps() {
    assert_cancel_ends_a_held_setup_request(
        "session/set_config_option",
        profile(),
        OpenSession::new("setting").with_configuration(
            ConfigurationPatch::new().model(ConfigurationChange::Set(String::from("large"))),
        ),
    )
    .await;
}

/// A routing-only patch is local: it touches no wire request, so only the token can refuse it once
/// the watcher has ended the session on the host's shutdown.
#[tokio::test]
async fn configuring_after_a_host_shutdown_is_refused_and_close_still_returns() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeAcpAgent::new().process());
    let cancel = CancelToken::new();
    let session = AcpHarness::new(profile())
        .open_session(
            &cancellable_host(&launcher, &cancel),
            OpenSession::new("configure"),
        )
        .await
        .expect("expected a session");
    cancel.cancel();
    assert_settles_closed(session.as_ref()).await;

    let refused = session
        .configure(
            ConfigurationPatch::new().routing(ConfigurationChange::Set(ApprovalRouting::User)),
        )
        .await;

    let error = match refused {
        Err(error) => error,
        Ok(outcome) => panic!(
            "expected configure after a host shutdown: Cancelled(Shutdown) | received: Ok({outcome:?})"
        ),
    };
    assert!(
        matches!(
            error.cause(),
            Error::Cancelled {
                reason: CancelReason::Shutdown
            }
        ),
        "expected error: Cancelled(Shutdown) | received: {error:?}"
    );
    assert_eq!(
        error.dispatch(),
        Dispatch::NotSubmitted,
        "expected dispatch: NotSubmitted | received: {:?}",
        error.dispatch()
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        session.close(CloseReason::Requested),
    )
    .await
    .expect("expected close after the shutdown to return, not hang")
    .expect("expected close to observe the watcher's successful cleanup");
    assert_children_reaped(&launcher).await;
}

/// A catalog request that the shutdown fails part-way must not publish a partial configuration
/// as if the session were still live.
#[tokio::test]
async fn a_setting_failed_by_a_host_shutdown_is_not_published() {
    let agent = HeldRequestAgent::new("session/set_config_option");
    let launcher = FakeLauncher::new();
    launcher.push(agent.process());
    let cancel = CancelToken::new();
    let opened = AcpHarness::new(profile())
        .open_session(
            &cancellable_host(&launcher, &cancel),
            OpenSession::new("held-setting"),
        )
        .await
        .expect("expected a session");
    let session: Arc<dyn Session> = Arc::from(opened);
    let configuring = tokio::spawn({
        let session = Arc::clone(&session);
        async move {
            session
                .configure(
                    ConfigurationPatch::new()
                        .model(ConfigurationChange::Set(String::from("large"))),
                )
                .await
        }
    });
    agent.wait_until_entered().await;

    cancel.cancel();

    let result = tokio::time::timeout(Duration::from_secs(8), configuring)
        .await
        .expect("expected the held setting to end after the shutdown, received a pending call")
        .expect("expected the configure task not to panic");
    let error = match result {
        Err(error) => error,
        Ok(outcome) => panic!(
            "expected configure ended by a host shutdown: an error | received: Ok({outcome:?})"
        ),
    };
    assert!(
        matches!(
            error.cause(),
            Error::Cancelled { .. } | Error::Closed { .. }
        ),
        "expected error: Cancelled or Closed | received: {error:?}"
    );
    assert_children_reaped(&launcher).await;
}
