//! The harness driven against transcripts a real `codex app-server` produced.
//!
//! Nothing here spawns anything: `mea capture` recorded the conversations, and a fake child
//! replays them. That is the only way to test a dialect on a machine where the vendor's CLI is not
//! installed — which is every machine, in CI.

#[path = "replay/contracts.rs"]
mod contracts;
mod support;

use std::sync::Arc;
use std::time::SystemTime;

use mango_agent_codex::CodexHarness;
use mango_external_agents::event::EventKind;
use mango_external_agents::permission::{
    BrokerDecision, DecisionSource, PermissionBroker, PermissionEffect, PermissionRequest,
    PermissionResponse,
};
use mango_external_agents::testing::{FakeLauncher, FakeProcess};
use mango_external_agents::{
    ApprovalRouting, CancelReason, Clock, CloseReason, ConfigurationChange, ConfigurationPatch,
    EnvSource, Harness, HostContext, OpenSession, PermissionLevel, Session, SessionQuery,
    SessionStatus, SessionSubscription, Steer, TurnRequest,
};
use support::Transcript;

/// A host clock fixed at the instant a paused-time approval test begins.
struct FixedClock(SystemTime);

impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        self.0
    }
}

fn approval_timeout() -> std::time::Duration {
    replay_limits().approval_timeout
}

/// A host clock that steps backward by a fixed drift on every read after its first.
///
/// Proves a caller reused one `now()` rather than reading twice: two reads taken back to back
/// land on different instants, exactly what a host clock correction looks like mid-request.
struct RewindingClock {
    calls: std::sync::atomic::AtomicU32,
    start: SystemTime,
    drift: std::time::Duration,
}

impl RewindingClock {
    fn new(start: SystemTime, drift: std::time::Duration) -> Self {
        Self {
            calls: std::sync::atomic::AtomicU32::new(0),
            start,
            drift,
        }
    }
}

impl Clock for RewindingClock {
    fn now(&self) -> SystemTime {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.start - self.drift * call
    }
}

/// A broker that answers every question the same way, and remembers what it was asked.
///
/// Named rather than an inline closure so a test reads as "this host had a policy" instead of as
/// a lambda, and so what the policy saw can be asserted on afterwards.
struct FixedBroker {
    decision: BrokerDecision,
    seen: std::sync::Mutex<Vec<PermissionRequest>>,
}

impl FixedBroker {
    fn new(decision: BrokerDecision) -> Arc<Self> {
        Arc::new(Self {
            decision,
            seen: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn seen(&self) -> Vec<PermissionRequest> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait::async_trait]
impl PermissionBroker for FixedBroker {
    async fn decide(&self, request: &PermissionRequest) -> BrokerDecision {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());
        self.decision.clone()
    }
}

/// A broker that has not made up its mind until the test says so.
///
/// A real policy that calls out to a service takes time to answer, and the host is reading events
/// the whole while. Named rather than inlined so the test reads as "this host had a slow policy",
/// which is the only thing about it that matters.
struct SlowBroker {
    release: tokio::sync::Semaphore,
}

impl SlowBroker {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            release: tokio::sync::Semaphore::new(0),
        })
    }

    fn make_up_its_mind(&self) {
        self.release.add_permits(1);
    }
}

#[async_trait::async_trait]
impl PermissionBroker for SlowBroker {
    async fn decide(&self, _request: &PermissionRequest) -> BrokerDecision {
        let _permit = self.release.acquire().await;
        BrokerDecision::Ask
    }
}

/// An app-server that withholds its first `turn/start` answer until another call arrives.
///
/// The real peer can notify that a turn ended before answering the call that started it. Holding
/// that answer lets the tests put another host turn in the slot and then deliver the old success
/// or error, which is the ordering the session has to survive.
struct DelayedFirstStartServer {
    answer: DelayedStartAnswer,
    complete_before_answer: bool,
    thread_id: String,
    first_request_id: std::sync::Mutex<Option<serde_json::Value>>,
    starts: std::sync::atomic::AtomicUsize,
}

#[derive(Clone, Copy)]
enum DelayedStartAnswer {
    Success,
    Error,
}

impl DelayedFirstStartServer {
    fn respond(&self, frame: &serde_json::Value) -> Option<Vec<String>> {
        let method = frame.get("method").and_then(serde_json::Value::as_str)?;
        let id = frame.get("id").cloned().unwrap_or(serde_json::Value::Null);
        match method {
            "turn/start" => {
                let start = self
                    .starts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if start == 0 {
                    *self
                        .first_request_id
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(id);
                    return Some(if self.complete_before_answer {
                        vec![
                            serde_json::json!({
                                "method": "turn/completed",
                                "params": {
                                    "threadId": self.thread_id,
                                    "turn": {"id": "vendor-turn-1", "status": "completed"},
                                },
                            })
                            .to_string(),
                        ]
                    } else {
                        Vec::new()
                    });
                }
                Some(vec![
                    serde_json::json!({
                        "id": id,
                        "result": {"turn": {"id": format!("vendor-turn-{}", start + 1)}},
                    })
                    .to_string(),
                ])
            }
            "account/rateLimits/read" => {
                let first_id = self
                    .first_request_id
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                    .expect("expected the held turn/start request");
                let delayed = match self.answer {
                    DelayedStartAnswer::Success => serde_json::json!({
                        "id": first_id,
                        "result": {"turn": {"id": "vendor-turn-1"}},
                    }),
                    DelayedStartAnswer::Error => serde_json::json!({
                        "id": first_id,
                        "error": {"code": -32602, "message": "delayed start refusal"},
                    }),
                };
                Some(vec![
                    delayed.to_string(),
                    serde_json::json!({"id": id, "result": {"rateLimits": null}}).to_string(),
                ])
            }
            "turn/interrupt" => Some(vec![
                serde_json::json!({"id": id, "result": {}}).to_string(),
                serde_json::json!({
                    "method": "turn/completed",
                    "params": {
                        "threadId": self.thread_id,
                        "turn": {
                            "id": frame.pointer("/params/turnId").cloned()
                                .unwrap_or(serde_json::Value::Null),
                            "status": "interrupted",
                        },
                    },
                })
                .to_string(),
            ]),
            _ => None,
        }
    }
}

async fn open_with_delayed_first_start(
    answer: DelayedStartAnswer,
    complete_before_answer: bool,
) -> (Arc<dyn Session>, Arc<FakeLauncher>) {
    let transcript = Transcript::load("turn");
    let server = Arc::new(DelayedFirstStartServer {
        answer,
        complete_before_answer,
        thread_id: transcript
            .thread_id()
            .expect("expected the recording to name its thread"),
        first_request_id: std::sync::Mutex::new(None),
        starts: std::sync::atomic::AtomicUsize::new(0),
    });
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(transcript.as_process_intercepting(move |frame| server.respond(frame)));
    let (host, launcher) = with_launcher(launcher, None);
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    (Arc::from(session), launcher)
}

#[derive(Clone, Copy)]
enum InterruptRace {
    CompletesOwner,
    FailsWhileOwnerRemainsActive,
}

/// Replays an approval whose refusal races cancellation's in-flight interrupt.
///
/// The captured approval transcript continues after refusal, so this small server-side variation
/// makes the ordering under test explicit without inventing a fixture as if it were a capture.
async fn open_with_approval_cancellation_race(
    interrupt_race: InterruptRace,
) -> (Box<dyn Session>, Arc<FakeLauncher>) {
    let transcript = Transcript::load("approval");
    let thread_id = transcript
        .thread_id()
        .expect("expected the approval recording to name its thread");
    let turn_id = transcript
        .every_received()
        .into_iter()
        .find_map(|frame| {
            frame
                .pointer("/params/turn/id")
                .and_then(serde_json::Value::as_str)
        })
        .map(str::to_owned)
        .expect("expected the approval recording to name its turn");
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(transcript.as_process_intercepting(move |frame| {
        let method = frame.get("method").and_then(serde_json::Value::as_str);
        if method.is_none() {
            return Some(Vec::new());
        }
        if method == Some("turn/interrupt") {
            let error = serde_json::json!({
                "id": frame.get("id").cloned().unwrap_or(serde_json::Value::Null),
                "error": {"code": -32603, "message": "interrupted owner was already closed"},
            })
            .to_string();
            return Some(match interrupt_race {
                InterruptRace::CompletesOwner => vec![
                    serde_json::json!({
                        "method": "turn/completed",
                        "params": {
                            "threadId": thread_id,
                            "turn": {"id": turn_id, "status": "completed"},
                        },
                    })
                    .to_string(),
                    error,
                ],
                InterruptRace::FailsWhileOwnerRemainsActive => vec![error],
            });
        }
        None
    }));
    let (host, launcher) = with_launcher(launcher, None);
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    (session, launcher)
}

async fn wait_for_turn_start(launcher: &FakeLauncher) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !launcher
            .written()
            .iter()
            .any(|line| line.contains("\"turn/start\""))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected the held turn/start to reach the fake server");
}

/// A host whose launcher replays these fixtures, one child per named scenario.
fn host_replaying(scenarios: &[&str]) -> (HostContext, Arc<FakeLauncher>) {
    host_with(scenarios, None)
}

fn host_with(
    scenarios: &[&str],
    broker: Option<Arc<dyn PermissionBroker>>,
) -> (HostContext, Arc<FakeLauncher>) {
    let launcher = Arc::new(FakeLauncher::new());
    for scenario in scenarios {
        launcher.push(Transcript::load(scenario).as_process());
    }
    with_launcher(launcher, broker)
}

/// A patch that sets only the permission level.
fn level_patch(level: PermissionLevel) -> ConfigurationPatch {
    ConfigurationPatch::new().level(ConfigurationChange::Set(level))
}

/// A patch that sets both the permission level and who answers its prompts.
fn permission_patch(level: PermissionLevel, routing: ApprovalRouting) -> ConfigurationPatch {
    ConfigurationPatch::new()
        .level(ConfigurationChange::Set(level))
        .routing(ConfigurationChange::Set(routing))
}

/// What `codex --version` prints, for a test whose harness probes before it opens anything.
fn version_answer() -> FakeProcess {
    FakeProcess::transcript([format!(
        "codex-cli {}",
        mango_agent_codex::MINIMUM_CODEX_VERSION
    )])
}

fn with_launcher(
    launcher: Arc<FakeLauncher>,
    broker: Option<Arc<dyn PermissionBroker>>,
) -> (HostContext, Arc<FakeLauncher>) {
    with_launcher_limits(launcher, broker, replay_limits())
}

fn with_launcher_and_clock(
    launcher: Arc<FakeLauncher>,
    broker: Option<Arc<dyn PermissionBroker>>,
    clock: Arc<dyn Clock>,
) -> (HostContext, Arc<FakeLauncher>) {
    with_launcher_limits_and_cancel_and_clock(
        launcher,
        broker,
        replay_limits(),
        mango_external_agents::CancelToken::new(),
        Some(clock),
    )
}

