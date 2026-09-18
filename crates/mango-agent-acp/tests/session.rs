//! The harness driven end to end against [`FakeAcpAgent`], including the core conformance suite.
//!
//! Everything here goes through the real transport, the real dispatch loop and the real reducer; the
//! only thing replaced is the process, which the core's `FakeLauncher` hands over as scripted pipes.
//! That is deliberate: the parts most worth testing on this dialect are the ones the reducer's unit
//! tests cannot reach — that a turn ends exactly once, that an approval round trip lands, that a
//! cancel carries its reason, and that closing twice is not an error.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime};

use mango_agent_acp::testing::{Approval, FakeAcpAgent};
use mango_agent_acp::{AcpHarness, AcpProfile, SessionModeIds};
use mango_external_agents::testing::{FakeLauncher, FakeProcess, FrozenClock, RecordingBroker};
use mango_external_agents::{
    ApprovalRouting, BrokerDecision, CancelReason, CancelToken, Capability, Clock, CloseReason,
    Configuration, ConfigurationChange, ConfigurationOptionId, ConfigurationPatch,
    ConfigurationValue, DecisionSource, Dispatch, Error, EventKind, ExitStatus, Harness, HostContext, Limits,
    ManagedProcess, OpenSession, PermissionLevel, ProcessControl, ProcessLauncher, ResumeMode,
    Session, SessionStatus, SessionSubscription, TurnRequest, TurnStream, VendorInfo,
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
        .limits(Limits {
            approval_timeout: Duration::from_secs(120),
            ..Limits::default()
        })
        .build()
        .expect("expected a host")
}

/// A launcher that exposes the child control injected into a short-lived picker connection.
#[derive(Clone, Default)]
struct CapturingLauncher {
    inner: FakeLauncher,
    control: Arc<Mutex<Option<Arc<dyn ProcessControl>>>>,
    kills: Arc<AtomicUsize>,
}

impl CapturingLauncher {
    fn push(&self, process: FakeProcess) {
        self.inner.push(process);
    }

    fn child(&self) -> Arc<dyn ProcessControl> {
        self.control
            .lock()
            .expect("expected captured child state")
            .clone()
            .expect("expected picker child")
    }

    fn kill_count(&self) -> usize {
        self.kills.load(Ordering::Acquire)
    }
}

/// Counts the cleanup calls made to an injected child control.
struct CountingProcessControl {
    inner: Arc<dyn ProcessControl>,
    kills: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ProcessControl for CountingProcessControl {
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
        self.kills.fetch_add(1, Ordering::AcqRel);
        self.inner.kill(reason).await
    }
}

#[async_trait::async_trait]
impl ProcessLauncher for CapturingLauncher {
    async fn spawn(
        &self,
        spec: mango_external_agents::LaunchSpec,
    ) -> mango_external_agents::Result<ManagedProcess> {
        let mut process = self.inner.spawn(spec).await?;
        let control: Arc<dyn ProcessControl> = Arc::new(CountingProcessControl {
            inner: Arc::clone(&process.control),
            kills: Arc::clone(&self.kills),
        });
        *self.control.lock().expect("expected captured child state") = Some(Arc::clone(&control));
        process.control = control;
        Ok(process)
    }
}

/// A named ACP peer that publishes a newer catalog notification before returning a stale option response.
struct InterleavingConfigAgent;

impl InterleavingConfigAgent {
    fn process() -> FakeProcess {
        FakeProcess::responding(Self::answer)
    }

    fn answer(line: &str) -> Vec<String> {
        let message: serde_json::Value =
            serde_json::from_str(line).expect("expected harness JSON-RPC request");
        let id = message["id"].clone();
        match message["method"].as_str() {
            Some("initialize") => vec![Self::result(
                id,
                serde_json::json!({
                    "protocolVersion": 1,
                    "agentInfo": { "name": "interleaving-fake", "version": "1" },
                    "agentCapabilities": {
                        "loadSession": true,
                        "promptCapabilities": { "image": true, "embeddedContext": true },
                        "sessionCapabilities": {},
                    },
                    "authMethods": [],
                }),
            )],
            Some("session/new") => vec![Self::result(
                id,
                serde_json::json!({
                    "sessionId": "sess_fake",
                    "configOptions": [Self::model_option("small")],
                }),
            )],
            Some("session/set_config_option") => vec![
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": "sess_fake",
                        "update": {
                            "sessionUpdate": "config_option_update",
                            "configOptions": [Self::model_option("newer")],
                        },
                    },
                })
                .to_string(),
                Self::result(
                    id,
                    serde_json::json!({ "configOptions": [Self::model_option("large")] }),
                ),
            ],
            _ => vec![Self::result(id, serde_json::json!({}))],
        }
    }

    fn model_option(current: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": current,
            "options": [
                { "value": "small", "name": "Small" },
                { "value": "large", "name": "Large" },
                { "value": "newer", "name": "Newer" },
            ],
        })
    }

    fn result(id: serde_json::Value, value: serde_json::Value) -> String {
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": value }).to_string()
    }
}

/// An ACP peer whose accepted model change exposes a new configuration row in its replacement
/// catalog. It proves semantic native checks use the catalog current at each wire submission.
struct ReclassifyingCatalogAgent;

impl ReclassifyingCatalogAgent {
    fn process() -> FakeProcess {
        FakeProcess::responding(Self::answer)
    }

    fn answer(line: &str) -> Vec<String> {
        let request: serde_json::Value =
            serde_json::from_str(line).expect("expected harness JSON-RPC request");
        let id = request["id"].clone();
        let response = match request["method"].as_str() {
            Some("initialize") => serde_json::json!({
                "protocolVersion": 1,
                "agentInfo": { "name": "reclassifying-fake", "version": "1" },
                "agentCapabilities": {
                    "loadSession": true,
                    "promptCapabilities": { "image": true, "embeddedContext": true },
                    "sessionCapabilities": {},
                },
                "authMethods": [],
            }),
            Some("session/new") => serde_json::json!({
                "sessionId": "reclassifying-session",
                "configOptions": [InterleavingConfigAgent::model_option("small")],
            }),
            Some("session/set_config_option") => serde_json::json!({
                "configOptions": [
                    InterleavingConfigAgent::model_option("large"),
                    {
                        "id": "mode", "name": "Mode", "category": "mode", "type": "select",
                        "currentValue": "plan", "options": [
                            { "value": "plan", "name": "Plan" },
                            { "value": "code", "name": "Code" }
                        ]
                    }
                ],
            }),
            _ => serde_json::json!({}),
        };
        vec![InterleavingConfigAgent::result(id, response)]
    }
}

/// A named ACP peer that announces a newer catalog while an opening response is in flight.
struct OpeningCatalogInterleavingAgent {
    method: &'static str,
}

impl OpeningCatalogInterleavingAgent {
    fn for_new() -> FakeProcess {
        Self {
            method: "session/new",
        }
        .process()
    }

    fn for_load() -> FakeProcess {
        Self {
            method: "session/load",
        }
        .process()
    }

    fn process(self) -> FakeProcess {
        FakeProcess::responding(move |line| self.answer(line))
    }

    fn answer(&self, line: &str) -> Vec<String> {
        let message: serde_json::Value =
            serde_json::from_str(line).expect("expected harness JSON-RPC request");
        let id = message["id"].clone();
        match message["method"].as_str() {
            Some("initialize") => vec![InterleavingConfigAgent::result(
                id,
                serde_json::json!({
                    "protocolVersion": 1,
                    "agentInfo": { "name": "opening-interleaving-fake", "version": "1" },
                    "agentCapabilities": {
                        "loadSession": true,
                        "promptCapabilities": { "image": true, "embeddedContext": true },
                        "sessionCapabilities": {},
                    },
                    "authMethods": [],
                }),
            )],
            Some(method) if method == self.method => {
                let session_id = if method == "session/load" {
                    String::from("resumed-session")
                } else {
                    String::from("new-session")
                };
                let mut response = serde_json::json!({
                    "configOptions": [InterleavingConfigAgent::model_option("stale")],
                });
                if method == "session/new" {
                    response["sessionId"] = serde_json::Value::String(session_id.clone());
                }
                vec![
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": session_id,
                            "update": {
                                "sessionUpdate": "config_option_update",
                                "configOptions": [InterleavingConfigAgent::model_option("newer")],
                            },
                        },
                    })
                    .to_string(),
                    InterleavingConfigAgent::result(id, response),
                ]
            }
            _ => vec![InterleavingConfigAgent::result(id, serde_json::json!({}))],
        }
    }
}

/// A named ACP peer that holds a set-option response until close has claimed the session.
#[derive(Clone)]
struct HeldSetOptionAgent {
    entered: Arc<AtomicBool>,
    release: Arc<(Mutex<bool>, Condvar)>,
}

impl HeldSetOptionAgent {
    fn new() -> Self {
        Self {
            entered: Arc::new(AtomicBool::new(false)),
            release: Arc::new((Mutex::new(false), Condvar::new())),
        }
    }

    fn process(self) -> FakeProcess {
        FakeProcess::responding(move |line| self.answer(line))
    }

    fn answer(&self, line: &str) -> Vec<String> {
        let message: serde_json::Value =
            serde_json::from_str(line).expect("expected harness JSON-RPC request");
        let id = message["id"].clone();
        let response = match message["method"].as_str() {
            Some("initialize") => serde_json::json!({
                "protocolVersion": 1,
                "agentInfo": { "name": "held-option-fake", "version": "1" },
                "agentCapabilities": {
                    "promptCapabilities": { "image": true, "embeddedContext": true },
                    "sessionCapabilities": {},
                },
                "authMethods": [],
            }),
            Some("session/new") => serde_json::json!({
                "sessionId": "held-option-session",
                "configOptions": [InterleavingConfigAgent::model_option("small")],
            }),
            Some("session/set_config_option") => {
                self.entered.store(true, Ordering::Release);
                let (lock, ready) = &*self.release;
                let mut released = lock.lock().expect("expected set-option release gate");
                while !*released {
                    released = ready.wait(released).expect("expected release notification");
                }
                serde_json::json!({
                    "configOptions": [InterleavingConfigAgent::model_option("large")],
                })
            }
            _ => serde_json::json!({}),
        };
        vec![InterleavingConfigAgent::result(id, response)]
    }

    fn release(&self) {
        let (lock, ready) = &*self.release;
        *lock.lock().expect("expected set-option release gate") = true;
        ready.notify_all();
    }
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

/// A named clock that blocks the next read after a test arms it, until the test releases it.
///
/// Armed rather than counted. Opening a session reads the host's clock more than once — the
/// opening snapshot's stamp, and again every time the handshake publishes a session fact through
/// `SessionState` — so a clock that blocked "the second read" would block inside `open_session`
/// and never return. Arming after the session is open makes the next read the one a test means:
/// `EventSink::emit`'s stamp on `TurnStarted`, the first event a turn can produce. That is the
/// window a `close` has to be forced into, between the turn handle being installed and its prompt
/// being written to the wire.
#[derive(Debug, Default)]
struct TurnStartedClock {
    state: Mutex<TurnStartedClockState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct TurnStartedClockState {
    armed: bool,
    blocked: bool,
    released: bool,
}

impl TurnStartedClock {
    /// Blocks the next read, and only the next one.
    fn arm(&self) {
        let mut state = self.state.lock().expect("expected the clock state");
        state.armed = true;
    }

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

impl Clock for TurnStartedClock {
    fn now(&self) -> SystemTime {
        let mut state = self.state.lock().expect("expected the clock state");
        if state.armed && !state.released {
            // Disarmed as it blocks, so the reads that follow the release run straight through.
            state.armed = false;
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

/// A patch that sets only the level, leaving every other axis untouched.
fn at_level(level: PermissionLevel) -> ConfigurationPatch {
    ConfigurationPatch::new().level(ConfigurationChange::Set(level))
}

fn permissive() -> ConfigurationPatch {
    at_level(PermissionLevel::Default)
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
    configuration: ConfigurationPatch,
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

/// The status a session settles on, once it stops changing.
///
/// Only the terminal is asserted on: a [`SessionSubscription`] coalesces, so a teardown that does
/// not block between its transitions legitimately shows a subscriber only the last one. Reports
/// the status it is stuck on rather than hanging to a bare timeout, so a lifecycle that never ends
/// fails as "received Ready".
async fn status_once_settled(lifecycle: &mut SessionSubscription) -> SessionStatus {
    loop {
        if lifecycle.current().status == SessionStatus::Closed {
            return SessionStatus::Closed;
        }
        if !matches!(
            tokio::time::timeout(Duration::from_secs(5), lifecycle.changed()).await,
            Ok(Some(_))
        ) {
            return lifecycle.current().status;
        }
    }
}

/// An agent that exits, or a transport that fails, ends the dispatch loop with no `close` in
/// sight. Nothing else on that path touches the lifecycle, so without a watcher the handle reports
/// `Ready` forever and a host learns the connection is dead only from the next request's failure.
#[tokio::test]
async fn an_agent_that_dies_ends_the_published_lifecycle() {
    let launcher = FakeLauncher::new();
    let agent_gone = CancelToken::new();
    launcher.push(
        FakeAcpAgent::new()
            .process()
            .ending_stdout_when(agent_gone.clone()),
    );
    let session = AcpHarness::new(profile())
        .open_session(
            &host(&launcher),
            OpenSession::new("chat-1").with_configuration(permissive()),
        )
        .await
        .expect("expected a session");
    let mut lifecycle = session.subscribe();
    assert_eq!(lifecycle.current().status, SessionStatus::Ready);

    agent_gone.cancel();

    assert_eq!(
        status_once_settled(&mut lifecycle).await,
        SessionStatus::Closed,
        "expected a dead dispatch loop to end the published lifecycle"
    );
    // The half a status assertion alone cannot see. `ending_stdout_when` closes the pipe and leaves
    // the child running, which is exactly the shape a watcher that publishes `Closed` without
    // reaping would report as "nothing more will happen".
    assert_eq!(
        launcher.live_children(),
        0,
        "expected Closed to mean the child is gone, not only that the pipe closed"
    );
}

/// The other half of the watcher, and the one a clean EOF cannot reach: a session dropped instead
/// of closed releases the dispatch loop's shutdown channel with the handle, so the loop ends with
/// no EOF to report. Without the driver-done arm the lifecycle would stop at `Ready` there.
#[tokio::test]
async fn a_session_dropped_without_a_close_still_ends_its_lifecycle() {
    let (session, launcher) = open(FakeAcpAgent::new(), permissive()).await;
    let mut lifecycle = session.subscribe();
    assert_eq!(lifecycle.current().status, SessionStatus::Ready);

    drop(session);

    assert_eq!(
        status_once_settled(&mut lifecycle).await,
        SessionStatus::Closed,
        "expected a dropped session to reach its terminal rather than stay Ready forever"
    );
    assert_eq!(
        launcher.live_children(),
        0,
        "expected the watcher to reap the child the dropped session abandoned"
    );
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
        matches!(events.first(), Some(EventKind::TurnStarted { .. })),
        "expected the turn to be named first, received {events:?}"
    );
    // The vendor's own session id, which used to ride the first turn event, is session state now:
    // it has to reach the host through `SessionState::set_native_session_id` instead.
    assert_eq!(session.ids().native_session_id, "sess_fake");
    assert!(
        events
            .iter()
            .any(|kind| matches!(kind, EventKind::TextDelta { text } if text == "hello")),
        "received {events:?}"
    );
    // The command catalog is session state now, not a turn event: it lands on the session's own
    // snapshot rather than in the stream.
    assert!(
        !session.snapshot().commands.is_empty(),
        "expected the announced command catalog to reach session state"
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
        !turn.native_turn_id().is_empty(),
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
        at_level(PermissionLevel::ReadOnly),
    )
    .await;
    assert_eq!(
        session.snapshot().configuration.accepted.level,
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
        at_level(PermissionLevel::ReadOnly),
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
        ConfigurationPatch::new(),
    )
    .await;

    let mut first = session
        .start_turn(
            TurnRequest::new("turn-1", "delete the build")
                .with_configuration(at_level(PermissionLevel::ReadOnly)),
        )
        .await
        .expect("expected the explicit restriction to be accepted");
    assert!(has_automatic_refusal(&drain(&mut first).await));
    assert_eq!(
        session.snapshot().configuration.accepted,
        Configuration::unknown().with_level(PermissionLevel::ReadOnly)
    );

    let mut second = session
        .start_turn(
            TurnRequest::new("turn-2", "delete the build").with_configuration(
                ConfigurationPatch::new()
                    .routing(ConfigurationChange::Set(ApprovalRouting::AutoReview)),
            ),
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
        session.snapshot().configuration.accepted,
        Configuration::unknown()
            .with_level(PermissionLevel::ReadOnly)
            .with_routing(ApprovalRouting::AutoReview)
    );
}

/// An agent that offers nothing to refuse with leaves nothing for a standing refusal to pick, so the
/// question has to reach a person rather than being answered with whatever was first in the list.
#[tokio::test]
async fn a_read_only_session_still_asks_when_the_agent_offered_no_way_to_refuse() {
    let (session, _launcher) = open(
        FakeAcpAgent::new().asking_for_approval(Approval::OnlyAllows),
        at_level(PermissionLevel::ReadOnly),
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
        ConfigurationPatch::new(),
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
/// race for one stream of updates with nothing on the wire to tell them apart. The refusal is typed
/// as busy, so a host can wait for the owned turn instead of treating active work as malformed input.
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

    assert!(matches!(error.cause(), Error::Busy), "received {error:?}");
}

/// ACP queues a prompt locally but does not acknowledge that the peer received it.
#[tokio::test]
async fn an_accepted_acp_prompt_keeps_replay_safety_unknown() {
    let (session, _launcher) =
        open(FakeAcpAgent::new().never_finishing_turns(), permissive()).await;

    let turn = session
        .start_turn(TurnRequest::new("turn-1", "one"))
        .await
        .expect("expected a prompt stream");

    assert_eq!(turn.dispatch(), Dispatch::AcceptanceUnknown);
}

/// Revocation between session setup and prompt admission must not queue native work.
#[tokio::test]
async fn a_revoked_host_cannot_admit_an_acp_prompt() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeAcpAgent::new().never_finishing_turns().process());
    let cancel = CancelToken::new();
    let host = HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .cancel(cancel.clone())
        .build()
        .expect("expected a host");
    let session = AcpHarness::new(profile())
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session before revocation");

    cancel.cancel();
    let error = refusal(session.start_turn(TurnRequest::new("turn-1", "one")).await);
    assert!(
        matches!(
            error.cause(),
            Error::Cancelled {
                reason: CancelReason::Shutdown
            }
        ),
        "received {error:?}"
    );
    assert_eq!(error.dispatch(), Dispatch::NotSubmitted);
    assert!(
        !launcher
            .written()
            .iter()
            .any(|line| line.contains("session/prompt")),
        "revoked host must not queue a prompt, received {:?}",
        launcher.written()
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
    assert_eq!(
        session.snapshot().status,
        mango_external_agents::SessionStatus::Closed,
        "expected a closed session to say so on its own snapshot"
    );

    let error = session
        .start_turn(TurnRequest::new("turn-1", "still there?"))
        .await
        .expect_err("expected a closed session");
    assert!(
        matches!(error.cause(), Error::Closed { subject: "session" }),
        "received {error:?}"
    );
}

/// `TurnStarted` is emitted after the turn handle is installed. If close wins while that event is
/// stalled, releasing the event must not let the starter submit a detached `session/prompt`.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_close_between_handle_installation_and_prompt_write_refuses_the_detached_turn() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeAcpAgent::new().process());
    let clock = Arc::new(TurnStartedClock::default());
    let host = host_with_clock(&launcher, Arc::clone(&clock) as Arc<dyn Clock>);
    let opened = AcpHarness::new(profile())
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let session: Arc<dyn Session> = Arc::from(opened);
    let configuration_before = session.snapshot().configuration.clone();

    // Armed only now: opening reads the clock for its own snapshot and again for every session
    // fact the handshake publishes, and blocking any of those would stall the open itself.
    clock.arm();
    let starting = {
        let session = Arc::clone(&session);
        tokio::spawn(async move {
            session
                .start_turn(
                    TurnRequest::new("turn-1", "one")
                        .with_configuration(at_level(PermissionLevel::ReadOnly)),
                )
                .await
        })
    };
    clock.wait_until_blocked();

    let mut closing = {
        let session = Arc::clone(&session);
        tokio::spawn(async move { session.close(CloseReason::ConsentRevoked).await })
    };
    let close_result = tokio::time::timeout(Duration::from_secs(2), &mut closing).await;
    let close_won = close_result.is_ok();
    clock.release();
    if let Ok(result) = close_result {
        result
            .expect("expected close task")
            .expect("expected close");
    } else {
        closing
            .await
            .expect("expected close task")
            .expect("expected close");
    }

    let error = refusal(
        starting
            .await
            .expect("expected the start task to return a result"),
    );
    assert!(
        close_won,
        "expected close to finish while TurnStarted is stalled; received a close blocked by configuration publication"
    );
    assert_eq!(
        session.snapshot().configuration,
        configuration_before,
        "a turn closed before submission must not publish its configuration"
    );
    assert!(
        matches!(error.cause(), Error::Closed { subject: "session" }),
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
    assert!(!silent.capabilities().has(Capability::SessionListing));
    let error = silent
        .list_sessions(Default::default())
        .await
        .expect_err("expected a refusal");
    assert!(
        matches!(error.cause(), Error::NotSupported { .. }),
        "received {error:?}"
    );

    let (listing, _launcher) = open(FakeAcpAgent::new().listing_sessions(), permissive()).await;
    assert!(listing.capabilities().has(Capability::SessionListing));
    let page = listing
        .list_sessions(Default::default())
        .await
        .expect("expected a page");
    assert_eq!(page.sessions.len(), 1);
    assert_eq!(page.sessions[0].native_session_id, "sess_old");
    assert_eq!(page.sessions[0].title.as_deref(), Some("Yesterday"));
}

/// A request the agent never answers remains in ACP's own pending-reply map after its future is
/// dropped. The harness therefore has to close request admission and own cleanup on timeout rather
/// than releasing a permit that would let another request accumulate behind the silent peer.
#[tokio::test(start_paused = true)]
async fn a_timed_out_generic_request_closes_admission_and_reaps_the_silent_peer() {
    let launcher = FakeLauncher::new();
    launcher.push(SilentListingAgent::process());
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
    let session = AcpHarness::new(profile())
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");

    let timed_out = session
        .list_sessions(Default::default())
        .await
        .expect_err("expected the silent list request to time out");
    assert!(
        matches!(timed_out.cause(), Error::Timeout { .. }),
        "received {timed_out:?}"
    );
    let refused = session
        .list_sessions(Default::default())
        .await
        .expect_err("timed-out request must close later generic admission");
    assert!(
        matches!(
            refused.cause(),
            Error::Closed {
                subject: "ACP connection"
            }
        ),
        "received {refused:?}"
    );
    assert_eq!(
        launcher
            .written()
            .iter()
            .filter(|line| line.contains("\"session/list\""))
            .count(),
        1,
        "the closed admission must not queue a second silent request"
    );
    tokio::time::timeout(Duration::from_secs(30), async {
        while launcher.live_children() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected owned shutdown to reap the silent peer");
}

/// A caller can abandon a generic request before its own deadline. ACP only queues a cancellation
/// notification in that case, so the harness must still seal admission and let its owned teardown
/// release the SDK's reply slot without waiting for another caller to close the session.
#[tokio::test]
async fn an_abandoned_generic_request_closes_admission_and_reaps_the_silent_peer() {
    let launcher = FakeLauncher::new();
    launcher.push(SilentListingAgent::process());
    let host = HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .limits(Limits {
            request_timeout: Duration::from_secs(30),
            ..Limits::default()
        })
        .build()
        .expect("expected a host");
    let opened = AcpHarness::new(profile())
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let session: Arc<dyn Session> = Arc::from(opened);

    let listing = {
        let session = Arc::clone(&session);
        tokio::spawn(async move { session.list_sessions(Default::default()).await })
    };
    tokio::time::timeout(Duration::from_millis(250), async {
        while !launcher
            .written()
            .iter()
            .any(|line| line.contains("\"session/list\""))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected the silent list request to enter ACP");
    listing.abort();
    let _ = listing.await;

    let refused = session
        .list_sessions(Default::default())
        .await
        .expect_err("abandonment must close later generic admission");
    assert!(
        matches!(
            refused.cause(),
            Error::Closed {
                subject: "ACP connection"
            }
        ),
        "received {refused:?}"
    );
    assert_eq!(
        launcher
            .written()
            .iter()
            .filter(|line| line.contains("\"session/list\""))
            .count(),
        1,
        "the closed admission must not queue another silent request"
    );
    tokio::time::timeout(Duration::from_millis(250), async {
        while launcher.live_children() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected abandoned request cleanup to reap the silent peer");
}

/// ACP v1 has no page-size request field, so an oversized reply cannot be cut without losing
/// rows behind the agent's cursor.
#[tokio::test]
async fn listing_refuses_more_workspace_rows_than_the_host_requested() {
    for (limit, received) in [(1, 2), (50, 51)] {
        let launcher = FakeLauncher::new();
        let host = host(&launcher);
        let rows = (0..received)
            .map(|index| {
                serde_json::json!({
                    "sessionId": format!("sess_{index}"),
                    "cwd": host.cwd(),
                })
            })
            .collect();
        launcher.push(FakeAcpAgent::new().with_listed_sessions(rows).process());
        let error = AcpHarness::new(profile())
            .list_sessions(
                &host,
                mango_external_agents::SessionQuery {
                    limit: Some(limit),
                    ..Default::default()
                },
            )
            .await
            .expect_err("expected an oversized ACP page to be refused without losing rows");
        assert!(
            matches!(
                error.cause(),
                Error::LimitExceeded {
                    subject: "ACP session/list rows in one page",
                    limit: actual_limit,
                    received: actual_received,
                } if *actual_limit == limit && *actual_received == received
            ),
            "expected a bounded-page refusal for {received} rows over limit {limit}, received {error:?}"
        );
    }
}

/// Picker listing uses a short-lived initialized ACP connection; it must never create a conversation.
#[tokio::test]
async fn harness_listing_is_workspace_bound_before_any_conversation_opens() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeAcpAgent::new().listing_sessions().process());
    let harness = AcpHarness::new(profile());
    let host = host(&launcher);
    let page = harness
        .list_sessions(&host, Default::default())
        .await
        .expect("expected the advertised picker listing");
    assert_eq!(page.sessions.len(), 1);
    assert_eq!(
        page.sessions[0].workspace_path.as_deref(),
        host.cwd().to_str()
    );
    assert!(
        page.sessions[0].updated_at.is_some(),
        "expected RFC3339 updatedAt to map"
    );
    let requests = launcher.written();
    assert!(
        requests
            .iter()
            .any(|line| line.contains("\"session/list\"")),
        "expected session/list, received {requests:?}"
    );
    assert!(
        !requests.iter().any(|line| line.contains("\"session/new\"")),
        "expected picker listing not to create a conversation, received {requests:?}"
    );
    let list: serde_json::Value = requests
        .iter()
        .find(|line| line.contains("\"session/list\""))
        .map(|line| serde_json::from_str(line).expect("expected JSON-RPC"))
        .expect("expected session/list request");
    assert_eq!(list["params"]["cwd"], host.cwd().display().to_string());
}

/// A caller cannot use an ACP picker to query a directory the host did not authorize.
#[tokio::test]
async fn harness_listing_refuses_a_different_workspace_before_launch() {
    let launcher = FakeLauncher::new();
    let harness = AcpHarness::new(profile());
    let error = harness
        .list_sessions(
            &host(&launcher),
            mango_external_agents::SessionQuery {
                workspace_path: Some("/another/workspace".into()),
                ..Default::default()
            },
        )
        .await
        .expect_err("expected a cross-workspace picker query to be refused");
    assert!(
        matches!(error, Error::HostConfiguration { .. }),
        "received {error:?}"
    );
    assert!(
        launcher.launches().is_empty(),
        "expected no child, received {:?}",
        launcher.launches()
    );
}

/// ACP puts the workspace on `session/new`, `session/load`, and `session/list`; a relative host
/// path must be rejected before any of those requests can cause the launcher to create a child.
#[tokio::test]
async fn acp_refuses_a_noncanonical_workspace_before_launch() {
    let launcher = FakeLauncher::new();
    let host = HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd("relative-workspace")
        .client_info("mea-tests", "0.1.0")
        .build()
        .expect("expected a host");
    let harness = AcpHarness::new(profile());

    let opening = refusal(
        harness
            .open_session(&host, OpenSession::new("relative-workspace"))
            .await,
    );
    assert!(
        matches!(
            opening.cause(),
            Error::HostConfiguration {
                expected: "an absolute, lexically normalized UTF-8 workspace path",
                ..
            }
        ),
        "received {opening:?}"
    );
    assert_eq!(opening.dispatch(), Dispatch::NotSubmitted);

    let listing = harness
        .list_sessions(&host, Default::default())
        .await
        .expect_err("expected a noncanonical picker workspace to be refused");
    assert!(
        matches!(
            listing.cause(),
            Error::HostConfiguration {
                expected: "an absolute, lexically normalized UTF-8 workspace path",
                ..
            }
        ),
        "received {listing:?}"
    );
    assert!(
        launcher.launches().is_empty(),
        "expected no child for either invalid workspace request, received {:?}",
        launcher.launches()
    );
}

/// Aborting a picker while its list request is held still ends the injected child process.
#[tokio::test]
async fn aborting_harness_listing_shuts_down_its_short_lived_child() {
    let launcher = CapturingLauncher::default();
    launcher.push(FakeAcpAgent::new().holding_listing().process());
    let host = HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .build()
        .expect("expected a host");
    let harness = Arc::new(AcpHarness::new(profile()));
    let task = tokio::spawn({
        let harness = Arc::clone(&harness);
        let host = host.clone();
        async move { harness.list_sessions(&host, Default::default()).await }
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while !launcher
            .inner
            .written()
            .iter()
            .any(|line| line.contains("\"session/list\""))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected the fake to hold session/list");
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_secs(4), launcher.child().wait())
        .await
        .expect("expected the dropped picker scope to kill its child")
        .expect("expected child cleanup to succeed");
    assert_eq!(
        launcher.kill_count(),
        1,
        "expected the cancellation guard to ask the injected control to kill exactly once"
    );
}

/// An explicit listing cleanup disarms its drop guard after the one child kill.
#[tokio::test]
async fn completed_harness_listing_kills_its_short_lived_child_once() {
    let launcher = CapturingLauncher::default();
    launcher.push(FakeAcpAgent::new().listing_sessions().process());
    let host = HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .build()
        .expect("expected a host");
    AcpHarness::new(profile())
        .list_sessions(&host, Default::default())
        .await
        .expect("expected a listing");
    tokio::time::timeout(Duration::from_secs(4), launcher.child().wait())
        .await
        .expect("expected the picker child to exit")
        .expect("expected child cleanup to succeed");
    assert_eq!(
        launcher.kill_count(),
        1,
        "expected explicit cleanup not to be repeated from ConnectionShutdownGuard::drop"
    );
}

/// Ordinary close and the connection watcher share one claim on the injected child.
#[tokio::test]
async fn closing_a_session_and_its_watcher_kills_the_child_once() {
    let launcher = CapturingLauncher::default();
    launcher.push(FakeAcpAgent::new().process());
    let host = HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .build()
        .expect("expected a host");
    let session = AcpHarness::new(profile())
        .open_session(&host, OpenSession::new("close-once"))
        .await
        .expect("expected a session");
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected close");
    tokio::time::timeout(Duration::from_secs(4), launcher.child().wait())
        .await
        .expect("expected close to reap its child")
        .expect("expected child cleanup to succeed");
    for _ in 0..128 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        launcher.kill_count(),
        1,
        "expected close and watcher to make one process-control kill request"
    );
}

/// Rows outside the host workspace and malformed timestamps never reach a picker as local sessions.
#[tokio::test]
async fn listing_filters_foreign_rows_and_keeps_only_valid_updated_at() {
    let launcher = FakeLauncher::new();
    let workspace = std::env::temp_dir().display().to_string();
    launcher.push(
        FakeAcpAgent::new()
            .with_listed_sessions(vec![
                serde_json::json!({
                    "sessionId": "foreign", "cwd": "/another/workspace", "title": "Foreign",
                    "updatedAt": "2026-09-17T12:34:56Z"
                }),
                serde_json::json!({ "sessionId": "missing-cwd", "title": "Malformed" }),
                serde_json::json!({
                    "sessionId": "bad-time", "cwd": workspace, "title": "Local",
                    "updatedAt": "not-a-timestamp"
                }),
            ])
            .process(),
    );
    let harness = AcpHarness::new(profile());
    let page = harness
        .list_sessions(&host(&launcher), Default::default())
        .await
        .expect("expected the safe local listing");
    assert_eq!(
        page.sessions.len(),
        1,
        "expected the foreign row to be excluded"
    );
    assert_eq!(page.sessions[0].native_session_id, "bad-time");
    assert_eq!(
        page.sessions[0].updated_at, None,
        "expected malformed RFC3339 to stay unknown"
    );
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
            OpenSession::new("chat-1").with_configuration(at_level(PermissionLevel::FullAccess)),
        )
        .await
        .expect("expected a session");

    let error = refusal(
        session
            .start_turn(
                TurnRequest::new("turn-1", "just read")
                    .with_configuration(at_level(PermissionLevel::ReadOnly)),
            )
            .await,
    );
    assert!(
        matches!(error.cause(), Error::Protocol { expected, .. } if expected.contains("the session's own level")),
        "received {error:?}"
    );
    // The refusal names the relationship, not the agent's mode id.
    assert!(
        !error.to_string().contains("bypassPermissions"),
        "received {error:?}"
    );
}

/// `close` must not drop a half-sent terminal. A timeout that abandoned the `emit` would send nothing —
/// `mpsc::Sender::send` is cancel-safe — so a host that stopped reading and then closed would get a
/// stream that just ends, with no terminal, which the core's conformance rules refuse. Depending on
/// whether close or the full transcript wins first, that terminal is either the existing overflow
/// error or close's cancellation pair.
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

    // The transcript budget is full. Its reserved terminal must still be observable after close.
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
        matches!(
            events.last(),
            Some(EventKind::Completed | EventKind::Error { .. })
        ),
        "expected the committed terminal to remain last, received {events:?}"
    );
}

/// The close operation owns its shutdown and terminal work after it claims the lifecycle. Dropping
/// the initiating future at a blocked process kill must not leave the session stuck in `Closing`.
#[tokio::test]
async fn a_dropped_close_finishes_cleanup_status_and_the_owned_turn() {
    let inner = FakeLauncher::new();
    inner.push(FakeAcpAgent::new().never_finishing_turns().process());
    let launcher = GatedLauncher::new(inner.clone(), false);
    let opened = AcpHarness::new(profile())
        .open_session(&gated_host(&launcher), OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let session: Arc<dyn Session> = Arc::from(opened);
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "keep working"))
        .await
        .expect("expected a turn");

    let closing = {
        let session = Arc::clone(&session);
        tokio::spawn(async move { session.close(CloseReason::Shutdown).await })
    };
    launcher.wait_for_kill().await;
    closing.abort();
    launcher.release();

    tokio::time::timeout(Duration::from_millis(250), async {
        while session.snapshot().status != SessionStatus::Closed || inner.live_children() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected the owned close task to finish after its caller was dropped");

    let events = drain(&mut turn).await;
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, EventKind::Completed | EventKind::Error { .. }))
            .count(),
        1,
        "expected one terminal after the dropped close, received {events:?}"
    );
}

/// A second close is a joiner, not a second cleanup attempt or an early success while the first
/// close is still waiting for the host process control.
#[tokio::test]
async fn concurrent_closes_wait_for_and_share_one_cleanup_result() {
    let inner = FakeLauncher::new();
    inner.push(FakeAcpAgent::new().never_finishing_turns().process());
    let launcher = GatedLauncher::new(inner, false);
    let opened = AcpHarness::new(profile())
        .open_session(&gated_host(&launcher), OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let session: Arc<dyn Session> = Arc::from(opened);

    let first = {
        let session = Arc::clone(&session);
        tokio::spawn(async move { session.close(CloseReason::Shutdown).await })
    };
    launcher.wait_for_kill().await;
    let mut second = {
        let session = Arc::clone(&session);
        tokio::spawn(async move { session.close(CloseReason::Requested).await })
    };
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut second)
            .await
            .is_err(),
        "a repeated close returned before the first cleanup completed"
    );

    launcher.release();
    first
        .await
        .expect("expected first close task")
        .expect("expected first close result");
    second
        .await
        .expect("expected second close task")
        .expect("expected second close result");
    assert_eq!(session.snapshot().status, SessionStatus::Closed);
}

/// A failed process cleanup means the session remains stopping. Reporting `Closed` or allowing a
/// later prompt would claim a child has gone away when the host said it did not.
#[tokio::test]
async fn a_failed_close_cleanup_returns_an_error_and_keeps_admission_closed() {
    let inner = FakeLauncher::new();
    inner.push(FakeAcpAgent::new().never_finishing_turns().process());
    let launcher = GatedLauncher::new(inner, true);
    let opened = AcpHarness::new(profile())
        .open_session(&gated_host(&launcher), OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let session: Arc<dyn Session> = Arc::from(opened);
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "keep working"))
        .await
        .expect("expected a turn");