fn replay_limits() -> mango_external_agents::Limits {
    mango_external_agents::Limits {
        // A call the replay has no answer for is a bug in the fixture or in the harness, and the
        // default two minutes would report it as a test that hangs rather than one that fails.
        request_timeout: std::time::Duration::from_secs(5),
        // Deliberately long, and not shortened the way `request_timeout` was. Every test that
        // exercises this deadline runs under `start_paused`, where `tokio::time::advance` reaches
        // it instantly whatever it is; every other test in this file runs on the real clock and
        // answers its approval from host code. A short value there is a wall-clock budget between
        // `await_approval` and `respond`, which a loaded runner spends before the answer lands —
        // the harness then declines on the host's behalf and the decision arrives as `Expired`.
        approval_timeout: std::time::Duration::from_secs(30 * 60),
        ..mango_external_agents::Limits::default()
    }
}

fn with_launcher_limits(
    launcher: Arc<FakeLauncher>,
    broker: Option<Arc<dyn PermissionBroker>>,
    limits: mango_external_agents::Limits,
) -> (HostContext, Arc<FakeLauncher>) {
    with_launcher_limits_and_cancel(
        launcher,
        broker,
        limits,
        mango_external_agents::CancelToken::new(),
    )
}

fn with_launcher_limits_and_cancel(
    launcher: Arc<FakeLauncher>,
    broker: Option<Arc<dyn PermissionBroker>>,
    limits: mango_external_agents::Limits,
    cancel: mango_external_agents::CancelToken,
) -> (HostContext, Arc<FakeLauncher>) {
    with_launcher_limits_and_cancel_and_clock(launcher, broker, limits, cancel, None)
}

fn with_launcher_limits_and_cancel_and_clock(
    launcher: Arc<FakeLauncher>,
    broker: Option<Arc<dyn PermissionBroker>>,
    limits: mango_external_agents::Limits,
    cancel: mango_external_agents::CancelToken,
    clock: Option<Arc<dyn Clock>>,
) -> (HostContext, Arc<FakeLauncher>) {
    let mut builder = HostContext::builder()
        .launcher(launcher.clone())
        .cwd("/workspace")
        .client_info("mango-test", "0.0.1")
        .environment(EnvSource::from_pairs([
            ("PATH", "/usr/bin"),
            ("CODEX_HOME", "/home/user/.codex"),
            ("CONNECTOR_SECRET", "never-forward-this"),
        ]))
        .limits(limits)
        .cancel(cancel);
    if let Some(clock) = clock {
        builder = builder.clock(clock);
    }
    if let Some(broker) = broker {
        builder = builder.broker(broker);
    }
    (builder.build().expect("expected a host"), launcher)
}

async fn open(scenario: &str) -> (Box<dyn Session>, Arc<FakeLauncher>) {
    let (host, launcher) = host_replaying(&[scenario]);
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    (session, launcher)
}

/// Every event a turn produced, up to and including its terminal.
async fn drain(turn: &mut mango_external_agents::TurnStream) -> Vec<EventKind> {
    let mut events = Vec::new();
    let collected = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(event) = turn.recv().await {
            let terminal = event.is_terminal();
            events.push(event.kind);
            if terminal {
                break;
            }
        }
    })
    .await;
    assert!(
        collected.is_ok(),
        "expected the turn to end, received {events:#?}"
    );
    events
}

/// Reads a turn until the vendor asks for an approval, so the turn is provably still running.
async fn await_approval(turn: &mut mango_external_agents::TurnStream) -> PermissionRequest {
    let asked = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(event) = turn.recv().await {
            if let EventKind::ApprovalRequested { request } = event.kind {
                return Some(request);
            }
        }
        None
    })
    .await
    .expect("expected the recorded approval within the deadline");
    asked.expect("expected the recorded approval to reach the host")
}

/// Reads until the vendor has rendered an activity, proving the turn was live before its output
/// pipe disappears in the EOF regression below.
async fn await_activity_started(turn: &mut mango_external_agents::TurnStream) {
    let seen = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(event) = turn.recv().await {
            if matches!(event.kind, EventKind::ActivityStarted { .. }) {
                return true;
            }
        }
        false
    })
    .await
    .expect("expected an activity within the deadline");
    assert!(seen, "expected the activity before the output pipe closed");
}

/// The command line the library builds, and the environment it does not forward.
#[tokio::test]
async fn a_session_spawns_the_app_server_with_only_the_environment_it_documents() {
    let (_session, launcher) = open("turn").await;

    let launch = launcher.last_launch().expect("expected one launch");
    assert_eq!(launch.argv, vec!["codex", "app-server"]);
    assert_eq!(launch.cwd.to_string_lossy(), "/workspace");
    assert_eq!(
        launch.env.get("CODEX_HOME").map(String::as_str),
        Some("/home/user/.codex"),
        "expected the vendor's own configuration directory to reach the child"
    );
    assert_eq!(
        launch.env.get("CONNECTOR_SECRET"),
        None,
        "expected the host's own secret to stay out of a vendor child"
    );
    assert!(launch.stdin, "expected a writable stdin");
}

/// The app-server's own README: JSON-RPC 2.0 "with the `\"jsonrpc\":\"2.0\"` header omitted on the
/// wire". A frame carrying it is one a strict server refuses.
#[tokio::test]
async fn no_frame_this_harness_writes_carries_the_jsonrpc_header() {
    let (_session, launcher) = open("turn").await;

    for line in launcher.written() {
        assert!(
            !line.contains("\"jsonrpc\""),
            "expected no version header, received {line}"
        );
    }
}

/// The handshake identifies the host to OpenAI's compliance logging. Writing anything but the
/// host's own name would be a misattribution.
#[tokio::test]
async fn the_handshake_names_the_host_rather_than_this_library() {
    let (_session, launcher) = open("turn").await;

    let initialize = launcher
        .written()
        .into_iter()
        .find(|line| line.contains("\"initialize\""))
        .expect("expected an initialize frame");
    let frame: serde_json::Value =
        serde_json::from_str(&initialize).expect("expected a JSON frame");
    assert_eq!(frame["params"]["clientInfo"]["name"], "mango-test");
    assert_eq!(frame["params"]["clientInfo"]["version"], "0.0.1");
}

#[tokio::test]
async fn opening_a_session_adopts_the_thread_the_server_opened() {
    let expected = Transcript::load("turn")
        .thread_id()
        .expect("expected the fixture to open a thread");
    let (session, _) = open("turn").await;

    assert_eq!(session.ids().session_id.as_str(), "chat-1");
    assert_eq!(session.ids().native_session_id, expected);
    assert!(!session.snapshot().resumed);
}

/// The recorded turn: a command runs, the answer streams, usage and quota arrive, the turn ends.
#[tokio::test]
async fn a_recorded_turn_replays_as_a_turn_that_ends_exactly_once() {
    let (session, _) = open("turn").await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "run echo mango"))
        .await
        .expect("expected a turn");
    let events = drain(&mut turn).await;

    let terminals = events
        .iter()
        .filter(|kind| matches!(kind, EventKind::Completed | EventKind::Error { .. }))
        .count();
    assert_eq!(terminals, 1, "expected one terminal, received {events:#?}");
    assert!(
        matches!(events.last(), Some(EventKind::Completed)),
        "expected the terminal last, received {events:#?}"
    );

    assert!(
        matches!(events.first(), Some(EventKind::TurnStarted { .. })),
        "expected the vendor's acceptance of this attempt first, received {events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|kind| matches!(kind, EventKind::TextDelta { .. })),
        "expected the answer to stream, received {events:#?}"
    );
    assert!(
        events.iter().any(|kind| matches!(
            kind,
            EventKind::ActivityStarted { activity, .. }
                if activity.kind == mango_external_agents::ActivityKind::Command
        )),
        "expected the command to be rendered as activity, received {events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|kind| matches!(kind, EventKind::Usage { .. })),
        "expected this turn's own usage, received {events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|kind| matches!(kind, EventKind::AccountLimits { .. })),
        "expected the account quota the server rolled forward, received {events:#?}"
    );
}

/// The turn-scoped counterpart to opening a session: the vendor accepted this attempt and named
/// its own handle for it, first on every turn's stream — not just the first, since it says
/// something about this attempt rather than about the session.
///
/// The session's own identity — its native id, whether it was resumed — is set once, at open,
/// and does not vary from one turn to the next; that fact used to ride a session-wide
/// announcement on the first turn and now lives on [`Session::snapshot`] from the moment the
/// session opens.
#[tokio::test]
async fn every_turn_announces_its_own_acceptance_while_the_sessions_identity_stays_put() {
    let (session, _) = open("turn").await;
    let opened = session.snapshot();

    let mut first = session
        .start_turn(TurnRequest::new("turn-1", "one"))
        .await
        .expect("expected a turn");
    let first = drain(&mut first).await;
    assert!(matches!(first.first(), Some(EventKind::TurnStarted { .. })));

    let mut second = session
        .start_turn(TurnRequest::new("turn-2", "two"))
        .await
        .expect("expected a second turn");
    let second = drain(&mut second).await;
    assert!(
        matches!(second.first(), Some(EventKind::TurnStarted { .. })),
        "expected the second turn to announce its own acceptance too, received {second:#?}"
    );

    assert_eq!(
        session.snapshot().ids.native_session_id,
        opened.ids.native_session_id,
        "expected the session's own identity to stay put across turns"
    );
}

/// The app-server takes `turn/start` on a live turn as a steer. A host that meant a new turn would
/// otherwise hold a stream that never gets a `turn/completed` of its own.
///
/// The recorded approval is what makes this deterministic: the server blocks on its own question
/// until the client answers, so the first turn is provably still running when the second starts.
#[tokio::test]
async fn a_second_turn_started_while_one_is_running_is_refused_rather_than_steered() {
    let (session, launcher) = open("approval").await;
    let mut first = session
        .start_turn(TurnRequest::new("turn-1", "create mango.txt"))
        .await
        .expect("expected a turn");
    await_approval(&mut first).await;

    let before = launcher.written().len();
    let error = session
        .start_turn(TurnRequest::new("turn-2", "two"))
        .await
        .expect_err("expected a refusal, received a second turn");

    assert!(
        matches!(error.cause(), mango_external_agents::Error::Busy),
        "expected a typed busy refusal, received {error:?}"
    );
    assert!(
        error.retryable(),
        "expected the running turn to be worth waiting for"
    );
    assert_eq!(
        launcher.written().len(),
        before,
        "expected nothing to be written to the vendor"
    );
}