    let close = {
        let launcher = launcher.clone();
        let session = Arc::clone(&session);
        let task = tokio::spawn(async move { session.close(CloseReason::Shutdown).await });
        launcher.wait_for_kill().await;
        launcher.release();
        task.await.expect("expected close task")
    };
    assert!(
        close.is_err(),
        "expected failed process cleanup, received {close:?}"
    );
    assert_eq!(session.snapshot().status, SessionStatus::Closing);
    let events = drain(&mut turn).await;
    assert!(
        matches!(events.last(), Some(EventKind::Error { .. })),
        "expected failed cleanup to settle the owned turn, received {events:?}"
    );
    let refused = session
        .start_turn(TurnRequest::new("turn-2", "must not run"))
        .await
        .expect_err("a stopping session must reject new work");
    assert!(
        matches!(refused.cause(), Error::Closed { subject: "session" }),
        "received {refused:?}"
    );
    assert!(
        session.close(CloseReason::Requested).await.is_err(),
        "a repeated close must return the first cleanup failure"
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
            OpenSession::new("chat-1").with_configuration(at_level(PermissionLevel::ReadOnly)),
        )
        .await
        .expect("expected a session");
    assert_eq!(
        session.snapshot().configuration.accepted.level,
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
    assert!(matches!(error.cause(), Error::Busy), "received {error:?}");
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
                TurnRequest::new("turn-2", "two")
                    .with_configuration(at_level(PermissionLevel::ReadOnly)),
            )
            .await,
    );
    assert!(matches!(error.cause(), Error::Busy), "received {error:?}");
    assert_eq!(
        session.snapshot().configuration.accepted,
        Configuration::unknown().with_level(PermissionLevel::Default),
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

/// A custom profile's id is host-authored text, so a timeout must describe the ACP operation without
/// copying an identifier that could contain tenant data or credentials into diagnostics.
#[tokio::test(start_paused = true)]
async fn a_custom_profiles_id_stays_out_of_the_timeout_diagnostic() {
    // Profile ids now validate their syntax at construction. A valid tenant-bearing identifier
    // must still stay out of operation diagnostics rather than bypassing that contract here.
    let hostile_id = "profile-secret";
    let launcher = FakeLauncher::new();
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

    let opened = tokio::time::timeout(
        Duration::from_secs(600),
        AcpHarness::new(Arc::new(AcpProfile::custom(
            hostile_id,
            ["fake-acp", "acp"],
            VENDOR,
        )))
        .open_session(&host, OpenSession::new("chat-1")),
    )
    .await
    .expect("expected the handshake to give up on its own deadline");

    let error = refusal(opened);
    let Error::Timeout { operation, .. } = &error else {
        panic!("received {error:?}");
    };
    assert_eq!(operation, "initialize on an ACP agent");
    assert!(
        !operation.contains("profile-secret"),
        "received a caller-owned profile id in diagnostics: {operation:?}"
    );
}

/// Once the transport is ready, an agent can still exit under `initialize` or `session/new`. The
/// opening call returns no control handle, so its redacted stderr must travel through the typed
/// vendor failure before cleanup drops the handle.
#[tokio::test]
async fn an_agent_that_exits_after_connecting_keeps_its_stderr_on_the_open_failure() {
    let close_stdout = mango_external_agents::CancelToken::new();
    let close_after_initialize = close_stdout.clone();
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeProcess::responding(move |line| {
            let message: serde_json::Value =
                serde_json::from_str(line).expect("expected a JSON-RPC request");
            let method = message["method"].as_str();
            let id = message["id"].clone();
            if method == Some("initialize") {
                return vec![
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": 1,
                            "agentInfo": { "name": "fake-acp", "version": "1.2.3" },
                            "agentCapabilities": {
                                "loadSession": true,
                                "promptCapabilities": { "image": true, "embeddedContext": true },
                                "sessionCapabilities": {},
                            },
                            "authMethods": [],
                        },
                    })
                    .to_string(),
                ];
            }
            if method == Some("session/new") {
                close_after_initialize.cancel();
            }
            Vec::new()
        })
        .ending_stdout_when(close_stdout)
        .with_stderr("invalid ACP configuration"),
    );

    let error = refusal(
        AcpHarness::new(profile())
            .open_session(&host(&launcher), OpenSession::new("chat-1"))
            .await,
    );
    let Error::Vendor(vendor) = &error else {
        panic!("expected a vendor failure, received {error:?}");
    };
    assert_eq!(vendor.code.as_str(), "acp-link-closed");
    assert!(
        vendor.message.contains("invalid ACP configuration"),
        "expected the redacted stderr tail on the typed field, received {:?}",
        vendor.message
    );
    for rendered in [error.to_string(), format!("{error:?}")] {
        assert!(
            !rendered.contains("invalid ACP configuration"),
            "expected the stderr tail to stay out of diagnostics, received {rendered:?}"
        );
    }
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
                TurnRequest::new("turn-1", "do everything")
                    .with_configuration(at_level(PermissionLevel::FullAccess)),
            )
            .await,
    );
    assert!(
        matches!(error.cause(), Error::HostConfiguration { .. }),
        "received {error:?}"
    );
}

/// A custom profile's id is host-authored text: the host names its own in-house agent, and that
/// name may carry a tenant or a credential. Both pair refusals — `open_session`'s and the turn's —
/// summarised it into a diagnostic the formatter writes verbatim.
#[tokio::test]
async fn a_custom_profile_id_stays_out_of_pair_refusals() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeAcpAgent::new().process());
    let harness = AcpHarness::new(Arc::new(AcpProfile::custom(
        "tenant-secret",
        ["fake-acp", "acp"],
        VENDOR,
    )));

    // `Box<dyn Session>` is not `Debug`, so the success arm is named rather than unwrapped.
    let refused_open = match harness
        .open_session(
            &host(&launcher),
            OpenSession::new("chat-1").with_configuration(at_level(PermissionLevel::FullAccess)),
        )
        .await
    {
        Ok(_) => panic!("expected an unsupported pair to be refused"),
        Err(error) => error,
    };

    let session = harness
        .open_session(
            &host(&launcher),
            OpenSession::new("chat-2").with_configuration(permissive()),
        )
        .await
        .expect("expected a session");
    let refused_turn = refusal(
        session
            .start_turn(
                TurnRequest::new("turn-1", "do everything")
                    .with_configuration(at_level(PermissionLevel::FullAccess)),
            )
            .await,
    );

    for error in [&refused_open, &refused_turn] {
        assert!(
            matches!(error.cause(), Error::HostConfiguration { .. }),
            "expected a host-configuration refusal, received {error:?}"
        );
        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(
                !rendered.contains("tenant-secret"),
                "expected the profile id to stay out of diagnostics, received {rendered:?}"
            );
            assert!(
                rendered.contains("FullAccess"),
                "expected the refused level to survive the summary, received {rendered:?}"
            );
        }
    }
}