/// A cancel cannot name a turn until `turn/start` answers. The unnamed vendor turn still occupies
/// the app-server, so another start in that interval would be treated as a steer of it.
#[tokio::test]
async fn a_cancelled_start_holds_the_slot_until_its_vendor_handle_arrives() {
    let (session, launcher) =
        open_with_delayed_first_start(DelayedStartAnswer::Success, false).await;
    let first_session = Arc::clone(&session);
    let first = tokio::spawn(async move {
        first_session
            .start_turn(TurnRequest::new("turn-reused", "one"))
            .await
    });
    wait_for_turn_start(&launcher).await;

    session
        .cancel(CancelReason::Requested)
        .await
        .expect("expected the pending start to be cancelled");
    let error = session
        .start_turn(TurnRequest::new("turn-reused", "two"))
        .await
        .expect_err("expected the unnamed vendor turn to keep the slot occupied");
    assert!(
        matches!(error.cause(), mango_external_agents::Error::Busy),
        "expected a typed busy refusal, received {error:?}"
    );
    assert_eq!(
        launcher
            .written()
            .iter()
            .filter(|line| line.contains("\"turn/start\""))
            .count(),
        1,
        "expected no second turn/start to reach the occupied app-server"
    );

    session
        .refresh_account_usage()
        .await
        .expect("expected the trigger call to release the held answer");
    let mut first = first
        .await
        .expect("expected the first start task to finish")
        .expect("expected the delayed vendor handle");
    assert!(
        launcher.written().iter().any(|line| {
            line.contains("\"turn/interrupt\"") && line.contains("\"vendor-turn-1\"")
        }),
        "expected the delayed handle to be interrupted once it arrived"
    );
    let events = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut events = Vec::new();
        while let Some(event) = first.recv().await {
            events.push(event.kind);
        }
        events
    })
    .await
    .expect("expected the interrupted turn stream to close");
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, EventKind::Completed | EventKind::Error { .. }))
            .count(),
        1,
        "expected exactly one terminal, received {events:#?}"
    );
    assert!(
        matches!(
            events.last(),
            Some(EventKind::Completed | EventKind::Error { .. })
        ),
        "expected nothing after the terminal, received {events:#?}"
    );
}

/// A caller that abandons `start_turn` before a handle exists cannot strand the admission slot or
/// leave a hidden app-server process behind.
#[tokio::test(start_paused = true)]
async fn dropping_a_start_future_reaps_an_unacknowledged_attempt() {
    let (session, launcher) =
        open_with_delayed_first_start(DelayedStartAnswer::Success, false).await;
    let starting_session = Arc::clone(&session);
    let start = tokio::spawn(async move {
        starting_session
            .start_turn(TurnRequest::new("turn-abandoned", "one"))
            .await
    });
    wait_for_turn_start(&launcher).await;

    start.abort();
    let _ = start.await;
    tokio::task::yield_now().await;
    tokio::time::advance(replay_limits().shutdown_timeout).await;
    tokio::task::yield_now().await;

    assert_eq!(
        launcher.live_children(),
        0,
        "expected an abandoned pre-acknowledgement start to reap its app-server"
    );
    let error = session
        .start_turn(TurnRequest::new("turn-after-abandonment", "two"))
        .await
        .expect_err("expected the reaped session to reject new work");
    assert!(
        matches!(error.cause(), mango_external_agents::Error::Closed { .. }),
        "expected a closed session after bounded abandoned-start teardown, received {error:?}"
    );
}

/// A timeout cannot prove the app-server rejected `turn/start`: it may have accepted it and lost
/// its reply while the first event already fills the one-slot host stream. A second start must
/// therefore stay local instead of becoming a vendor-side steer of the uninterruptible turn.
#[tokio::test]
async fn an_ambiguous_start_timeout_on_a_full_one_slot_stream_cannot_become_a_steer() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(Transcript::load("turn").as_process_intercepting(|frame| {
        if frame.get("method").and_then(serde_json::Value::as_str) == Some("turn/start") {
            return Some(Vec::new());
        }
        None
    }));
    let (host, _) = with_launcher_limits(
        Arc::clone(&launcher),
        None,
        mango_external_agents::Limits {
            turn_channel_capacity: 1,
            request_timeout: std::time::Duration::from_millis(30),
            ..mango_external_agents::Limits::default()
        },
    );
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");

    let first = session
        .start_turn(TurnRequest::new("turn-1", "one"))
        .await
        .expect("expected a recoverable acceptance-unknown stream");
    assert!(
        matches!(
            first.dispatch(),
            mango_external_agents::Dispatch::AcceptanceUnknown
        ),
        "expected explicit acceptance uncertainty, received {first:?}"
    );

    let second = session
        .start_turn(TurnRequest::new("turn-2", "two"))
        .await
        .expect_err("expected the ambiguous first turn to retain the slot");
    assert!(
        matches!(second.cause(), mango_external_agents::Error::Busy),
        "expected a typed busy refusal, received {second:?}"
    );
    assert_eq!(
        launcher
            .written()
            .iter()
            .filter(|line| line.contains("\"turn/start\""))
            .count(),
        1,
        "expected the second start not to reach the vendor as a steer"
    );

    session
        .close(CloseReason::Shutdown)
        .await
        .expect("expected ambiguous-start cleanup");
}

/// A completion can clear a slot before the corresponding `turn/start` answer arrives. A delayed
/// success belongs to that finished start, even when the host reuses its own turn id meanwhile.
#[tokio::test]
async fn a_delayed_start_success_cannot_replace_the_live_turns_vendor_handle() {
    let (session, launcher) =
        open_with_delayed_first_start(DelayedStartAnswer::Success, true).await;
    let first_session = Arc::clone(&session);
    let first = tokio::spawn(async move {
        first_session
            .start_turn(
                TurnRequest::new("turn-reused", "one")
                    .with_configuration(level_patch(PermissionLevel::FullAccess)),
            )
            .await
    });
    wait_for_turn_start(&launcher).await;

    let mut second = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match session
                .start_turn(
                    TurnRequest::new("turn-reused", "two")
                        .with_configuration(level_patch(PermissionLevel::ReadOnly)),
                )
                .await
            {
                Ok(turn) => break turn,
                Err(error) if matches!(error.cause(), mango_external_agents::Error::Busy) => {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("expected the replacement turn, received {error:?}"),
            }
        }
    })
    .await
    .expect("expected the completion notification to free the first slot");
    assert_eq!(second.native_turn_id(), "vendor-turn-2");

    session
        .refresh_account_usage()
        .await
        .expect("expected the trigger call to release the held answer");
    first
        .await
        .expect("expected the delayed start task to finish")
        .expect("expected the delayed start success");
    assert_eq!(
        session.snapshot().configuration.accepted.level,
        Some(PermissionLevel::ReadOnly),
        "expected the later accepted turn's read-only defaults, not the delayed full-access response"
    );
    session
        .cancel(CancelReason::Requested)
        .await
        .expect("expected the replacement turn to be cancelled");
    assert!(
        launcher.written().iter().any(|line| {
            line.contains("\"turn/interrupt\"") && line.contains("\"vendor-turn-2\"")
        }),
        "expected cancel to name the replacement handle, received {:?}",
        launcher.written()
    );
    let _ = drain(&mut second).await;
}

/// The same stale ownership on the error path used to `take()` the replacement out of the slot.
#[tokio::test]
async fn a_delayed_start_error_cannot_evict_a_live_replacement_with_the_same_host_id() {
    let (session, launcher) = open_with_delayed_first_start(DelayedStartAnswer::Error, true).await;
    let first_session = Arc::clone(&session);
    let first = tokio::spawn(async move {
        first_session
            .start_turn(TurnRequest::new("turn-reused", "one"))
            .await
    });
    wait_for_turn_start(&launcher).await;

    let _second = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match session
                .start_turn(TurnRequest::new("turn-reused", "two"))
                .await
            {
                Ok(turn) => break turn,
                Err(error) if matches!(error.cause(), mango_external_agents::Error::Busy) => {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("expected the replacement turn, received {error:?}"),
            }
        }
    })
    .await
    .expect("expected the completion notification to free the first slot");

    session
        .refresh_account_usage()
        .await
        .expect("expected the trigger call to release the held error");
    first
        .await
        .expect("expected the delayed start task to finish")
        .expect_err("expected the held start refusal");
    let error = session
        .start_turn(TurnRequest::new("turn-reused", "three"))
        .await
        .expect_err("expected the live replacement to keep the slot");
    assert!(
        matches!(error.cause(), mango_external_agents::Error::Busy),
        "expected a typed busy refusal, received {error:?}"
    );
    assert_eq!(
        launcher
            .written()
            .iter()
            .filter(|line| line.contains("\"turn/start\""))
            .count(),
        2,
        "expected no third turn/start to reach the occupied app-server"
    );
}

/// Dropping the library stream abandons its owner. The harness interrupts the native turn and
/// frees admission after that terminal commits; a host that wants a browser disconnect to be only
/// a UI event keeps this stream in its own supervisor instead of dropping it.
#[tokio::test]
async fn dropping_a_turn_stream_interrupts_it_and_releases_admission_without_drain() {
    let transcript = Transcript::load("interrupt");
    let thread_id = transcript
        .thread_id()
        .expect("expected the recorded thread id");
    let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let responder_starts = Arc::clone(&starts);
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(transcript.as_process_intercepting(move |frame| {
        let method = frame.get("method").and_then(serde_json::Value::as_str);
        let id = frame.get("id").cloned().unwrap_or(serde_json::Value::Null);
        match method {
            Some("turn/start") => {
                let number = responder_starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Some(vec![
                    serde_json::json!({
                        "id": id,
                        "result": {"turn": {"id": format!("vendor-turn-{number}")}},
                    })
                    .to_string(),
                ])
            }
            Some("turn/interrupt") => Some(vec![
                serde_json::json!({"id": id, "result": {}}).to_string(),
                serde_json::json!({
                    "method": "turn/completed",
                    "params": {
                        "threadId": thread_id,
                        "turn": {
                            "id": frame.pointer("/params/turnId").cloned()
                                .unwrap_or(serde_json::Value::Null),
                            "status": "interrupted",
                        },
                    },
                })
                .to_string(),
            ]),
            _ => None,
        }
    }));
    let (host, launcher) = with_launcher(launcher, None);
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");

    let first = session
        .start_turn(TurnRequest::new("turn-1", "one"))
        .await
        .expect("expected a running turn");
    drop(first);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !launcher
            .written()
            .iter()
            .any(|line| line.contains("\"turn/interrupt\""))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected native cancellation after stream abandonment");

    let second = session
        .start_turn(TurnRequest::new("turn-2", "two"))
        .await
        .expect(
            "expected native completion to release admission without consuming the first stream",
        );
    drop(second);
    session
        .close(CloseReason::Shutdown)
        .await
        .expect("expected cleanup after the ownership check");
}