/// A custom profile's mode ids are host-authored too, and `Error::Protocol` writes `expected`
/// verbatim — it has no shape gate of its own, unlike the code and profile-id renderings. Both
/// mode refusals named the id: the one for a mode the agent never advertised, and the one for a
/// turn whose level would need a different mode than the session opened with.
#[tokio::test]
async fn a_custom_mode_id_stays_out_of_protocol_refusals() {
    let profile = || {
        Arc::new(
            AcpProfile::custom("in-house", ["fake-acp", "acp"], VENDOR).with_modes(
                SessionModeIds {
                    read_only: None,
                    default: Some("tenant credential=session-mode-secret"),
                    full_access: Some("tenant credential=turn-mode-secret"),
                },
            ),
        )
    };

    // The agent advertises neither id, so `session/new` is refused before a turn exists.
    let unadvertised = FakeLauncher::new();
    unadvertised.push(FakeAcpAgent::new().process());
    let refused_open = match AcpHarness::new(profile())
        .open_session(
            &host(&unadvertised),
            OpenSession::new("chat-1").with_configuration(at_level(PermissionLevel::Default)),
        )
        .await
    {
        Ok(_) => panic!("expected an unadvertised mode to be refused"),
        Err(error) => error,
    };

    // The agent advertises the session's mode, so the session opens and the turn is refused for
    // wanting a level the session's own mode does not cover.
    let advertised = FakeLauncher::new();
    advertised.push(
        FakeAcpAgent::new()
            .with_modes(["tenant credential=session-mode-secret"])
            .process(),
    );
    let session = AcpHarness::new(profile())
        .open_session(
            &host(&advertised),
            OpenSession::new("chat-2").with_configuration(at_level(PermissionLevel::Default)),
        )
        .await
        .expect("expected a session");
    let refused_turn = refusal(
        session
            .start_turn(
                TurnRequest::new("turn-1", "do everything")
                    .with_configuration(at_level(PermissionLevel::FullAccess)),
            )
            .await,
    );

    for error in [&refused_open, &refused_turn] {
        assert!(
            matches!(error.cause(), Error::Protocol { .. }),
            "expected a protocol refusal, received {error:?}"
        );
        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(
                !rendered.contains("secret"),
                "expected the mode id to stay out of diagnostics, received {rendered:?}"
            );
        }
    }
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

/// A launcher whose process cleanup waits at a test-controlled gate.
///
/// Keeping the gate in `ProcessControl::kill` makes the test exercise the real ACP connection
/// shutdown path, including the point at which a caller might drop its `close` future.
#[derive(Clone)]
struct GatedLauncher {
    inner: FakeLauncher,
    kill_started: CancelToken,
    release_kill: CancelToken,
    fail_kill: bool,
}

/// An ACP peer that completes setup but deliberately never answers `session/list`.
struct SilentListingAgent;

impl SilentListingAgent {
    fn process() -> FakeProcess {
        FakeProcess::responding(|line| {
            let Ok(request) = serde_json::from_str::<serde_json::Value>(line) else {
                return Vec::new();
            };
            let Some(id) = request.get("id") else {
                return Vec::new();
            };
            let response = |result: serde_json::Value| {
                serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
            };
            match request.get("method").and_then(serde_json::Value::as_str) {
                Some("initialize") => vec![response(serde_json::json!({
                    "protocolVersion": 1,
                    "agentInfo": { "name": "silent-listing", "version": "1.0.0" },
                    "agentCapabilities": {
                        "loadSession": true,
                        "promptCapabilities": { "image": false, "embeddedContext": false },
                        "sessionCapabilities": { "list": {} }
                    },
                    "authMethods": []
                }))],
                Some("session/new") => vec![response(serde_json::json!({
                    "sessionId": "sess_silent"
                }))],
                Some("session/list") => Vec::new(),
                _ => Vec::new(),
            }
        })
    }
}

impl GatedLauncher {
    fn new(inner: FakeLauncher, fail_kill: bool) -> Self {
        Self {
            inner,
            kill_started: CancelToken::new(),
            release_kill: CancelToken::new(),
            fail_kill,
        }
    }

    async fn wait_for_kill(&self) {
        self.kill_started.cancelled().await;
    }

    fn release(&self) {
        self.release_kill.cancel();
    }
}

#[async_trait::async_trait]
impl mango_external_agents::ProcessLauncher for GatedLauncher {
    async fn spawn(
        &self,
        spec: mango_external_agents::LaunchSpec,
    ) -> mango_external_agents::Result<mango_external_agents::ManagedProcess> {
        let process = self.inner.spawn(spec).await?;
        Ok(mango_external_agents::ManagedProcess {
            control: Arc::new(GatedControl {
                inner: process.control,
                kill_started: self.kill_started.clone(),
                release_kill: self.release_kill.clone(),
                fail_kill: self.fail_kill,
            }),
            ..process
        })
    }
}

struct GatedControl {
    inner: Arc<dyn mango_external_agents::ProcessControl>,
    kill_started: CancelToken,
    release_kill: CancelToken,
    fail_kill: bool,
}

#[async_trait::async_trait]
impl mango_external_agents::ProcessControl for GatedControl {
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
        self.kill_started.cancel();
        self.release_kill.cancelled().await;
        if self.fail_kill {
            return Err(Error::Launch {
                program: String::from("fake ACP agent"),
                message: String::from("the test process refused termination"),
            });
        }
        self.inner.kill(reason).await
    }
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

fn bounded_recording_host(launcher: &KillRecordingLauncher) -> HostContext {
    HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .limits(Limits {
            kill_grace: Duration::from_millis(10),
            shutdown_timeout: Duration::from_millis(50),
            ..Limits::default()
        })
        .build()
        .expect("expected a host")
}

fn gated_host(launcher: &GatedLauncher) -> HostContext {
    HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .limits(Limits {
            kill_grace: Duration::from_millis(10),
            shutdown_timeout: Duration::from_millis(50),
            ..Limits::default()
        })
        .build()
        .expect("expected a host")
}

/// A launcher whose pipe refuses just `session/cancel`, after the prompt was accepted.
#[derive(Clone)]
struct CancelFailingLauncher {
    inner: FakeLauncher,
    attempts: Arc<std::sync::atomic::AtomicUsize>,
}

impl CancelFailingLauncher {
    fn new(inner: FakeLauncher) -> Self {
        Self {
            inner,
            attempts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    fn attempts(&self) -> usize {
        self.attempts.load(std::sync::atomic::Ordering::Acquire)
    }
}

#[async_trait::async_trait]
impl mango_external_agents::ProcessLauncher for CancelFailingLauncher {
    async fn spawn(
        &self,
        spec: mango_external_agents::LaunchSpec,
    ) -> mango_external_agents::Result<mango_external_agents::ManagedProcess> {
        let mut process = self.inner.spawn(spec).await?;
        if let Some(stdin) = process.stdin.take() {
            process.stdin = Some(Box::new(CancelFailingSink {
                inner: stdin,
                attempts: Arc::clone(&self.attempts),
            }));
        }
        Ok(process)
    }
}

struct CancelFailingSink {
    inner: Box<dyn mango_external_agents::ByteSink>,
    attempts: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl mango_external_agents::ByteSink for CancelFailingSink {
    async fn write_all(&mut self, bytes: &[u8]) -> mango_external_agents::Result<()> {
        if bytes
            .windows(b"session/cancel".len())
            .any(|window| window == b"session/cancel")
        {
            self.attempts
                .fetch_add(1, std::sync::atomic::Ordering::Release);
            return Err(Error::Link {
                peer: String::from("fake ACP agent"),
                message: String::from("the test pipe rejected session/cancel"),
            });
        }
        self.inner.write_all(bytes).await
    }

    async fn close(&mut self) -> mango_external_agents::Result<()> {
        self.inner.close().await
    }
}

fn cancel_failing_host(launcher: &CancelFailingLauncher) -> HostContext {
    HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .limits(Limits {
            kill_grace: Duration::from_millis(10),
            shutdown_timeout: Duration::from_millis(50),
            ..Limits::default()
        })
        .build()
        .expect("expected a host")
}

/// A prompt already submitted has an owned stream even if its subsequent cancel notification fails.
#[tokio::test]
async fn a_failed_cancel_after_prompt_acceptance_keeps_the_owned_terminal_stream() {
    let inner = FakeLauncher::new();
    inner.push(
        FakeAcpAgent::new()
            .asking_for_approval(Approval::Once)
            .process(),
    );
    let launcher = CancelFailingLauncher::new(inner.clone());
    let session = AcpHarness::new(profile())
        .open_session(&cancel_failing_host(&launcher), OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "one"))
        .await
        .expect("expected an accepted prompt stream");

    session
        .cancel(CancelReason::Requested)
        .await
        .expect("queueing a notification is asynchronous");
    let events = drain(&mut turn).await;
    assert!(
        matches!(events.last(), Some(EventKind::Completed)),
        "expected the accepted stream to own the cancellation outcome, received {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            EventKind::Cancelled {
                reason: CancelReason::Requested
            }
        )),
        "expected the host cancellation reason before the terminal, received {events:?}"
    );
    assert_eq!(
        launcher.attempts(),
        1,
        "expected one failed cancellation write"
    );
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected session cleanup");
    assert_eq!(
        inner.live_children(),
        0,
        "expected bounded cleanup after cancellation failure"
    );
}

/// Dropping the stream actively cancels the native turn, then reaps a peer that ignores cancellation.
#[tokio::test]
async fn a_dropped_stream_reaps_an_agent_that_ignores_native_cancellation() {
    let inner = FakeLauncher::new();
    inner.push(FakeAcpAgent::new().never_finishing_turns().process());
    let launcher = KillRecordingLauncher::new(inner.clone());
    let session = AcpHarness::new(profile())
        .open_session(
            &bounded_recording_host(&launcher),
            OpenSession::new("chat-1"),
        )
        .await
        .expect("expected a session");

    let turn = session
        .start_turn(TurnRequest::new("turn-1", "one"))
        .await
        .expect("expected a turn");
    drop(turn);

    tokio::time::timeout(Duration::from_millis(250), async {
        while inner.live_children() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected cancellation escalation to reap the agent");
    assert!(
        launcher.kills() >= 1,
        "expected the host process control to terminate the ignoring agent"
    );
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
                OpenSession::new("chat-1").with_configuration(at_level(PermissionLevel::ReadOnly)),
            )
            .await,
    );

    assert!(
        matches!(error.cause(), Error::Protocol { expected, .. } if expected.contains("the requested level")),
        "received {error:?}"
    );
    // The refusal names the relationship, not the profile's mode id.
    assert!(!error.to_string().contains("plan"), "received {error:?}");
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
        matches!(error.cause(), Error::AuthRequired { login_hint } if login_hint == "fake-acp login"),
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
    assert_eq!(
        capabilities["session"]["configOptions"]["boolean"],
        serde_json::json!({}),
        "expected the pinned ACP v1 boolean config-option capability"
    );
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
        matches!(error.cause(), Error::Protocol { expected, .. } if expected.contains("protocol version 1")),
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
                OpenSession::new("chat-1").with_configuration(
                    ConfigurationPatch::new()
                        .level(ConfigurationChange::Set(PermissionLevel::FullAccess))
                        .routing(ConfigurationChange::Set(ApprovalRouting::User)),
                ),
            )
            .await,
    );
    assert!(
        matches!(error.cause(), Error::HostConfiguration { .. }),
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

/// ACP returns the full live catalog after opening and after every `session/set_config_option`.
///
/// The session must preserve the agent's order and use the response to update both its catalog and
/// observed configuration. Before the configuration service existed, `configure` returned
/// `NotSupported` here even though the pinned v1 schema had this method.
#[tokio::test]
async fn a_live_acp_catalog_is_applied_between_turns_and_reports_current_values() {
    let agent = FakeAcpAgent::new().with_config_options(vec![
        serde_json::json!({
            "id": "model",
            "name": "Model",
            "category": "model",
            "type": "select",
            "currentValue": "small",
            "options": [
                { "value": "small", "name": "Small" },
                { "value": "large", "name": "Large" }
            ]
        }),
        serde_json::json!({
            "id": "thought",
            "name": "Thought level",
            "category": "thought_level",
            "type": "select",
            "currentValue": "low",
            "options": [
                { "value": "low", "name": "Low" },
                { "value": "high", "name": "High" }
            ]
        }),
        serde_json::json!({
            "id": "web-search",
            "name": "Web search",
            "type": "boolean",
            "currentValue": false
        }),
    ]);
    let launcher = FakeLauncher::new();
    launcher.push(agent.process());
    let session = AcpHarness::new(profile())
        .open_session(&host(&launcher), OpenSession::new("chat-configuration"))
        .await
        .expect("expected the fake session to open");

    assert_eq!(session.snapshot().catalog.options().len(), 3);
    assert_eq!(
        session.snapshot().configuration.observed.model.as_deref(),
        Some("small")
    );
    assert_eq!(
        session.snapshot().configuration.observed.effort.as_deref(),
        Some("low")
    );

    let outcome = session
        .configure(
            ConfigurationPatch::new()
                .model(ConfigurationChange::Set(String::from("large")))
                .effort(ConfigurationChange::Set(String::from("high")))
                .native(
                    ConfigurationOptionId::new("web-search"),
                    ConfigurationChange::Set(ConfigurationValue::Boolean(true)),
                ),
        )
        .await
        .expect("expected supported ACP options to be set between turns");

    assert!(outcome.is_complete());
    let state = session.snapshot();
    assert_eq!(state.configuration.accepted.model.as_deref(), Some("large"));
    assert_eq!(state.configuration.accepted.effort.as_deref(), Some("high"));
    assert_eq!(
        state
            .configuration
            .accepted
            .native
            .get(&ConfigurationOptionId::new("web-search")),
        Some(&ConfigurationValue::Boolean(true))
    );
    assert_eq!(
        state.configuration.observed.model.as_deref(),
        Some("large"),
        "expected the response catalog to report the final model, sent {:?}",
        launcher.written()
    );
    assert_eq!(state.configuration.observed.effort.as_deref(), Some("high"));
    assert_eq!(
        state
            .configuration
            .observed
            .native
            .get(&ConfigurationOptionId::new("web-search")),
        Some(&ConfigurationValue::Boolean(true))
    );
}

/// A newer `config_option_update` wins over the stale response that follows it on the same request.
#[tokio::test]
async fn a_config_notification_is_not_overwritten_by_a_stale_option_response() {
    let launcher = FakeLauncher::new();
    launcher.push(InterleavingConfigAgent::process());
    let session = AcpHarness::new(profile())
        .open_session(&host(&launcher), OpenSession::new("catalog-order"))
        .await
        .expect("expected the interleaving fake session to open");
    session
        .configure(ConfigurationPatch::new().model(ConfigurationChange::Set(String::from("large"))))
        .await
        .expect("expected the stale response itself to be accepted");
    let snapshot = session.snapshot();
    assert_eq!(
        snapshot.configuration.observed.model.as_deref(),
        Some("newer"),
        "expected the notification published before the response to remain authoritative"
    );
    assert_eq!(
        snapshot.catalog.options()[0].current,
        Some(ConfigurationValue::Text(String::from("newer"))),
        "expected the public catalog not to regress to the response value"
    );
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected cleanup");
}

/// A notification received while `session/new` is pending remains the authoritative catalog.
#[tokio::test]
async fn opening_a_new_session_keeps_a_newer_catalog_notification() {
    let launcher = FakeLauncher::new();
    launcher.push(OpeningCatalogInterleavingAgent::for_new());
    let session = AcpHarness::new(profile())
        .open_session(&host(&launcher), OpenSession::new("new-catalog-order"))
        .await
        .expect("expected the interleaving session to open");
    assert_eq!(
        session.snapshot().configuration.observed.model.as_deref(),
        Some("newer"),
        "expected the notification during session/new to remain authoritative"
    );
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected cleanup");
}

/// A notification received while `session/load` is pending remains the authoritative catalog.
#[tokio::test]
async fn opening_a_loaded_session_keeps_a_newer_catalog_notification() {
    let launcher = FakeLauncher::new();
    launcher.push(OpeningCatalogInterleavingAgent::for_load());
    let session = AcpHarness::new(profile())
        .open_session(
            &host(&launcher),
            OpenSession::new("load-catalog-order").resuming("resumed-session", ResumeMode::Strict),
        )
        .await
        .expect("expected the interleaving session to load");
    assert_eq!(
        session.snapshot().configuration.observed.model.as_deref(),
        Some("newer"),
        "expected the notification during session/load to remain authoritative"
    );
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected cleanup");
}

/// A configuration request begun after close is refused before ACP receives an option change.
#[tokio::test]
async fn a_configuration_request_after_close_is_not_submitted() {
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeAcpAgent::new()
            .with_config_options(vec![InterleavingConfigAgent::model_option("small")])
            .process(),
    );
    let session = AcpHarness::new(profile())
        .open_session(&host(&launcher), OpenSession::new("closed-config"))
        .await
        .expect("expected the fake session to open");
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected close");
    let writes_before = launcher.written();
    let error = session
        .configure(ConfigurationPatch::new().model(ConfigurationChange::Set(String::from("large"))))
        .await
        .expect_err("expected the closed session to refuse configuration");
    assert!(matches!(error, Error::Closed { subject: "session" }));
    assert_eq!(
        launcher.written(),
        writes_before,
        "expected no set-config-option request after close"
    );
}