/// An interrupt acknowledgement does not free admission before Codex sends its terminal frame.
#[tokio::test]
async fn a_native_interrupt_in_progress_keeps_admission_owned() {
    let transcript = Transcript::load("interrupt");
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(transcript.as_process_intercepting(move |frame| {
        let method = frame.get("method").and_then(serde_json::Value::as_str);
        let id = frame.get("id").cloned().unwrap_or(serde_json::Value::Null);
        match method {
            Some("turn/start") => Some(vec![
                serde_json::json!({
                    "id": id,
                    "result": {"turn": {"id": "interrupting-turn"}},
                })
                .to_string(),
            ]),
            Some("turn/interrupt") => Some(vec![
                serde_json::json!({"id": id, "result": {}}).to_string(),
            ]),
            _ => None,
        }
    }));
    let mut limits = replay_limits();
    limits.shutdown_timeout = std::time::Duration::from_millis(30);
    let (host, launcher) = with_launcher_limits(launcher, None, limits);
    let session: Arc<dyn Session> = Arc::from(
        CodexHarness::new()
            .open_session(&host, OpenSession::new("chat-1"))
            .await
            .expect("expected a session"),
    );
    let _turn = session
        .start_turn(TurnRequest::new("turn-1", "one"))
        .await
        .expect("expected a turn");

    let cancelling_session = Arc::clone(&session);
    let cancellation =
        tokio::spawn(async move { cancelling_session.cancel(CancelReason::Requested).await });
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !launcher
            .written()
            .iter()
            .any(|line| line.contains("\"turn/interrupt\""))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected the native interrupt request");
    let error = session
        .start_turn(TurnRequest::new("turn-2", "two"))
        .await
        .expect_err("expected the interrupting native turn to retain admission");
    assert!(
        matches!(error.cause(), mango_external_agents::Error::Busy),
        "expected a typed busy refusal while native cancellation is in progress, received {error:?}"
    );

    assert!(
        matches!(
            cancellation
                .await
                .expect("expected cancellation task to finish")
                .expect_err("expected the silent native turn to time out")
                .cause(),
            mango_external_agents::Error::Timeout { .. }
        ),
        "expected cancellation to escalate after its terminal deadline"
    );
}

/// An inactive native turn cannot retain the host's process beyond its configured idle deadline.
#[tokio::test(start_paused = true)]
async fn an_idle_native_turn_is_cancelled_and_reaped_at_the_hosts_deadline() {
    let transcript = Transcript::load("interrupt");
    let thread_id = transcript
        .thread_id()
        .expect("expected the recorded thread id");
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(transcript.as_process_intercepting(move |frame| {
        let method = frame.get("method").and_then(serde_json::Value::as_str);
        let id = frame.get("id").cloned().unwrap_or(serde_json::Value::Null);
        match method {
            Some("turn/start") => Some(vec![
                serde_json::json!({
                    "id": id,
                    "result": {"turn": {"id": "idle-turn"}},
                })
                .to_string(),
            ]),
            Some("turn/interrupt") => Some(vec![
                serde_json::json!({"id": id, "result": {}}).to_string(),
                serde_json::json!({
                    "method": "turn/completed",
                    "params": {
                        "threadId": thread_id,
                        "turn": {"id": "idle-turn", "status": "interrupted"},
                    },
                })
                .to_string(),
            ]),
            _ => None,
        }
    }));
    let mut limits = replay_limits();
    limits.idle_timeout = std::time::Duration::from_secs(5);
    let (host, launcher) = with_launcher_limits(launcher, None, limits);
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "one"))
        .await
        .expect("expected an active turn");

    tokio::task::yield_now().await;
    tokio::time::advance(limits.idle_timeout).await;
    let events = drain(&mut turn).await;

    assert!(
        events.iter().any(|event| matches!(
            event,
            EventKind::Cancelled {
                reason: CancelReason::Timeout
            }
        )),
        "expected the idle deadline to name its timeout reason, received {events:#?}"
    );
    assert!(
        launcher
            .written()
            .iter()
            .any(|line| line.contains("\"turn/interrupt\"") && line.contains("idle-turn")),
        "expected idle expiry to interrupt the native turn"
    );
    session
        .close(CloseReason::Shutdown)
        .await
        .expect("expected cleanup after idle cancellation");
}

/// A pending approval has its own deadline and does not consume the turn's idle budget.
#[tokio::test(start_paused = true)]
async fn a_pending_approval_pauses_the_native_idle_deadline() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(Transcript::load("approval").as_process());
    let mut limits = replay_limits();
    limits.idle_timeout = std::time::Duration::from_secs(5);
    let (host, launcher) = with_launcher_limits(launcher, None, limits);
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "create mango.txt"))
        .await
        .expect("expected a turn");
    await_approval(&mut turn).await;

    tokio::task::yield_now().await;
    tokio::time::advance(limits.idle_timeout * 2).await;
    tokio::task::yield_now().await;
    assert!(
        !launcher
            .written()
            .iter()
            .any(|line| line.contains("\"turn/interrupt\"")),
        "expected the approval deadline to own this wait phase"
    );

    drop(turn);
    drop(session);
}

/// Refusing a pending approval can complete the turn while cancellation's interrupt is in flight.
/// That completion is already the requested end, so cancel must not report the stale RPC error.
#[tokio::test]
async fn cancellation_succeeds_when_approval_cleanup_completes_the_turn_first() {
    let (session, _launcher) =
        open_with_approval_cancellation_race(InterruptRace::CompletesOwner).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "create mango.txt"))
        .await
        .expect("expected a turn");
    await_approval(&mut turn).await;

    session
        .cancel(CancelReason::Requested)
        .await
        .expect("expected a completed owner to make cancellation a no-op");
    let events = drain(&mut turn).await;
    assert!(
        matches!(events.last(), Some(EventKind::Completed)),
        "expected approval cleanup to complete the turn, received {events:?}"
    );
}

/// An interrupt failure still belongs to the caller when the owner did not end in the race.
#[tokio::test]
async fn cancellation_reports_an_interrupt_error_for_an_active_owner() {
    let (session, _launcher) =
        open_with_approval_cancellation_race(InterruptRace::FailsWhileOwnerRemainsActive).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "create mango.txt"))
        .await
        .expect("expected a turn");
    await_approval(&mut turn).await;

    let error = session
        .cancel(CancelReason::Requested)
        .await
        .expect_err("expected the active owner to receive the interrupt error");
    assert!(
        matches!(error, mango_external_agents::Error::Vendor(ref vendor)
            if vendor.message == "interrupted owner was already closed"),
        "expected the active owner's interrupt error, received {error:?}"
    );
}

/// A host answers the moment it sees the question, because the event is where it learns the id.
///
/// The question has to be answerable by then. Announcing it before registering it left a window —
/// as wide as the host's policy takes to decide — in which `respond` found nothing waiting, so the
/// host spent its one handle on a protocol error and the server stayed blocked until the deadline
/// declined on its behalf.
#[tokio::test]
async fn a_question_is_answerable_the_instant_the_host_is_told_about_it() {
    let broker = SlowBroker::new();
    let (host, _launcher) = host_with(&["approval"], Some(broker.clone()));
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "create mango.txt"))
        .await
        .expect("expected a turn");

    let asked = await_approval(&mut turn).await;
    let option_id = asked
        .options
        .iter()
        .find(|option| option.effect == PermissionEffect::Reject)
        .map(|option| option.id.clone())
        .expect("expected a refusal among the recorded options");
    let answered = session
        .respond(PermissionResponse::from_user(asked.id().clone(), option_id))
        .await;
    broker.make_up_its_mind();

    answered.expect("expected the host's answer to reach a question it was just shown");
    let events = drain(&mut turn).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            EventKind::ApprovalResolved { decision, .. }
                if decision.source == DecisionSource::User
        )),
        "expected the person's answer to be the one that stood, received {events:#?}"
    );
}

/// The deadline begins when the request is created, so an unresponsive broker cannot extend it.
#[tokio::test(start_paused = true)]
async fn a_stalled_broker_is_refused_at_the_original_approval_deadline() {
    let broker = SlowBroker::new();
    let launcher = Arc::new(FakeLauncher::new());
    let transcript = Transcript::load("approval");
    launcher.push(transcript.as_process());
    let (host, launcher) = with_launcher_and_clock(
        launcher,
        Some(broker),
        Arc::new(FixedClock(SystemTime::UNIX_EPOCH)),
    );
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "create mango.txt"))
        .await
        .expect("expected a turn");

    await_approval(&mut turn).await;
    tokio::time::advance(approval_timeout()).await;
    let events = drain(&mut turn).await;

    assert!(
        events.iter().any(|event| matches!(
            event,
            EventKind::ApprovalResolved { decision, .. }
                if decision.option_id == "decline" && decision.source == DecisionSource::Expired
        )),
        "expected the original deadline to refuse the stalled broker, received {events:#?}"
    );
    assert!(
        launcher
            .written()
            .iter()
            .any(|line| line.contains("\"decision\":\"decline\"")),
        "expected the deadline refusal to reach the vendor"
    );
}

/// A choice accepted before expiry remains accepted if the handler runs after expiry.
#[tokio::test(start_paused = true)]
async fn a_timely_host_choice_is_not_retroactively_expired() {
    let broker = SlowBroker::new();
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(Transcript::load("approval").as_process());
    let (host, launcher) = with_launcher_and_clock(
        launcher,
        Some(broker),
        Arc::new(FixedClock(SystemTime::UNIX_EPOCH)),
    );
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "create mango.txt"))
        .await
        .expect("expected a turn");

    let request = await_approval(&mut turn).await;
    session
        .respond(request.allow().expect("expected an allow option"))
        .await
        .expect("expected the pre-deadline host grant to be accepted");
    tokio::time::advance(approval_timeout()).await;
    let events = drain(&mut turn).await;

    assert!(
        events.iter().any(|event| matches!(
            event,
            EventKind::ApprovalResolved { decision, .. }
                if decision.option_id == "accept" && decision.source == DecisionSource::User
        )),
        "expected the accepted host grant to survive the handler delay, received {events:#?}"
    );
    assert!(
        launcher
            .written()
            .iter()
            .any(|line| line.contains("\"decision\":\"accept\"")),
        "expected the timely host grant to reach the vendor"
    );
}

/// A late host grant must be refused whether or not the expiry waiter has already run.
#[tokio::test(start_paused = true)]
async fn a_host_decision_after_the_approval_deadline_is_rejected() {
    let broker = SlowBroker::new();
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(Transcript::load("approval").as_process());
    let (host, launcher) = with_launcher_and_clock(
        launcher,
        Some(broker),
        Arc::new(FixedClock(SystemTime::UNIX_EPOCH)),
    );
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "create mango.txt"))
        .await
        .expect("expected a turn");

    let request = await_approval(&mut turn).await;
    tokio::time::advance(approval_timeout()).await;
    let error = session
        .respond(request.allow().expect("expected an allow option"))
        .await
        .expect_err("expected an expired approval to reject the host grant");
    assert!(
        matches!(error, mango_external_agents::Error::Protocol { .. }),
        "expected an expired approval protocol refusal, received {error:?}"
    );
    let events = drain(&mut turn).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            EventKind::ApprovalResolved { decision, .. }
                if decision.option_id == "decline" && decision.source == DecisionSource::Expired
        )),
        "expected the late grant to become an expiry refusal before completion, received {events:#?}"
    );
    assert!(
        launcher
            .written()
            .iter()
            .any(|line| line.contains("\"decision\":\"decline\"")),
        "expected the expiry refusal to reach the vendor"
    );
    assert!(
        !launcher
            .written()
            .iter()
            .any(|line| line.contains("\"decision\":\"accept\"")),
        "expected the late grant to stay off the wire, received {:?}",
        launcher.written()
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EventKind::Completed)),
        "expected the refused approval to let the turn finish, received {events:#?}"
    );
}

/// A second `host.now()` read between stamping an approval's deadline and translating it back
/// into a wait would let a host clock correction stretch the monotonic window past what
/// `expires_at` advertised. See `CodexHandler::decide`'s clock read in `session.rs`.
#[tokio::test(start_paused = true)]
async fn the_approval_deadline_reuses_one_clock_read_despite_a_backward_jump() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(Transcript::load("approval").as_process());
    let (host, launcher) = with_launcher_and_clock(
        launcher,
        None,
        Arc::new(RewindingClock::new(
            SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(86_400),
            std::time::Duration::from_secs(300),
        )),
    );
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "create mango.txt"))
        .await
        .expect("expected a turn");

    await_approval(&mut turn).await;
    // Past the advertised deadline, but short of the extra five minutes a stray second clock
    // read would add. Checked on the wire rather than by draining to
    // the terminal: a still-open deadline leaves the turn running, which `drain`'s own timeout
    // would report as a hang rather than as the missed deadline this test is about.
    tokio::time::advance(approval_timeout() + std::time::Duration::from_secs(1)).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert!(
        launcher
            .written()
            .iter()
            .any(|line| line.contains("\"decision\":\"decline\"")),
        "expected the advertised deadline to expire on schedule, received {:?}",
        launcher.written()
    );
    let events = drain(&mut turn).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            EventKind::ApprovalResolved { decision, .. }
                if decision.option_id == "decline" && decision.source == DecisionSource::Expired
        )),
        "expected the on-schedule deadline to be reported as an expiry, received {events:#?}"
    );
}

/// The recorded approval: the agent asks to leave its sandbox, the host refuses, the turn goes on.
#[tokio::test]
async fn a_recorded_approval_reaches_the_host_and_its_refusal_reaches_the_vendor() {
    let (session, launcher) = open("approval").await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "create mango.txt"))
        .await
        .expect("expected a turn");

    let mut asked = None;
    let events = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut events = Vec::new();
        while let Some(event) = turn.recv().await {
            let terminal = event.is_terminal();
            if let EventKind::ApprovalRequested { request } = &event.kind
                && asked.is_none()
            {
                asked = Some(request.clone());
                let refusal = request.deny().expect("expected a way to refuse");
                session
                    .respond(refusal)
                    .await
                    .expect("expected the refusal to land");
            }
            events.push(event.kind);
            if terminal {
                break;
            }
        }
        events
    })
    .await
    .expect("expected the turn to end");

    let asked = asked.expect("expected the recorded approval to reach the host");
    assert_eq!(asked.kind, mango_external_agents::ActivityKind::Command);
    assert!(
        asked.options.iter().any(|option| option.id == "accept")
            && asked.options.iter().any(|option| option.id == "decline"),
        "expected the vendor's own decisions as options, received {:?}",
        asked.options
    );
    assert!(
        events.iter().any(|kind| matches!(
            kind,
            EventKind::ApprovalResolved { decision, .. }
                if decision.option_id == "decline" && decision.source == DecisionSource::User
        )),
        "expected the refusal to be recorded, received {events:#?}"
    );

    let answer = launcher
        .written()
        .into_iter()
        .find(|line| line.contains("\"decision\""))
        .expect("expected the refusal to reach the vendor");
    let frame: serde_json::Value = serde_json::from_str(&answer).expect("expected a JSON frame");
    assert_eq!(frame["result"]["decision"], "decline");
}

/// With a policy, the question still reaches the host as an event — the library brokers approvals,
/// it does not hide them — and the vendor receives the policy's answer.
#[tokio::test]
async fn a_broker_that_refuses_answers_the_vendor_and_the_host_still_sees_the_question() {
    let broker = FixedBroker::new(BrokerDecision::Deny {
        reason: String::from("no writes outside the workspace"),
    });
    let (host, launcher) = host_with(&["approval"], Some(broker.clone()));
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "create mango.txt"))
        .await
        .expect("expected a turn");
    let events = drain(&mut turn).await;

    assert_eq!(
        broker.seen().len(),
        1,
        "expected the policy to be asked exactly once"
    );
    assert!(
        events
            .iter()
            .any(|kind| matches!(kind, EventKind::ApprovalRequested { .. })),
        "expected the host to see the question anyway, received {events:#?}"
    );
    assert!(
        events.iter().any(|kind| matches!(
            kind,
            EventKind::ApprovalResolved { decision, .. }
                if decision.source == DecisionSource::AutoReview
        )),
        "expected the answer to be attributed to the policy, received {events:#?}"
    );
    assert!(
        launcher
            .written()
            .iter()
            .any(|line| line.contains("\"decline\"")),
        "expected the policy's refusal to reach the vendor"
    );
}

/// The recorded interrupt: a running turn is stopped, and the stream still ends.
#[tokio::test]
async fn a_cancelled_turn_still_completes_and_says_why_it_stopped() {
    let (session, launcher) = open("interrupt").await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "count to 200"))
        .await
        .expect("expected a turn");

    session
        .cancel(CancelReason::Shutdown)
        .await
        .expect("expected the cancel to land");
    let events = drain(&mut turn).await;

    assert!(
        events.iter().any(|kind| matches!(
            kind,
            EventKind::Cancelled {
                reason: CancelReason::Shutdown
            }
        )),
        "expected this side's own reason rather than the server's bare interruption, \
         received {events:#?}"
    );
    assert!(
        matches!(events.last(), Some(EventKind::Completed)),
        "expected the marker to be followed by a terminal, received {events:#?}"
    );
    assert!(
        launcher
            .written()
            .iter()
            .any(|line| line.contains("turn/interrupt")),
        "expected the vendor to be told to stop"
    );
}

/// The status a session settles on, once it stops changing.
///
/// Only the terminal status is asserted on, never the `Closing` before it: a
/// [`SessionSubscription`] coalesces, so a teardown that does not block between its two
/// transitions publishes both and a subscriber legitimately sees only the second.
///
/// Reports the status it is stuck on rather than hanging to a bare timeout: a lifecycle that never
/// ends has to fail as "received `Ready`", which names the bug, not as "the test timed out", which
/// names nothing.
async fn status_once_settled(lifecycle: &mut SessionSubscription) -> SessionStatus {
    loop {
        if lifecycle.current().status == SessionStatus::Closed {
            return SessionStatus::Closed;
        }
        let next =
            tokio::time::timeout(std::time::Duration::from_secs(5), lifecycle.changed()).await;
        if !matches!(next, Ok(Some(_))) {
            return lifecycle.current().status;
        }
    }
}

/// The host's lifetime token stops an ordinary vendor turn even when no approval is pending.
#[tokio::test]
async fn host_shutdown_cancels_an_ordinary_turn_and_ends_its_child() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(Transcript::load("interrupt").as_process());
    let cancel = mango_external_agents::CancelToken::new();
    let (host, _) = with_launcher_limits_and_cancel(
        launcher,
        None,
        mango_external_agents::Limits {
            request_timeout: std::time::Duration::from_secs(5),
            ..mango_external_agents::Limits::default()
        },
        cancel.clone(),
    );
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "count slowly"))
        .await
        .expect("expected a running turn");

    cancel.cancel();
    let events = drain(&mut turn).await;
    assert!(matches!(
        events
            .iter()
            .find(|event| matches!(event, EventKind::Cancelled { .. })),
        Some(EventKind::Cancelled {
            reason: CancelReason::Shutdown
        })
    ));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, EventKind::Completed | EventKind::Error { .. }))
            .count(),
        1,
        "expected one shutdown terminal, received {events:?}"
    );
    assert!(
        session
            .start_turn(TurnRequest::new("turn-2", "after shutdown"))
            .await
            .is_err(),
        "expected the shutdown session to reject a second turn"
    );
}

/// A session the host's own lifetime token tore down never saw `close`, so the shutdown watcher
/// owes the lifecycle the transitions `close` publishes. Without them the snapshot keeps saying
/// `Ready` while every later start is refused, and a subscriber is told nothing at all.
#[tokio::test]
async fn host_shutdown_publishes_the_lifecycle_its_watcher_drove() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(Transcript::load("turn").as_process());
    let cancel = mango_external_agents::CancelToken::new();
    let (host, _) = with_launcher_limits_and_cancel(
        Arc::clone(&launcher),
        None,
        replay_limits(),
        cancel.clone(),
    );
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let mut lifecycle = session.subscribe();
    assert_eq!(lifecycle.current().status, SessionStatus::Ready);

    cancel.cancel();

    assert_eq!(
        status_once_settled(&mut lifecycle).await,
        SessionStatus::Closed,
        "expected the watcher to publish the terminal close publishes"
    );
    assert_eq!(
        launcher.live_children(),
        0,
        "expected Closed to mean the child is gone, not only that the status moved"
    );
}

/// A full unread transcript cannot delay terminal session publication: terminal events are
/// reserved outside the payload budget and teardown never awaits the consumer.
#[tokio::test(start_paused = true)]
async fn the_watcher_closes_without_waiting_for_an_unread_transcript() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(Transcript::load("turn").as_process());
    let cancel = mango_external_agents::CancelToken::new();
    let (host, _) = with_launcher_limits_and_cancel(
        launcher,
        None,
        mango_external_agents::Limits {
            turn_channel_capacity: 1,
            request_timeout: std::time::Duration::from_secs(5),
            ..mango_external_agents::Limits::default()
        },
        cancel.clone(),
    );
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let mut lifecycle = session.subscribe();
    // Held, never read: this is the host that walked away.
    let _unread = session
        .start_turn(TurnRequest::new("turn-1", "run echo mango"))
        .await
        .expect("expected a turn");
    // Let the pump fill the one-event payload budget.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    cancel.cancel();

    assert_eq!(
        status_once_settled(&mut lifecycle).await,
        SessionStatus::Closed,
        "expected teardown not to wait for transcript consumption"
    );
}

/// The other way the watcher runs: the app-server disappears on its own. `connection_terminated`
/// stops new work and cancels `terminated`, and the lifecycle has to follow — a host left holding
/// a `Ready` session over a dead connection learns it only from the next refusal.
#[tokio::test]
async fn a_dead_app_server_connection_publishes_the_session_terminal() {
    let launcher = Arc::new(FakeLauncher::new());
    let close_stdout = mango_external_agents::CancelToken::new();
    launcher.push(
        Transcript::load("turn")
            .as_process()
            .ending_stdout_when(close_stdout.clone()),
    );
    let (host, _) = with_launcher(Arc::clone(&launcher), None);
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let mut lifecycle = session.subscribe();

    close_stdout.cancel();

    assert_eq!(
        status_once_settled(&mut lifecycle).await,
        SessionStatus::Closed,
        "expected a terminated connection to end the published lifecycle"
    );
    assert_eq!(
        launcher.live_children(),
        0,
        "expected the watcher to reap the child it reported closed"
    );
}