/// A response released after close claims the session cannot publish a new accepted setting.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn closing_during_a_held_config_response_refuses_the_late_acceptance() {
    let agent = HeldSetOptionAgent::new();
    let launcher = FakeLauncher::new();
    launcher.push(agent.clone().process());
    let opened = AcpHarness::new(profile())
        .open_session(&host(&launcher), OpenSession::new("held-config"))
        .await
        .expect("expected a session");
    let session: Arc<dyn Session> = Arc::from(opened);
    let accepted_before = session.snapshot().configuration.accepted.clone();
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
    tokio::time::timeout(Duration::from_secs(2), async {
        while !agent.entered.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected the agent to hold a set-option response");
    let closing = tokio::spawn({
        let session = Arc::clone(&session);
        async move { session.close(CloseReason::Requested).await }
    });
    let claimed = tokio::time::timeout(Duration::from_secs(2), async {
        while !matches!(
            session.snapshot().status,
            SessionStatus::Closing | SessionStatus::Closed
        ) {
            tokio::task::yield_now().await;
        }
    })
    .await;
    agent.release();
    claimed.expect("expected close to claim the session without waiting for configuration");
    let error = configuring
        .await
        .expect("expected the configuration task")
        .expect_err("expected close to refuse the late configuration acceptance");
    assert!(
        matches!(error.cause(), Error::Closed { subject: "session" }),
        "expected a typed close refusal, received {error:?}"
    );
    closing
        .await
        .expect("expected close task")
        .expect("expected close");
    assert_eq!(
        session.snapshot().configuration.accepted,
        accepted_before,
        "expected no accepted setting to publish after close claimed the session"
    );
}

/// Accepted ACP defaults survive a later prompt whose request leaves configuration at `keep`.
#[tokio::test]
async fn a_turn_keeps_the_last_accepted_model_effort_and_native_settings() {
    let agent = FakeAcpAgent::new().with_config_options(vec![
        serde_json::json!({
            "id": "model", "name": "Model", "category": "model", "type": "select",
            "currentValue": "small", "options": [
                { "value": "small", "name": "Small" }, { "value": "large", "name": "Large" }
            ]
        }),
        serde_json::json!({
            "id": "thought", "name": "Thought", "category": "thought_level", "type": "select",
            "currentValue": "low", "options": [
                { "value": "low", "name": "Low" }, { "value": "high", "name": "High" }
            ]
        }),
        serde_json::json!({ "id": "web-search", "name": "Web", "type": "boolean", "currentValue": false }),
    ]);
    let (session, _) = open(agent, ConfigurationPatch::new()).await;
    session
        .configure(
            ConfigurationPatch::new()
                .model(ConfigurationChange::Set(String::from("large")))
                .effort(ConfigurationChange::Set(String::from("high")))
                .native(
                    ConfigurationOptionId::new("web-search"),
                    ConfigurationChange::Set(ConfigurationValue::Boolean(true)),
                ),
        )
        .await
        .expect("expected all three settings to be accepted");
    let mut turn = session
        .start_turn(TurnRequest::new("inherit-settings", "continue"))
        .await
        .expect("expected a turn without configuration overrides");
    let _ = drain(&mut turn).await;
    let accepted = &session.snapshot().configuration.accepted;
    assert_eq!(accepted.model.as_deref(), Some("large"));
    assert_eq!(accepted.effort.as_deref(), Some("high"));
    assert_eq!(
        accepted
            .native
            .get(&ConfigurationOptionId::new("web-search")),
        Some(&ConfigurationValue::Boolean(true)),
        "expected the last vendor-confirmed native setting to persist"
    );
}

/// A later explicit vendor refusal reports the preceding confirmed settings as a partial outcome.
#[tokio::test]
async fn a_later_config_option_refusal_publishes_the_confirmed_partial_state() {
    let agent = FakeAcpAgent::new()
        .with_config_options(vec![
            serde_json::json!({
                "id": "model", "name": "Model", "category": "model", "type": "select",
                "currentValue": "small", "options": [
                    { "value": "small", "name": "Small" }, { "value": "large", "name": "Large" }
                ]
            }),
            serde_json::json!({ "id": "web-search", "name": "Web", "type": "boolean", "currentValue": false }),
        ])
        .refusing_config_option("web-search", -32001, "option rejected");
    let (session, _) = open(agent, ConfigurationPatch::new()).await;
    let outcome = session
        .configure(
            ConfigurationPatch::new()
                .model(ConfigurationChange::Set(String::from("large")))
                .native(
                    ConfigurationOptionId::new("web-search"),
                    ConfigurationChange::Set(ConfigurationValue::Boolean(true)),
                ),
        )
        .await
        .expect("expected a vendor refusal to produce a partial configuration outcome");
    assert!(
        outcome.is_partial(),
        "expected partial outcome, received {outcome:?}"
    );
    assert_eq!(
        outcome.rollback,
        mango_external_agents::Rollback::NotAttempted
    );
    assert_eq!(
        session.snapshot().configuration.accepted.model.as_deref(),
        Some("large"),
        "expected the preceding response-confirmed model to remain visible"
    );
    assert_eq!(
        session
            .snapshot()
            .configuration
            .accepted
            .native
            .get(&ConfigurationOptionId::new("web-search")),
        None,
        "expected the refused native setting not to be reported as accepted"
    );
}

/// A legacy mode request can fail after an option request succeeded; the prior write remains real.
#[tokio::test]
async fn a_later_mode_refusal_publishes_the_confirmed_partial_state() {
    let agent = FakeAcpAgent::new()
        .with_modes(["plan"])
        .with_config_options(vec![serde_json::json!({
            "id": "model", "name": "Model", "category": "model", "type": "select",
            "currentValue": "small", "options": [
                { "value": "small", "name": "Small" }, { "value": "large", "name": "Large" }
            ]
        })])
        .refusing_set_mode(-32001, "mode rejected");
    let launcher = FakeLauncher::new();
    launcher.push(agent.process());
    let profile = Arc::new(
        AcpProfile::custom("fake", ["fake-acp", "acp"], VENDOR).with_modes(SessionModeIds {
            read_only: Some("plan"),
            ..SessionModeIds::UNKNOWN
        }),
    );
    let session = AcpHarness::new(profile)
        .open_session(&host(&launcher), OpenSession::new("chat-configuration"))
        .await
        .expect("expected the fake session to open");

    let outcome = session
        .configure(
            ConfigurationPatch::new()
                .model(ConfigurationChange::Set(String::from("large")))
                .level(ConfigurationChange::Set(PermissionLevel::ReadOnly)),
        )
        .await;
    assert_eq!(
        session.snapshot().configuration.accepted.model.as_deref(),
        Some("large"),
        "expected the response-confirmed model to survive the later mode refusal"
    );
    assert_eq!(session.snapshot().configuration.accepted.level, None);
    assert!(
        matches!(outcome, Ok(ref outcome) if outcome.is_partial()),
        "expected a partial outcome for the refused mode, received {outcome:?}"
    );
}

/// A profile without a full-access mode cannot claim that an ACP configuration applied it.
#[tokio::test]
async fn configuration_refuses_full_access_when_the_profile_has_no_matching_mode() {
    let (session, _) = open(FakeAcpAgent::new(), ConfigurationPatch::new()).await;
    let outcome = session
        .configure(
            at_level(PermissionLevel::FullAccess)
                .routing(ConfigurationChange::Set(ApprovalRouting::User)),
        )
        .await
        .expect("expected a typed partial configuration outcome");
    assert!(
        !outcome.is_complete(),
        "expected full access to be rejected"
    );
    assert_eq!(session.snapshot().configuration.accepted.level, None);
    assert_eq!(
        session.snapshot().configuration.accepted.routing,
        None,
        "expected the paired routing not to be claimed after the level rejection"
    );
}

/// Opening establishes a profile-mapped mode once; the catalog configuration pass must not repeat
/// the same `session/set_mode` request after that mode already succeeded.
#[tokio::test]
async fn opening_a_permission_level_applies_its_profile_mode_once() {
    let launcher = FakeLauncher::new();
    launcher.push(FakeAcpAgent::new().with_modes(["plan"]).process());
    let profile = Arc::new(
        AcpProfile::custom("fake", ["fake-acp", "acp"], VENDOR).with_modes(SessionModeIds {
            read_only: Some("plan"),
            ..SessionModeIds::UNKNOWN
        }),
    );

    let session = AcpHarness::new(profile)
        .open_session(
            &host(&launcher),
            OpenSession::new("one-mode").with_configuration(at_level(PermissionLevel::ReadOnly)),
        )
        .await
        .expect("expected the read-only session to open");

    assert_eq!(
        launcher
            .written()
            .iter()
            .filter(|line| line.contains("\"session/set_mode\""))
            .count(),
        1,
        "expected one profile mode request while opening"
    );
    assert_eq!(
        session.snapshot().configuration.accepted.level,
        Some(PermissionLevel::ReadOnly),
        "expected the single mode request to establish the accepted level"
    );
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected cleanup");
}

/// ACP config rows labelled `mode` can bypass the profile's permission mapping if they are treated
/// as ordinary native options. The host must retain the read-only level it established through the
/// profile instead of sending a raw switch to the agent's execute mode.
#[tokio::test]
async fn a_native_mode_write_cannot_bypass_the_profiles_permission_matrix() {
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeAcpAgent::new()
            .with_modes(["plan", "code"])
            .with_config_options(vec![serde_json::json!({
                "id": "mode", "name": "Mode", "category": "mode", "type": "select",
                "currentValue": "plan", "options": [
                    { "value": "plan", "name": "Plan" },
                    { "value": "code", "name": "Code" }
                ]
            })])
            .process(),
    );
    let profile = Arc::new(
        AcpProfile::custom("fake", ["fake-acp", "acp"], VENDOR).with_modes(SessionModeIds {
            full_access: Some("code"),
            read_only: Some("plan"),
            ..SessionModeIds::UNKNOWN
        }),
    );
    let session = AcpHarness::new(profile)
        .open_session(
            &host(&launcher),
            OpenSession::new("native-mode").with_configuration(at_level(PermissionLevel::ReadOnly)),
        )
        .await
        .expect("expected the read-only session to open");
    let writes_before = launcher.written();

    let outcome = session
        .configure(ConfigurationPatch::new().native(
            ConfigurationOptionId::new("mode"),
            ConfigurationChange::Set(ConfigurationValue::Text(String::from("code"))),
        ))
        .await
        .expect("expected a typed configuration outcome");

    assert!(
        !outcome.is_complete(),
        "expected the raw mode selector to be rejected, received {outcome:?}"
    );
    assert_eq!(
        session.snapshot().configuration.accepted.level,
        Some(PermissionLevel::ReadOnly),
        "expected the established read-only level to remain authoritative"
    );
    assert_eq!(
        launcher.written(),
        writes_before,
        "expected no raw mode config request to reach the agent"
    );
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected cleanup");
}