/// Shutdown while an approval is waiting records cancellation rather than an expired deadline.
#[tokio::test]
async fn host_shutdown_marks_a_pending_approval_as_cancelled() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(Transcript::load("approval").as_process());
    let cancel = mango_external_agents::CancelToken::new();
    let (host, _) = with_launcher_limits_and_cancel(
        launcher,
        None,
        mango_external_agents::Limits::default(),
        cancel.clone(),
    );
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "create mango.txt"))
        .await
        .expect("expected a turn");
    let _approval = await_approval(&mut turn).await;

    cancel.cancel();
    let events = drain(&mut turn).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            EventKind::ApprovalResolved { decision, .. }
                if decision.source == DecisionSource::Cancelled
        )),
        "expected shutdown to record a cancelled approval, received {events:?}"
    );
}

/// EOF after a live activity has no `turn/completed` to reduce, so it must still fail the stream.
#[tokio::test]
async fn app_server_eof_after_an_activity_fails_the_turn_without_waiting_for_a_timeout() {
    let transcript = Transcript::load("interrupt");
    let thread_id = transcript
        .thread_id()
        .expect("expected a recorded thread id");
    let launcher = Arc::new(FakeLauncher::new());
    let close_stdout = mango_external_agents::CancelToken::new();
    launcher.push(
        transcript
            .as_process_intercepting(move |frame| {
                if frame.get("method").and_then(serde_json::Value::as_str) != Some("turn/start") {
                    return None;
                }
                let id = frame.get("id").cloned().unwrap_or(serde_json::Value::Null);
                Some(vec![
                    serde_json::json!({
                        "id": id,
                        "result": {"turn": {"id": "vendor-turn-eof"}},
                    })
                    .to_string(),
                    serde_json::json!({
                        "method": "item/started",
                        "params": {
                            "threadId": thread_id,
                            "turnId": "vendor-turn-eof",
                            "item": {
                                "id": "command-eof",
                                "type": "commandExecution",
                                "command": "sleep 60",
                                "cwd": "/workspace",
                                "status": "inProgress",
                            },
                        },
                    })
                    .to_string(),
                ])
            })
            .ending_stdout_when(close_stdout.clone()),
    );
    let (host, _) = with_launcher_limits(
        Arc::clone(&launcher),
        None,
        mango_external_agents::Limits {
            request_timeout: std::time::Duration::from_secs(5),
            ..mango_external_agents::Limits::default()
        },
    );
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "run a command"))
        .await
        .expect("expected the turn handle before EOF");

    await_activity_started(&mut turn).await;
    close_stdout.cancel();
    let events = drain(&mut turn).await;
    assert!(
        matches!(events.last(), Some(EventKind::Error { .. })),
        "expected EOF to end with a vendor error, received {events:?}"
    );
    assert_eq!(
        launcher.live_children(),
        0,
        "expected EOF cleanup to reap the still-running child"
    );
    assert!(
        session.close(CloseReason::Shutdown).await.is_ok(),
        "expected cleanup after EOF to return without waiting for the request timeout"
    );
}

/// A host that stops reading its turn must not be able to stop the session from closing.
///
/// The turn channel is bounded, so an unread stream parks whatever is feeding it. If that park
/// happens while the turn lock is held, every other caller of that lock — `cancel`, `close`, the
/// next `start_turn` — parks behind a host that is never coming back, and a close that cannot
/// return leaves a `codex app-server` running for the life of the process.
///
/// The capacity is exactly what `start_turn` emits before returning the stream: one
/// `TurnStarted`. The channel is therefore full the instant the turn is returned, and the pump's
/// first real event parks. A larger capacity and the test proves nothing; a smaller one and
/// `start_turn` itself parks and this hangs instead of failing.
#[tokio::test(start_paused = true)]
async fn a_host_that_stops_reading_cannot_stop_the_session_from_closing() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(Transcript::load("turn").as_process());
    let (host, _) = with_launcher_limits(
        launcher,
        None,
        mango_external_agents::Limits {
            turn_channel_capacity: 1,
            request_timeout: std::time::Duration::from_secs(5),
            ..mango_external_agents::Limits::default()
        },
    );
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");

    let turn = session
        .start_turn(TurnRequest::new("turn-1", "run echo mango"))
        .await
        .expect("expected a turn");
    // Held, never read: this is the host that walked away.
    let _unread = turn;

    // Twice, so the pump gets the thread and then reaches its park inside the emit.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // Comfortably past the grace a closing session offers the terminal under. With the guard held
    // across the emit there is no timer at all, so a paused runtime advances straight to this one
    // and the close is still parked when it fires.
    let closed = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        session.close(CloseReason::Shutdown),
    )
    .await
    .expect("expected the close to answer rather than park behind an unread turn");
    closed.expect("expected a clean close");
}

/// Closing twice must not fail the second caller, and the child goes with the first.
#[tokio::test]
async fn closing_twice_is_harmless_and_ends_the_child() {
    let (session, _) = open("turn").await;

    session
        .close(CloseReason::Requested)
        .await
        .expect("expected a clean close");
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected closing twice to be harmless");

    let error = session
        .start_turn(TurnRequest::new("turn-1", "anything"))
        .await
        .expect_err("expected a closed session to refuse a turn");
    assert!(
        matches!(error.cause(), mango_external_agents::Error::Closed { .. }),
        "expected a closed-session refusal, received {error:?}"
    );
}

/// The session owns its app-server, so dropping the final session handle starts bounded cleanup.
#[tokio::test]
async fn dropping_the_owning_session_reaps_its_child() {
    let (session, launcher) = open("turn").await;

    drop(session);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while launcher.live_children() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected the dropped session to reap its app-server");
}

/// A steer for a turn that is not the one running is refused here rather than landing on whatever
/// turn happens to be live.
#[tokio::test]
async fn a_steer_for_a_turn_that_is_not_running_is_refused_before_it_is_sent() {
    let (session, launcher) = open("turn").await;
    let before = launcher.written().len();

    let outcome = session
        .steer(Steer {
            turn_id: mango_external_agents::TurnId::new("turn-that-ended"),
            native_turn_id: String::from("01a00000-0000"),
            input: String::from("also this"),
        })
        .await
        .expect("expected an outcome rather than an error");

    assert_eq!(
        outcome,
        mango_external_agents::SteerOutcome::Rejected {
            reason: mango_external_agents::SteerRejection::TurnAlreadyCompleted
        }
    );
    assert_eq!(
        launcher.written().len(),
        before,
        "expected nothing to be written to the vendor"
    );
}

/// And when the server names no review thread at all, the fallback has to be reachable.
///
/// `reviewThreadId` deserialises to an empty string when it is absent, so wrapping it
/// unconditionally made the fallback dead code and handed the host a thread with no name. A host
/// doing what the core tells it to — refuse a thread it is not subscribed to — would have dropped
/// a review it should have shown.
#[tokio::test]
async fn a_review_the_server_named_no_thread_for_runs_on_the_one_this_session_holds() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(Transcript::load("review").as_process_intercepting(|frame| {
        if frame.get("method").and_then(serde_json::Value::as_str) != Some("review/start") {
            return None;
        }
        // The recording carries a review thread; no recording carries its absence.
        Some(vec![
            serde_json::json!({
                "id": frame.get("id").cloned().unwrap_or(serde_json::Value::Null),
                "result": {"turn": {"id": "review-turn-1"}},
            })
            .to_string(),
        ])
    }));
    let (host, launcher) = with_launcher(launcher, None);
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");

    let review = session
        .start_review(mango_external_agents::ReviewRequest {
            turn_id: mango_external_agents::TurnId::new("review-1"),
            target: mango_external_agents::ReviewTarget::UncommittedChanges,
        })
        .await
        .expect("expected a review");

    assert_eq!(
        review.review_thread_id,
        session.ids().native_session_id,
        "expected a review the server named no thread for to fall back to this session's own"
    );
    let writes_before_steer = launcher.written().len();
    let steer = session
        .steer(Steer {
            turn_id: review.turn.turn_id().clone(),
            native_turn_id: review.turn.native_turn_id().to_owned(),
            input: String::from("also inspect this"),
        })
        .await
        .expect("expected a local review-steer rejection");
    assert_eq!(
        steer,
        mango_external_agents::SteerOutcome::Rejected {
            reason: mango_external_agents::SteerRejection::TurnNotSteerable,
        }
    );
    assert_eq!(
        launcher.written().len(),
        writes_before_steer,
        "expected a review steer not to reach the vendor"
    );
}

/// The recorded review. An inline review runs on the session's own thread, which is what leaving
/// `delivery` absent asks for — a detached one would stream on a thread the reducer drops.
#[tokio::test]
async fn a_recorded_review_runs_on_the_thread_this_session_is_subscribed_to() {
    let (session, launcher) = open("review").await;

    let mut review = session
        .start_review(mango_external_agents::ReviewRequest {
            turn_id: mango_external_agents::TurnId::new("review-1"),
            target: mango_external_agents::ReviewTarget::UncommittedChanges,
        })
        .await
        .expect("expected a review");

    assert_eq!(
        review.review_thread_id,
        session.ids().native_session_id,
        "expected an inline review on this session's own thread"
    );

    let started = launcher
        .written()
        .into_iter()
        .find(|line| line.contains("review/start"))
        .expect("expected the vendor to be asked");
    let frame: serde_json::Value = serde_json::from_str(&started).expect("expected a JSON frame");
    assert_eq!(frame["params"]["target"]["type"], "uncommittedChanges");
    assert!(
        frame["params"].get("delivery").is_none(),
        "expected no delivery member, which the server reads as inline; received {frame}"
    );

    // The review's own id, as `review/start` answered it — the one every later item and
    // completion on this stream is routed against.
    let native_turn_id = review.turn.native_turn_id().to_owned();

    let events = drain(&mut review.turn).await;
    assert!(
        matches!(events.last(), Some(EventKind::Completed)),
        "expected the review to end like any other turn, received {events:#?}"
    );
    // A review is an attempt like any other, and gets its own acceptance first — there is no
    // session-wide announcement left to ride the stream instead. Captured transcripts show the
    // vendor's own `turn/started` notification can name a different id for a review than
    // `review/start`'s response does; the announcement must carry the response's id regardless
    // of whether that notification or the response itself won the race to send it.
    assert_eq!(
        events.first(),
        Some(&EventKind::TurnStarted {
            native_turn_id: native_turn_id.clone()
        }),
        "expected the review to announce its own acceptance under its own response id, \
         received {events:#?}"
    );
}