/// A patch cannot address one ACP semantic row through both its neutral axis and native id: the
/// two values would otherwise leave the accepted model or effort disagreeing with the agent.
#[tokio::test]
async fn semantic_axes_and_native_ids_cannot_target_the_same_acp_option() {
    let agent = FakeAcpAgent::new().with_config_options(vec![
        serde_json::json!({
            "id": "model", "name": "Model", "category": "model", "type": "select",
            "currentValue": "small", "options": [
                { "value": "small", "name": "Small" }, { "value": "large", "name": "Large" }
            ]
        }),
        serde_json::json!({
            "id": "thought", "name": "Thought", "category": "thought_level", "type": "select",
            "currentValue": "low", "options": [
                { "value": "low", "name": "Low" }, { "value": "high", "name": "High" }
            ]
        }),
    ]);
    let (session, launcher) = open(agent, ConfigurationPatch::new()).await;
    let writes_before = launcher.written();

    let outcome = session
        .configure(
            ConfigurationPatch::new()
                .model(ConfigurationChange::Set(String::from("large")))
                .effort(ConfigurationChange::Set(String::from("high")))
                .native(
                    ConfigurationOptionId::new("model"),
                    ConfigurationChange::Set(ConfigurationValue::Text(String::from("small"))),
                )
                .native(
                    ConfigurationOptionId::new("thought"),
                    ConfigurationChange::Set(ConfigurationValue::Text(String::from("low"))),
                ),
        )
        .await
        .expect("expected a typed configuration outcome");

    assert!(
        !outcome.is_complete(),
        "expected conflicting targets to be refused, received {outcome:?}"
    );
    assert_eq!(
        launcher.written(),
        writes_before,
        "expected no conflicted config write to reach the agent"
    );
    let accepted = &session.snapshot().configuration.accepted;
    assert_eq!(
        accepted.model, None,
        "expected no model to be claimed accepted"
    );
    assert_eq!(
        accepted.effort, None,
        "expected no effort to be claimed accepted"
    );
    assert!(
        accepted.native.is_empty(),
        "expected no semantic id to be claimed as native acceptance"
    );
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected cleanup");
}

/// Refusing native writes to semantic rows on every call prevents an earlier native request from
/// leaving the neutral accepted model or effort axis stale on a later configuration call.
#[tokio::test]
async fn native_semantic_options_are_refused_across_calls_before_acceptance_drift() {
    let agent = FakeAcpAgent::new().with_config_options(vec![
        serde_json::json!({
            "id": "model", "name": "Model", "category": "model", "type": "select",
            "currentValue": "small", "options": [
                { "value": "small", "name": "Small" }, { "value": "large", "name": "Large" }
            ]
        }),
        serde_json::json!({
            "id": "thought", "name": "Thought", "category": "thought_level", "type": "select",
            "currentValue": "low", "options": [
                { "value": "low", "name": "Low" }, { "value": "high", "name": "High" }
            ]
        }),
    ]);
    let (session, launcher) = open(agent, ConfigurationPatch::new()).await;
    let writes_before = launcher.written();

    for (id, value) in [("model", "large"), ("thought", "high")] {
        let outcome = session
            .configure(ConfigurationPatch::new().native(
                ConfigurationOptionId::new(id),
                ConfigurationChange::Set(ConfigurationValue::Text(String::from(value))),
            ))
            .await
            .expect("expected a typed semantic-option refusal");
        assert!(
            !outcome.is_complete(),
            "expected {id} to be rejected as a semantic axis, received {outcome:?}"
        );
    }

    assert_eq!(
        launcher.written(),
        writes_before,
        "expected no semantic native config request to reach the agent"
    );
    let accepted = &session.snapshot().configuration.accepted;
    assert_eq!(accepted.model, None, "expected no stale model acceptance");
    assert_eq!(accepted.effort, None, "expected no stale effort acceptance");
    assert!(
        accepted.native.is_empty(),
        "expected no semantic option to become a native accepted setting"
    );
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected cleanup");
}

/// A successful model update can replace the catalog before the next requested native setting is
/// considered. A row that becomes `mode` in that response must be refused before its raw write.
#[tokio::test]
async fn a_catalog_refresh_cannot_make_a_native_mode_write_permissible() {
    let launcher = FakeLauncher::new();
    launcher.push(ReclassifyingCatalogAgent::process());
    let session = AcpHarness::new(profile())
        .open_session(&host(&launcher), OpenSession::new("reclassified-mode"))
        .await
        .expect("expected a session");

    let outcome = session
        .configure(
            ConfigurationPatch::new()
                .model(ConfigurationChange::Set(String::from("large")))
                .native(
                    ConfigurationOptionId::new("mode"),
                    ConfigurationChange::Set(ConfigurationValue::Text(String::from("code"))),
                ),
        )
        .await
        .expect("expected a partial configuration outcome");

    assert!(
        outcome.is_partial(),
        "expected the refreshed mode row to be refused, received {outcome:?}"
    );
    assert_eq!(
        session.snapshot().configuration.accepted.model.as_deref(),
        Some("large"),
        "expected the model response confirmed before the native refusal to remain accepted"
    );
    let writes = launcher.written();
    assert_eq!(
        writes
            .iter()
            .filter(|line| line.contains("\"session/set_config_option\""))
            .count(),
        1,
        "expected only the confirmed model write, received {writes:?}"
    );
    assert!(
        writes
            .iter()
            .all(|line| !line.contains("\"configId\":\"mode\"")),
        "expected no raw mode write after catalog refresh, received {writes:?}"
    );
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected cleanup");
}

/// A mode row is discovered only after `session/new`, but it must still be rejected before the
/// opening path can send a raw `session/set_config_option` that changes permissions outside the
/// profile matrix.
#[tokio::test]
async fn opening_refuses_a_native_mode_write_before_submitting_it() {
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeAcpAgent::new()
            .with_config_options(vec![serde_json::json!({
                "id": "mode", "name": "Mode", "category": "mode", "type": "select",
                "currentValue": "plan", "options": [
                    { "value": "plan", "name": "Plan" },
                    { "value": "code", "name": "Code" }
                ]
            })])
            .process(),
    );

    let result = AcpHarness::new(profile())
        .open_session(
            &host(&launcher),
            OpenSession::new("opening-native-mode").with_configuration(
                ConfigurationPatch::new().native(
                    ConfigurationOptionId::new("mode"),
                    ConfigurationChange::Set(ConfigurationValue::Text(String::from("code"))),
                ),
            ),
        )
        .await;
    let Err(error) = result else {
        panic!("expected the un-mapped mode selector to be refused");
    };

    assert!(
        matches!(error.cause(), Error::HostConfiguration { .. }),
        "received {error:?}"
    );
    assert!(
        launcher
            .written()
            .iter()
            .all(|line| !line.contains("session/set_config_option")),
        "expected the opening refusal before a raw mode write, sent {:?}",
        launcher.written()
    );
    assert_eq!(launcher.live_children(), 0, "expected opening cleanup");
}

/// ACP can replace its complete configuration catalog while a session is live.
///
/// The update is session state, so it must reach the snapshot without becoming turn transcript.
#[tokio::test]
async fn a_config_option_update_replaces_the_live_catalog_and_observed_values() {
    let initial = serde_json::json!({
        "id": "model",
        "name": "Model",
        "category": "model",
        "type": "select",
        "currentValue": "small",
        "options": [{ "value": "small", "name": "Small" }]
    });
    let updated = serde_json::json!({
        "id": "model",
        "name": "Model",
        "category": "model",
        "type": "select",
        "currentValue": "large",
        "options": [{ "value": "large", "name": "Large" }]
    });
    let agent = FakeAcpAgent::new()
        .with_config_options(vec![initial])
        .with_updates(vec![serde_json::json!({
            "sessionUpdate": "config_option_update",
            "configOptions": [updated]
        })]);
    let (session, _) = open(agent, ConfigurationPatch::new()).await;

    let mut turn = session
        .start_turn(TurnRequest::new("catalog-update", "hello"))
        .await
        .expect("expected a turn");
    let events = drain(&mut turn).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, EventKind::TextDelta { .. })),
        "expected the catalog update to stay out of the transcript, received {events:?}"
    );
    let snapshot = session.snapshot();
    assert_eq!(snapshot.catalog.options().len(), 1);
    assert_eq!(
        snapshot.configuration.observed.model.as_deref(),
        Some("large"),
        "expected the replacement catalog to be visible"
    );
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected close");
}

/// Option rows cross from an external agent into a host snapshot through the catalog normalizer.
#[tokio::test]
async fn catalog_rows_are_bounded_on_open_and_on_config_option_updates() {
    let oversized = format!("unsafe\u{1b}{}", "x".repeat(400));
    let initial = serde_json::json!({
        "id": "\u{0}", "name": oversized, "type": "boolean", "currentValue": false
    });
    let updated = serde_json::json!({
        "id": "safe", "name": oversized, "type": "boolean", "currentValue": true
    });
    let agent = FakeAcpAgent::new()
        .with_config_options(vec![initial])
        .with_updates(vec![serde_json::json!({
            "sessionUpdate": "config_option_update", "configOptions": [updated]
        })]);
    let (session, _) = open(agent, ConfigurationPatch::new()).await;
    assert!(
        session.snapshot().catalog.options().is_empty(),
        "expected the malformed initial id to be dropped"
    );
    let mut turn = session
        .start_turn(TurnRequest::new("catalog-normalization", "hello"))
        .await
        .expect("expected a turn");
    let _ = drain(&mut turn).await;
    let snapshot = session.snapshot();
    let option = &snapshot.catalog.options()[0];
    let name = option.name.as_deref().expect("expected a normalized name");
    assert!(
        name.chars().count() <= 256,
        "expected bounded name, received {name:?}"
    );
    assert!(
        !name.contains('\u{1b}'),
        "expected control text removed, received {name:?}"
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

#[path = "session/contracts.rs"]
mod contracts;