/// The recorded `thread/list`, through the core's own bounding.
#[tokio::test]
async fn listing_the_vendors_own_sessions_asks_for_a_bounded_page() {
    let (session, launcher) = open("handshake").await;

    let page = session
        .list_sessions(SessionQuery::default())
        .await
        .expect("expected a page");

    let asked = launcher
        .written()
        .into_iter()
        .find(|line| line.contains("thread/list"))
        .expect("expected the vendor to be asked");
    let frame: serde_json::Value = serde_json::from_str(&asked).expect("expected a JSON frame");
    assert_eq!(
        frame["params"]["limit"], 50,
        "expected the cap to be asked for rather than applied to the answer"
    );
    assert!(!page.truncated);
    for row in &page.sessions {
        assert!(
            !row.native_session_id.is_empty(),
            "expected every row to name a conversation a host could adopt"
        );
    }
}

/// The recorded `account/rateLimits/read`.
#[tokio::test]
async fn refreshing_account_usage_reports_the_windows_the_vendor_named() {
    let (session, _) = open("handshake").await;

    let usage = session
        .refresh_account_usage()
        .await
        .expect("expected a reading");
    let limits = usage.limits.expect("expected the recorded snapshot");
    assert!(
        !limits.windows.is_empty(),
        "expected at least one metered window, received {limits:?}"
    );
    for window in &limits.windows {
        assert!(
            (0.0..=100.0).contains(&window.used_percent),
            "expected a renderable percentage, received {window:?}"
        );
    }
}

/// A permission level is two vendor settings that move together; a routing is a third.
#[tokio::test]
async fn the_configuration_a_host_chose_reaches_the_thread_as_three_settings() {
    let (host, launcher) = host_replaying(&["turn"]);
    let request = OpenSession::new("chat-1").with_configuration(permission_patch(
        PermissionLevel::Default,
        ApprovalRouting::AutoReview,
    ));
    let _session = CodexHarness::new()
        .open_session(&host, request)
        .await
        .expect("expected a session");

    let started = launcher
        .written()
        .into_iter()
        .find(|line| line.contains("thread/start"))
        .expect("expected a thread to be opened");
    let frame: serde_json::Value = serde_json::from_str(&started).expect("expected a JSON frame");
    assert_eq!(frame["params"]["sandbox"], "workspace-write");
    assert_eq!(frame["params"]["approvalPolicy"], "on-request");
    assert_eq!(frame["params"]["approvalsReviewer"], "auto_review");
    assert_eq!(frame["params"]["cwd"], "/workspace");
}

/// The old session-wide announcement rode the first turn's stream, so a first turn the server
/// refused took it down with it, and only the next turn to actually start carried it. That
/// coupling is gone: the session's own identity is known from the moment the session opens,
/// before any turn runs, so a refused first turn cannot take it down with it.
#[tokio::test]
async fn a_first_turn_the_server_refused_does_not_take_the_sessions_identity_with_it() {
    let refused = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = Arc::clone(&refused);
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(
        Transcript::load("turn").as_process_intercepting(move |frame| {
            if frame.get("method").and_then(serde_json::Value::as_str) != Some("turn/start")
                || seen.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                return None;
            }
            // No recording holds a refused turn, and inventing one as a fixture would put words in
            // the app-server's mouth. This is the transport's own shape, not the vendor's dialect.
            Some(vec![
                serde_json::json!({
                    "id": frame.get("id").cloned().unwrap_or(serde_json::Value::Null),
                    "error": {"code": -32602, "message": "no such model"},
                })
                .to_string(),
            ])
        }),
    );
    let (host, _launcher) = with_launcher(launcher, None);
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let opened = session.snapshot();
    assert!(
        !opened.ids.native_session_id.is_empty(),
        "expected the session's own identity to be known before any turn ran"
    );

    let error = session
        .start_turn(TurnRequest::new("turn-1", "one"))
        .await
        .expect_err("expected the server's refusal to reach the caller");
    assert_eq!(
        error.dispatch(),
        mango_external_agents::Dispatch::Accepted,
        "expected a definitive server rejection not to be safe to replay"
    );

    let mut second = session
        .start_turn(TurnRequest::new("turn-2", "two"))
        .await
        .expect("expected the next turn to run");
    let events = drain(&mut second).await;
    assert!(
        matches!(events.first(), Some(EventKind::TurnStarted { .. })),
        "expected the next turn that actually started to announce its own acceptance, \
         received {events:#?}"
    );
    assert_eq!(
        session.snapshot().ids.native_session_id,
        opened.ids.native_session_id,
        "expected the refused first turn to leave the session's identity untouched"
    );
}

#[tokio::test]
async fn review_targets_are_available_through_the_shared_session_trait() {
    for (target, expected) in [
        (
            mango_external_agents::ReviewTarget::BaseBranch {
                branch: String::from("main"),
            },
            serde_json::json!({"type": "baseBranch", "branch": "main"}),
        ),
        (
            mango_external_agents::ReviewTarget::Commit {
                sha: String::from("abc123"),
                title: Some(String::from("Fix parser")),
            },
            serde_json::json!({"type": "commit", "sha": "abc123", "title": "Fix parser"}),
        ),
        (
            mango_external_agents::ReviewTarget::Custom {
                instructions: String::from("Review error handling"),
            },
            serde_json::json!({"type": "custom", "instructions": "Review error handling"}),
        ),
    ] {
        let (session, launcher) = open("review").await;
        let mut review = session
            .start_review(mango_external_agents::ReviewRequest {
                turn_id: mango_external_agents::TurnId::new("review-target"),
                target,
            })
            .await
            .expect("expected the documented review target to start");
        let events = drain(&mut review.turn).await;
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, EventKind::Completed))
                .count(),
            1
        );
        let frame = launcher
            .written()
            .iter()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|frame| frame["method"] == "review/start")
            .expect("review/start request");
        assert_eq!(frame["params"]["target"], expected);
    }
}

#[tokio::test]
async fn successful_permission_overrides_become_observable_defaults() {
    let (session, launcher) = open("turn").await;
    // Nothing was asked for at open, so the opening picture carries no level at all — proof that
    // the override below is not retroactively rewriting what opening the session reported.
    let opened_level = session.snapshot().configuration.accepted.level;
    assert_eq!(opened_level, None);

    let mut first = session
        .start_turn(
            TurnRequest::new("override", "mango")
                .with_configuration(level_patch(PermissionLevel::FullAccess)),
        )
        .await
        .expect("expected full-access override");
    drain(&mut first).await;
    assert_eq!(
        session.snapshot().configuration.accepted.level,
        Some(PermissionLevel::FullAccess)
    );
    let mut second = session
        .start_turn(TurnRequest::new("inherited", "done"))
        .await
        .expect("expected inherited turn");
    drain(&mut second).await;
    let frames: Vec<serde_json::Value> = launcher
        .written()
        .iter()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|frame| frame["method"] == "turn/start")
        .collect();
    assert_eq!(frames.len(), 2);
    assert!(frames[1]["params"].get("sandboxPolicy").is_none());
    assert!(frames[1]["params"].get("approvalPolicy").is_none());
    assert_eq!(
        session.snapshot().configuration.accepted.level,
        Some(PermissionLevel::FullAccess)
    );
}

#[tokio::test]
async fn explicit_close_never_relabels_a_vendor_terminal_that_already_won() {
    let (session, _) = open("approval").await;
    let mut stream = session
        .start_turn(TurnRequest::new("closing", "mango"))
        .await
        .expect("active turn");
    await_approval(&mut stream).await;
    session
        .close(CloseReason::ConsentRevoked)
        .await
        .expect("session close");
    let events = drain(&mut stream).await;
    assert!(
        events.iter().all(|event| !matches!(
            event,
            EventKind::Cancelled { reason }
                if *reason != CancelReason::ConsentRevoked
        )),
        "expected a close marker to retain consent revocation, received {events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, EventKind::Completed | EventKind::Error { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn omitted_permissions_defer_to_the_users_vendor_defaults() {
    let (session, launcher) = open("turn").await;
    let mut turn = session
        .start_turn(TurnRequest::new("vendor-defaults", "mango"))
        .await
        .expect("default-profile turn");
    drain(&mut turn).await;
    for frame in launcher
        .written()
        .iter()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|frame| {
            matches!(
                frame["method"].as_str(),
                Some("thread/start" | "turn/start")
            )
        })
    {
        for field in [
            "sandbox",
            "sandboxPolicy",
            "approvalPolicy",
            "approvalsReviewer",
        ] {
            assert!(
                frame["params"].get(field).is_none(),
                "expected omitted permissions to leave {field} to the user, received {frame}"
            );
        }
    }
}

#[tokio::test]
async fn every_extended_review_target_replays_its_own_capture() {
    for (scenario, target) in [
        (
            "review-base-branch",
            mango_external_agents::ReviewTarget::BaseBranch {
                branch: String::from("HEAD"),
            },
        ),
        (
            "review-commit",
            mango_external_agents::ReviewTarget::Commit {
                sha: String::from("HEAD"),
                title: Some(String::from("Current commit")),
            },
        ),
        (
            "review-custom",
            mango_external_agents::ReviewTarget::Custom {
                instructions: String::from(
                    "Review the current changes for correctness. Do not modify files.",
                ),
            },
        ),
    ] {
        let transcript = Transcript::load(scenario);
        let expected = transcript
            .steps
            .iter()
            .find(|step| step.method() == Some("review/start"))
            .expect("captured review request")
            .sent["params"]["target"]
            .clone();
        let (session, launcher) = open(scenario).await;
        let mut review = session
            .start_review(mango_external_agents::ReviewRequest {
                turn_id: mango_external_agents::TurnId::new("captured-review"),
                target,
            })
            .await
            .expect("expected captured review to start");
        let events = drain(&mut review.turn).await;
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, EventKind::Completed))
                .count(),
            1,
            "expected one completion for {scenario}, received {events:?}"
        );
        let actual = launcher
            .written()
            .iter()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|frame| frame["method"] == "review/start")
            .expect("review request");
        assert_eq!(actual["params"]["target"], expected);
    }
}

#[tokio::test]
async fn permission_transitions_replay_the_captured_policies() {
    let (session, launcher) = open("permission-transitions").await;
    let levels = [
        PermissionLevel::ReadOnly,
        PermissionLevel::Default,
        PermissionLevel::FullAccess,
    ];
    for (index, level) in levels.into_iter().enumerate() {
        let mut stream = session
            .start_turn(
                TurnRequest::new(format!("policy-{index}"), "mango")
                    .with_configuration(level_patch(level)),
            )
            .await
            .expect("expected permission transition");
        let events = drain(&mut stream).await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, EventKind::Completed)),
            "expected completed policy transition {level:?}, received {events:?}"
        );
        assert_eq!(session.snapshot().configuration.accepted.level, Some(level));
    }

    let mut inherited = session
        .start_turn(TurnRequest::new("policy-inherited", "mango"))
        .await
        .expect("expected an inherited permission transition");
    let events = drain(&mut inherited).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EventKind::Completed)),
        "expected completed inherited transition, received {events:?}"
    );
    assert_eq!(
        session.snapshot().configuration.accepted.level,
        Some(PermissionLevel::FullAccess),
        "an unconfigured turn must retain the last accepted explicit level"
    );
    let actual: Vec<serde_json::Value> = launcher
        .written()
        .iter()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|frame| frame["method"] == "turn/start")
        .collect();
    let transcript = Transcript::load("permission-transitions");
    let expected: Vec<_> = transcript
        .steps
        .iter()
        .filter(|step| step.method() == Some("turn/start"))
        .collect();
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().take(levels.len()).zip(expected) {
        assert_eq!(
            actual["params"]["sandboxPolicy"]["type"],
            expected.sent["params"]["sandboxPolicy"]["type"]
        );
        assert_eq!(
            actual["params"]["approvalPolicy"],
            expected.sent["params"]["approvalPolicy"]
        );
    }
    let inherited = actual
        .last()
        .expect("expected an unconfigured turn/start frame");
    for member in [
        "sandbox",
        "sandboxPolicy",
        "approvalPolicy",
        "approvalsReviewer",
    ] {
        assert!(
            inherited["params"].get(member).is_none(),
            "expected inherited turn to omit {member}, received {inherited}"
        );
    }
}

/// A narrowing override changes both the sandbox and the approval policy.
#[tokio::test]
async fn a_turn_permission_override_applies_both_halves_atomically() {
    let (host, launcher) = host_replaying(&["turn"]);
    let request = OpenSession::new("chat-1").with_configuration(permission_patch(
        PermissionLevel::FullAccess,
        ApprovalRouting::User,
    ));
    let session = CodexHarness::new()
        .open_session(&host, request)
        .await
        .expect("expected a session");

    let mut turn = session
        .start_turn(
            TurnRequest::new("turn-1", "read the tree").with_configuration(permission_patch(
                PermissionLevel::ReadOnly,
                ApprovalRouting::User,
            )),
        )
        .await
        .expect("expected the explicit read-only override to start");
    drain(&mut turn).await;
    let frame: serde_json::Value = launcher
        .written()
        .iter()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|frame| frame["method"] == "turn/start")
        .expect("expected turn/start");
    assert_eq!(frame["params"]["approvalPolicy"], "never");
    assert_eq!(
        frame["params"]["sandboxPolicy"],
        serde_json::json!({"type": "readOnly", "networkAccess": false})
    );
}

/// The same session's own level is not a change, and still runs.
#[tokio::test]
async fn a_turn_that_repeats_the_sessions_own_level_is_the_turn_it_always_was() {
    let (host, _launcher) = host_replaying(&["turn"]);
    let request = OpenSession::new("chat-1").with_configuration(permission_patch(
        PermissionLevel::Default,
        ApprovalRouting::User,
    ));
    let session = CodexHarness::new()
        .open_session(&host, request)
        .await
        .expect("expected a session");

    let mut turn = session
        .start_turn(
            TurnRequest::new("turn-1", "create mango.txt").with_configuration(permission_patch(
                PermissionLevel::Default,
                ApprovalRouting::AutoReview,
            )),
        )
        .await
        .expect("expected the turn to run");
    let events = drain(&mut turn).await;
    assert!(
        !events.is_empty(),
        "expected the recorded turn to produce events"
    );
}

/// A CLI that is not there is a discovery, not an error: `Discovery::not_installed` is what a host
/// renders as "install it", and a failure would be what it renders as "something broke".
#[tokio::test]
async fn a_machine_with_no_codex_on_it_discovers_nothing_rather_than_failing() {
    let launcher = Arc::new(FakeLauncher::new());
    let host = HostContext::builder()
        .launcher(launcher)
        .cwd("/workspace")
        .client_info("mango-test", "0.0.1")
        .build()
        .expect("expected a host");

    let discovery = CodexHarness::new()
        .discover(&host)
        .await
        .expect("expected a discovery rather than a failure");
    assert_eq!(
        discovery.gate,
        mango_external_agents::GateVerdict::NotInstalled
    );
    assert!(!discovery.is_usable());
}

/// An old build reports the gate verdict rather than crashing, and claims no capabilities.
#[tokio::test]
async fn an_older_codex_on_the_path_reports_the_gate_verdict() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(FakeProcess::transcript(["codex-cli 0.147.0"]));
    let host = HostContext::builder()
        .launcher(launcher.clone())
        .cwd("/workspace")
        .client_info("mango-test", "0.0.1")
        .build()
        .expect("expected a host");

    let discovery = CodexHarness::new()
        .discover(&host)
        .await
        .expect("expected a discovery");

    assert_eq!(
        discovery.gate,
        mango_external_agents::GateVerdict::VersionTooOld {
            found: String::from("0.147.0"),
            minimum: String::from(mango_agent_codex::MINIMUM_CODEX_VERSION),
        }
    );
    assert!(!discovery.is_usable());
    assert_eq!(
        discovery.capabilities,
        mango_external_agents::harness::DiscoveredCapabilities::none(),
        "expected a build that cannot be driven to claim nothing"
    );
    assert_eq!(
        launcher.launches().len(),
        1,
        "expected no app-server to be spawned for a build that would be refused"
    );
}

/// The contract every harness must pass, run against the recorded conversation.
#[tokio::test]
async fn the_harness_passes_the_cores_conformance_suite() {
    // The suite probes before it opens anything, and a probe spawns `codex --version` first.
    //
    // `approval` rather than a longer conversation: the suite watches for a session update on a
    // subscription it opens before `check_turn` rather than starting a turn of its own, so it
    // needs no more recorded turns than it did before session state existed — and this is the one
    // recorded conversation where the vendor actually asks for an approval, which is the round
    // trip the suite cannot prove anywhere else.
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(version_answer());
    launcher.push(Transcript::load("handshake").as_process());
    let transcript = Transcript::load("approval");
    let thread_id = transcript
        .thread_id()
        .expect("expected the approval transcript to name its thread");
    let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let responder_starts = Arc::clone(&starts);
    launcher.push(transcript.as_process_intercepting(move |frame| {
        let method = frame.get("method").and_then(serde_json::Value::as_str);
        let id = frame.get("id").cloned().unwrap_or(serde_json::Value::Null);
        match method {
            Some("turn/start")
                if responder_starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 =>
            {
                None
            }
            Some("turn/start") => Some(vec![
                serde_json::json!({
                    "id": id,
                    "result": {"turn": {"id": "conformance-cancel"}},
                })
                .to_string(),
            ]),
            Some("turn/interrupt") => Some(vec![
                serde_json::json!({"id": id, "result": {}}).to_string(),
                serde_json::json!({
                    "method": "turn/completed",
                    "params": {
                        "threadId": thread_id,
                        "turn": {"id": "conformance-cancel", "status": "interrupted"},
                    },
                })
                .to_string(),
            ]),
            _ => None,
        }
    }));
    let (host, _) = with_launcher(launcher, None);
    let report = mango_external_agents::testing::conformance::run(
        &CodexHarness::new(),
        &host,
        mango_external_agents::testing::conformance::Options {
            session_id: String::from("conformance-session"),
            prompt: String::from("create mango.txt"),
            turn_timeout: std::time::Duration::from_secs(10),
        },
    )
    .await;

    report.assert_passed();
    assert!(
        report.checks.len() >= 8,
        "expected the whole suite to run, received {:?}",
        report.checks
    );
}

/// Host MCP requests are unsupported on fresh and resumed Codex sessions.
#[tokio::test]
async fn host_mcp_servers_are_refused_before_spawning_codex() {
    for mode in [
        None,
        Some(mango_external_agents::ResumeMode::Strict),
        Some(mango_external_agents::ResumeMode::Fallback),
    ] {
        let (host, launcher) = host_replaying(&["handshake"]);
        let request = OpenSession::new("chat-1").with_mcp_servers(vec![
            mango_external_agents::McpServer::stdio("docs", "docs-mcp"),
        ]);

        let request = match mode {
            Some(mode) => request.resuming("existing-thread", mode),
            None => request,
        };
        let result = CodexHarness::new().open_session(&host, request).await;

        assert_eq!(
            launcher.launches().len(),
            0,
            "expected unsupported MCP configuration to be refused before spawning Codex"
        );
        let error = match result {
            Ok(_) => panic!("expected the typed MCP passthrough refusal"),
            Err(error) => error,
        };
        assert!(
            matches!(
                error.cause(),
                mango_external_agents::Error::HostConfiguration {
                    expected: "no MCP servers for a harness without MCP passthrough",
                    received,
                } if received == "MCP server count 1"
            ),
            "expected the typed MCP passthrough refusal"
        );
        assert_eq!(
            error.dispatch(),
            mango_external_agents::Dispatch::NotSubmitted
        );
    }
}

/// Codex has no "drop my override and fall back to config.toml" surface on `thread/start` or
/// `thread/resume`: neither accepts anything meaning "unset this". A patch that asks for a reset
/// at open time is refused explicitly rather than silently encoded as if nothing had been asked
/// for — which is what `overrides()` reading `ConfigurationChange::Reset` as absent would
/// otherwise do.
#[tokio::test]
async fn a_reset_requested_at_open_is_refused_rather_than_silently_dropped() {
    let (host, launcher) = host_replaying(&["turn"]);
    let request = OpenSession::new("chat-1")
        .with_configuration(ConfigurationPatch::new().level(ConfigurationChange::Reset));

    let error = match CodexHarness::new().open_session(&host, request).await {
        Ok(_) => panic!("expected a reset request to be refused rather than accepted"),
        Err(error) => error,
    };

    assert!(
        matches!(
            error.cause(),
            mango_external_agents::Error::HostConfiguration { .. }
        ),
        "expected a typed configuration refusal, received {error:?}"
    );
    assert!(
        error.to_string().contains("remove the override on level"),
        "expected the refusal to name what was asked, received {error}"
    );
    assert_eq!(
        launcher.launches().len(),
        1,
        "expected the refusal before thread/start, not a vendor round trip"
    );
    assert!(
        !launcher
            .written()
            .iter()
            .any(|line| line.contains("\"thread/start\"")),
        "expected no thread/start to reach the vendor for a patch this harness cannot encode"
    );
}

/// The same refusal, for a reset asked of a running session's own turn. `turn/start` has no reset
/// semantics either, so this must fail before anything reaches the wire rather than being read as
/// an omitted axis.
#[tokio::test]
async fn a_reset_requested_on_a_turn_is_refused_rather_than_silently_dropped() {
    let (session, launcher) = open("turn").await;
    let before = launcher.written().len();

    let error = session
        .start_turn(
            TurnRequest::new("turn-1", "mango")
                .with_configuration(ConfigurationPatch::new().routing(ConfigurationChange::Reset)),
        )
        .await
        .expect_err("expected a reset request to be refused rather than accepted");

    assert!(
        matches!(
            error.cause(),
            mango_external_agents::Error::HostConfiguration { .. }
        ),
        "expected a typed configuration refusal, received {error:?}"
    );
    assert!(
        error.to_string().contains("remove the override on routing"),
        "expected the refusal to name what was asked, received {error}"
    );
    assert_eq!(
        launcher.written().len(),
        before,
        "expected no turn/start to reach the vendor for a patch this harness cannot encode"
    );
}
