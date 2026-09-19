//! The harness driven against transcripts a real `codex app-server` produced.
//!
//! Nothing here spawns anything: `mea capture` recorded the conversations, and a fake child
//! replays them. That is the only way to test a dialect on a machine where the vendor's CLI is not
//! installed — which is every machine, in CI.

#[path = "replay/contracts.rs"]
mod contracts;
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::SystemTime;

use mango_agent_codex::CodexHarness;
use mango_external_agents::event::EventKind;
use mango_external_agents::permission::{
    BrokerDecision, DecisionSource, PermissionBroker, PermissionEffect, PermissionRequest,
    PermissionResponse,
};
use mango_external_agents::testing::{Announcer, FakeLauncher, FakeProcess};
use mango_external_agents::{
    Answer as QuestionAnswerValue, AnswerValue, ApprovalRouting, CancelReason, Clock, CloseReason,
    ConfigurationChange, ConfigurationPatch, EnvSource, ExitStatus, Harness, HostContext,
    InterruptOutcome, LaunchSpec, ManagedProcess, McpServer, McpTransport, OpenSession,
    PermissionLevel, ProcessControl, ProcessLauncher, QuestionId, QuestionOptionId,
    QuestionOutcome, QuestionResponse, Session, SessionQuery, SessionStatus, SessionSubscription,
    Steer, TurnRequest, UnsupportedQuestion,
};
use support::{Transcript, workspace_path};

/// A test-controlled pause in a fake child operation.
struct FakeGate {
    entered: AtomicBool,
    entered_notice: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

impl FakeGate {
    fn closed() -> Arc<Self> {
        Arc::new(Self {
            entered: AtomicBool::new(false),
            entered_notice: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        })
    }

    async fn wait_until_entered(&self) {
        loop {
            let notice = self.entered_notice.notified();
            if self.entered.load(Ordering::Acquire) {
                return;
            }
            notice.await;
        }
    }

    async fn wait_for_release(&self) {
        self.entered.store(true, Ordering::Release);
        self.entered_notice.notify_waiters();
        self.release
            .acquire()
            .await
            .expect("the fake gate must stay open")
            .forget();
    }

    fn open(&self) {
        self.release.add_permits(1);
    }
}

/// A fake launcher that can hold one process kill or one native-interrupt write.
#[derive(Clone)]
struct GatedLauncher {
    inner: Arc<FakeLauncher>,
    kill_gate: Option<Arc<FakeGate>>,
    interrupt_write_gate: Option<Arc<FakeGate>>,
    keep_child_alive_after_stdin_close: bool,
}

impl GatedLauncher {
    fn new(
        inner: Arc<FakeLauncher>,
        kill_gate: Option<Arc<FakeGate>>,
        interrupt_write_gate: Option<Arc<FakeGate>>,
    ) -> Self {
        Self {
            inner,
            kill_gate,
            interrupt_write_gate,
            keep_child_alive_after_stdin_close: false,
        }
    }

    fn keeping_child_alive_after_stdin_close(mut self) -> Self {
        self.keep_child_alive_after_stdin_close = true;
        self
    }
}

#[async_trait::async_trait]
impl ProcessLauncher for GatedLauncher {
    async fn spawn(&self, spec: LaunchSpec) -> mango_external_agents::Result<ManagedProcess> {
        let mut child = self.inner.spawn(spec).await?;
        if let Some(gate) = &self.interrupt_write_gate {
            child.stdin = child.stdin.take().map(|inner| {
                Box::new(GatedStdin {
                    inner,
                    gate: Arc::clone(gate),
                }) as Box<dyn mango_external_agents::ByteSink>
            });
        }
        if self.keep_child_alive_after_stdin_close {
            child.stdin = child.stdin.take().map(|inner| {
                Box::new(NonTerminatingStdin { inner }) as Box<dyn mango_external_agents::ByteSink>
            });
        }
        if let Some(gate) = &self.kill_gate {
            child.control = Arc::new(GatedProcessControl {
                inner: child.control,
                kill_gate: Arc::clone(gate),
            });
        }
        Ok(child)
    }
}

/// Keeps a fake child alive when its app-server link closes so kill ownership remains observable.
struct NonTerminatingStdin {
    inner: Box<dyn mango_external_agents::ByteSink>,
}

#[async_trait::async_trait]
impl mango_external_agents::ByteSink for NonTerminatingStdin {
    async fn write_all(&mut self, bytes: &[u8]) -> mango_external_agents::Result<()> {
        self.inner.write_all(bytes).await
    }

    async fn close(&mut self) -> mango_external_agents::Result<()> {
        Ok(())
    }
}

/// Makes the app-server probe lose stdin and report a cleanup failure to its host.
struct CleanupRequiredProbeLauncher {
    inner: Arc<FakeLauncher>,
}

#[async_trait::async_trait]
impl ProcessLauncher for CleanupRequiredProbeLauncher {
    async fn spawn(&self, spec: LaunchSpec) -> mango_external_agents::Result<ManagedProcess> {
        let app_server = spec.argv.iter().any(|argument| argument == "app-server");
        let mut child = self.inner.spawn(spec).await?;
        if app_server {
            child.stdin = None;
            child.control = Arc::new(FailingProbeCleanup {
                inner: child.control,
            });
        }
        Ok(child)
    }
}

/// Refuses forced termination so discovery has to return the host cleanup handle.
struct FailingProbeCleanup {
    inner: Arc<dyn ProcessControl>,
}

#[async_trait::async_trait]
impl ProcessControl for FailingProbeCleanup {
    fn pid(&self) -> Option<u32> {
        self.inner.pid()
    }

    fn stderr_tail(&self) -> String {
        self.inner.stderr_tail()
    }

    async fn wait(&self) -> mango_external_agents::Result<ExitStatus> {
        self.inner.wait().await
    }

    async fn kill(&self, _reason: CancelReason) -> mango_external_agents::Result<()> {
        Err(mango_external_agents::Error::Closed {
            subject: "test probe process",
        })
    }
}

/// Holds an interrupt write before it reaches the recorded app-server.
struct GatedStdin {
    inner: Box<dyn mango_external_agents::ByteSink>,
    gate: Arc<FakeGate>,
}

#[async_trait::async_trait]
impl mango_external_agents::ByteSink for GatedStdin {
    async fn write_all(&mut self, bytes: &[u8]) -> mango_external_agents::Result<()> {
        if bytes
            .windows(b"\"turn/interrupt\"".len())
            .any(|part| part == b"\"turn/interrupt\"")
        {
            self.gate.wait_for_release().await;
        }
        self.inner.write_all(bytes).await
    }

    async fn close(&mut self) -> mango_external_agents::Result<()> {
        self.inner.close().await
    }
}

/// Holds generic process termination while preserving every other fake-child operation.
struct GatedProcessControl {
    inner: Arc<dyn ProcessControl>,
    kill_gate: Arc<FakeGate>,
}

#[async_trait::async_trait]
impl ProcessControl for GatedProcessControl {
    fn pid(&self) -> Option<u32> {
        self.inner.pid()
    }

    fn stderr_tail(&self) -> String {
        self.inner.stderr_tail()
    }

    async fn wait(&self) -> mango_external_agents::Result<ExitStatus> {
        self.inner.wait().await
    }

    async fn interrupt(
        &self,
        reason: CancelReason,
    ) -> mango_external_agents::Result<InterruptOutcome> {
        self.inner.interrupt(reason).await
    }

    async fn kill(&self, reason: CancelReason) -> mango_external_agents::Result<()> {
        self.kill_gate.wait_for_release().await;
        self.inner.kill(reason).await
    }
}

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
    let host = host_context(launcher.clone(), broker, limits, cancel, clock);
    (host, launcher)
}

/// Builds a replay host around either the ordinary fake launcher or a gated wrapper.
fn host_context(
    launcher: Arc<dyn ProcessLauncher>,
    broker: Option<Arc<dyn PermissionBroker>>,
    limits: mango_external_agents::Limits,
    cancel: mango_external_agents::CancelToken,
    clock: Option<Arc<dyn Clock>>,
) -> HostContext {
    let mut builder = HostContext::builder()
        .launcher(launcher)
        .cwd(workspace_path())
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
    builder.build().expect("expected a host")
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
    assert_eq!(launch.cwd, workspace_path());
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

#[tokio::test]
async fn host_mcp_servers_use_the_thread_config_override_without_widening_the_child_environment() {
    let (host, launcher) = host_replaying(&["handshake"]);
    let servers = vec![
        McpServer {
            name: String::from("docs"),
            transport: McpTransport::Stdio {
                command: String::from("docs-mcp"),
                args: vec![String::from("--stdio")],
                env: [(String::from("DOCS_KEY"), String::from("server-secret"))].into(),
            },
        },
        McpServer {
            name: String::from("remote"),
            transport: McpTransport::Http {
                url: String::from("https://example.com/mcp"),
                headers: [(String::from("X-Docs-Key"), String::from("header-secret"))].into(),
            },
        },
    ];
    let _session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1").with_mcp_servers(servers))
        .await
        .expect("expected supported host MCP configuration");

    let start = launcher
        .written()
        .into_iter()
        .find(|line| line.contains("thread/start"))
        .expect("expected a thread start");
    let frame: serde_json::Value = serde_json::from_str(&start).expect("expected JSON");
    let config = &frame["params"]["config"]["mcp_servers"];
    assert_eq!(config["docs"]["command"], "docs-mcp");
    assert_eq!(config["docs"]["args"], serde_json::json!(["--stdio"]));
    assert_eq!(config["docs"]["env"]["DOCS_KEY"], "server-secret");
    assert_eq!(config["remote"]["url"], "https://example.com/mcp");
    assert_eq!(
        config["remote"]["http_headers"]["X-Docs-Key"],
        "header-secret"
    );
    let launch = launcher
        .last_launch()
        .expect("expected one app-server launch");
    assert!(!launch.env.contains_key("DOCS_KEY"));
    assert!(!launch.env.contains_key("CONNECTOR_SECRET"));
}

#[tokio::test]
async fn opening_with_explicit_effort_applies_it_on_the_thread_and_reports_acceptance() {
    let (host, launcher) = host_replaying(&["handshake"]);
    let mut request = OpenSession::new("chat-1");
    request.configuration =
        ConfigurationPatch::new().effort(ConfigurationChange::Set(String::from("high")));
    let session = CodexHarness::new()
        .open_session(&host, request)
        .await
        .expect("expected a supported per-thread effort override");

    let start = launcher
        .written()
        .into_iter()
        .find(|line| line.contains("thread/start"))
        .expect("expected a thread start");
    let frame: serde_json::Value = serde_json::from_str(&start).expect("expected JSON");
    assert_eq!(frame["params"]["config"]["model_reasoning_effort"], "high");
    assert_eq!(
        session.snapshot().configuration.accepted.effort.as_deref(),
        Some("high")
    );
}

#[tokio::test]
async fn malformed_explicit_effort_is_refused_before_opening_codex() {
    let (host, launcher) = host_replaying(&["handshake"]);
    let mut request = OpenSession::new("chat-1");
    request.configuration =
        ConfigurationPatch::new().effort(ConfigurationChange::Set(String::from("high\nunsafe")));
    let result = CodexHarness::new().open_session(&host, request).await;
    assert!(
        matches!(result, Err(error) if matches!(error.cause(), mango_external_agents::Error::HostConfiguration { expected: "a nonempty, bounded model or effort id without controls", .. })),
        "expected a typed configuration refusal"
    );
    assert!(launcher.launches().is_empty(), "expected no vendor launch");
}

#[tokio::test]
async fn malformed_turn_model_is_refused_without_submitting_a_prompt() {
    let (host, launcher) = host_replaying(&["handshake"]);
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected an open thread");
    let result = session
        .start_turn(TurnRequest::new("turn-1", "hello").with_configuration(
            ConfigurationPatch::new().model(ConfigurationChange::Set(String::from("bad\nmodel"))),
        ))
        .await;
    assert!(
        matches!(result, Err(error) if matches!(error.cause(), mango_external_agents::Error::HostConfiguration { expected: "a nonempty, bounded model or effort id without controls", .. }))
    );
    assert!(
        !launcher
            .written()
            .iter()
            .any(|line| line.contains("turn/start")),
        "expected no prompt submission"
    );
}

/// A pinned app-server resume error, with the captured handshake and new-thread answer.
/// The fake owns the error injection so each test can assert the wire consequence.
struct ResumeErrorServer {
    message: String,
}

/// The captured thread answer, returned to a resume request with that request's id.
struct ResumeSuccessServer {
    read_workspace: String,
    resume_id: Option<&'static str>,
    resume_workspace: Option<String>,
}

impl ResumeSuccessServer {
    fn process(self) -> FakeProcess {
        let transcript = Transcript::load("handshake");
        let result = transcript
            .every_received()
            .into_iter()
            .find(|frame| frame.pointer("/result/thread/id").is_some())
            .expect("expected the captured thread answer")
            .clone();
        transcript.as_process_intercepting(move |frame| {
            if frame["method"] == "thread/read" {
                return Some(vec![serde_json::json!({
                    "id": frame["id"],
                    "result": {"thread": {"id": frame["params"]["threadId"], "cwd": self.read_workspace}},
                }).to_string()]);
            }
            (frame["method"] == "thread/resume").then(|| {
                let mut answer = result.clone();
                answer["id"] = frame["id"].clone();
                if let Some(id) = self.resume_id {
                    answer["result"]["thread"]["id"] = id.into();
                }
                if let Some(cwd) = self.resume_workspace.as_ref() {
                    answer["result"]["thread"]["cwd"] = cwd.clone().into();
                }
                vec![answer.to_string()]
            })
        })
    }
}

#[tokio::test]
async fn resumed_codex_thread_receives_the_same_host_mcp_override() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(
        ResumeSuccessServer {
            read_workspace: workspace_path().to_string_lossy().into_owned(),
            resume_id: None,
            resume_workspace: None,
        }
        .process(),
    );
    let (host, launcher) = with_launcher(launcher, None);
    let native_id = Transcript::load("handshake")
        .thread_id()
        .expect("expected a captured thread id");
    let session = CodexHarness::new()
        .open_session(
            &host,
            OpenSession::new("chat-1")
                .resuming(&native_id, mango_external_agents::ResumeMode::Strict)
                .with_mcp_servers(vec![McpServer::stdio("docs", "docs-mcp")]),
        )
        .await
        .expect("expected the host MCP override on resume");

    assert!(session.snapshot().resumed);
    let written = launcher.written();
    assert!(!written.iter().any(|line| line.contains("thread/start")));
    let read = written
        .iter()
        .find(|line| line.contains("thread/read"))
        .expect("expected a metadata-only workspace preflight");
    let read: serde_json::Value = serde_json::from_str(read).expect("expected JSON");
    assert_eq!(read["params"]["threadId"], native_id);
    assert_eq!(read["params"]["includeTurns"], false);
    let resume = written
        .iter()
        .find(|line| line.contains("thread/resume"))
        .expect("expected a resume request");
    let frame: serde_json::Value = serde_json::from_str(resume).expect("expected JSON");
    assert_eq!(frame["params"]["threadId"], native_id);
    assert_eq!(
        frame["params"]["config"]["mcp_servers"]["docs"]["command"],
        "docs-mcp"
    );
}

#[tokio::test]
async fn a_foreign_resume_workspace_is_refused_before_loading_a_thread_or_starting_fresh() {
    for mode in [
        mango_external_agents::ResumeMode::Strict,
        mango_external_agents::ResumeMode::Fallback,
    ] {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(
            ResumeSuccessServer {
                read_workspace: String::from("/other/workspace"),
                resume_id: None,
                resume_workspace: None,
            }
            .process(),
        );
        let (host, launcher) = with_launcher(launcher, None);
        let native_id = Transcript::load("handshake")
            .thread_id()
            .expect("expected a captured thread id");
        let result = CodexHarness::new()
            .open_session(&host, OpenSession::new("chat-1").resuming(&native_id, mode))
            .await;
        assert!(
            matches!(result, Err(error) if matches!(error.cause(), mango_external_agents::Error::HostConfiguration { expected: "a Codex thread in the host's authorized workspace", .. }))
        );
        let written = launcher.written();
        assert!(written.iter().any(|line| line.contains("thread/read")));
        assert!(!written.iter().any(|line| line.contains("thread/resume")));
        assert!(!written.iter().any(|line| line.contains("thread/start")));
    }
}

#[tokio::test]
async fn resume_refuses_a_changed_identity_or_workspace_after_a_valid_preflight() {
    for (changed_id, changed_cwd) in [
        (Some("different-thread"), None),
        (None, Some("/other/workspace")),
    ] {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(
            ResumeSuccessServer {
                read_workspace: workspace_path().to_string_lossy().into_owned(),
                resume_id: changed_id,
                resume_workspace: changed_cwd.map(str::to_owned),
            }
            .process(),
        );
        let (host, launcher) = with_launcher(launcher, None);
        let native_id = Transcript::load("handshake")
            .thread_id()
            .expect("expected a captured thread id");
        let result = CodexHarness::new()
            .open_session(
                &host,
                OpenSession::new("chat-1")
                    .resuming(&native_id, mango_external_agents::ResumeMode::Strict),
            )
            .await;
        assert!(
            matches!(result, Err(error) if matches!(error.cause(), mango_external_agents::Error::HostConfiguration { .. }))
        );
        assert!(
            !launcher
                .written()
                .iter()
                .any(|line| line.contains("turn/start")),
            "expected no prompt after a changed resume answer"
        );
    }
}

impl ResumeErrorServer {
    fn process(self) -> FakeProcess {
        Transcript::load("handshake").as_process_intercepting(move |frame| {
            if frame["method"] == "thread/read" {
                return Some(vec![serde_json::json!({
                    "id": frame["id"],
                    "error": {"code": -32600, "message": format!("thread not loaded: {}", frame["params"]["threadId"].as_str().unwrap_or_default())},
                }).to_string()]);
            }
            (frame["method"] == "thread/resume").then(|| {
                vec![
                    serde_json::json!({
                        "id": frame["id"],
                        "error": {"code": -32600, "message": self.message},
                    })
                    .to_string(),
                ]
            })
        })
    }
}

#[tokio::test]
async fn fallback_starts_fresh_only_after_the_pinned_missing_rollout_result() {
    let launcher = Arc::new(FakeLauncher::new());
    let missing = "01a09ca7-cd7c-7312-8569-205578cada28";
    launcher.push(
        ResumeErrorServer {
            message: format!("no rollout found for thread id {missing}"),
        }
        .process(),
    );
    let (host, launcher) = with_launcher(launcher, None);

    let session = CodexHarness::new()
        .open_session(
            &host,
            OpenSession::new("chat-1")
                .resuming(missing, mango_external_agents::ResumeMode::Fallback),
        )
        .await
        .expect("expected a conclusive missing rollout to start a new thread");

    assert!(!session.snapshot().resumed);
    assert!(session.snapshot().fallback_reason.is_some());
    assert_ne!(session.ids().native_session_id, missing);
    assert!(
        launcher
            .written()
            .iter()
            .any(|line| line.contains("thread/start"))
    );
}

#[tokio::test]
async fn fallback_preserves_an_unrelated_resume_failure_without_starting_fresh() {
    let launcher = Arc::new(FakeLauncher::new());
    let missing = "01a09ca7-cd7c-7312-8569-205578cada28";
    launcher.push(
        ResumeErrorServer {
            message: String::from("failed to load configuration"),
        }
        .process(),
    );
    let (host, launcher) = with_launcher(launcher, None);

    let error = CodexHarness::new()
        .open_session(
            &host,
            OpenSession::new("chat-1")
                .resuming(missing, mango_external_agents::ResumeMode::Fallback),
        )
        .await;

    assert!(
        error.is_err(),
        "expected the configuration failure to be returned"
    );
    assert!(
        !launcher
            .written()
            .iter()
            .any(|line| line.contains("thread/start")),
        "expected no fresh conversation after a configuration failure"
    );
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

/// A close future is only a waiter. Once it begins generic child cleanup, abandoning that waiter
/// cannot leave a live app-server or require another host close to finish the work.
#[tokio::test(start_paused = true)]
async fn aborting_a_close_future_leaves_its_owned_reaper_running() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(Transcript::load("turn").as_process());
    let kill_gate = FakeGate::closed();
    let gated = Arc::new(GatedLauncher::new(
        Arc::clone(&launcher),
        Some(Arc::clone(&kill_gate)),
        None,
    ));
    let host = host_context(
        gated,
        None,
        replay_limits(),
        mango_external_agents::CancelToken::new(),
        None,
    );
    let session: Arc<dyn Session> = Arc::from(
        CodexHarness::new()
            .open_session(&host, OpenSession::new("chat-1"))
            .await
            .expect("expected a session"),
    );
    let mut lifecycle = session.subscribe();

    let closing_session = Arc::clone(&session);
    let close = tokio::spawn(async move { closing_session.close(CloseReason::Shutdown).await });
    kill_gate.wait_until_entered().await;
    close.abort();
    let _ = close.await;

    kill_gate.open();
    assert_eq!(
        status_once_settled(&mut lifecycle).await,
        SessionStatus::Closed,
        "expected the detached close worker to publish Closed after its caller disappeared"
    );
    assert_eq!(
        launcher.live_children(),
        0,
        "expected the detached close worker to reap the generic child"
    );

    session
        .close(CloseReason::Shutdown)
        .await
        .expect("expected repeated close to observe the first close worker's result");
}

/// A timed-out session reaper leaves recovery with the host, including for later close callers.
#[tokio::test(start_paused = true)]
async fn failed_close_retains_one_cleanup_control_for_reconciliation() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(Transcript::load("turn").as_process());
    let kill_gate = FakeGate::closed();
    let gated = Arc::new(
        GatedLauncher::new(Arc::clone(&launcher), Some(Arc::clone(&kill_gate)), None)
            .keeping_child_alive_after_stdin_close(),
    );
    let limits = mango_external_agents::Limits {
        shutdown_timeout: std::time::Duration::from_secs(1),
        ..replay_limits()
    };
    let host = host_context(
        gated,
        None,
        limits,
        mango_external_agents::CancelToken::new(),
        None,
    );
    let session: Arc<dyn Session> = Arc::from(
        CodexHarness::new()
            .open_session(&host, OpenSession::new("chat-1"))
            .await
            .expect("expected a session"),
    );

    let closing_session = Arc::clone(&session);
    let close = tokio::spawn(async move { closing_session.close(CloseReason::Shutdown).await });
    kill_gate.wait_until_entered().await;
    tokio::time::advance(limits.shutdown_timeout).await;

    let error = close
        .await
        .expect("expected the close waiter to complete")
        .expect_err("expected bounded cleanup to time out");
    assert!(
        matches!(
            error.cause(),
            mango_external_agents::Error::Timeout {
                operation,
                after,
            } if operation == "process-tree termination" && *after == limits.shutdown_timeout
        ),
        "expected the bounded process cleanup timeout, received {error:?}"
    );
    let control = error
        .cleanup_control()
        .expect("expected the failed close to retain a cleanup control");

    let repeated = session
        .close(CloseReason::Shutdown)
        .await
        .expect_err("expected the repeated close to observe the first cleanup failure");
    let repeated_control = repeated
        .cleanup_control()
        .expect("expected a repeated close to retain the same cleanup control");
    assert!(
        Arc::ptr_eq(&control, &repeated_control),
        "expected every close waiter to receive the same process control"
    );
    assert_eq!(
        launcher.live_children(),
        1,
        "expected the timed-out reaper to leave the child for host reconciliation"
    );

    kill_gate.open();
    mango_external_agents::process::stop_process_with_limits(
        control.as_ref(),
        CancelReason::Shutdown,
        host.limits(),
    )
    .await
    .expect("expected the host to reconcile and reap the child");
    assert_eq!(
        launcher.live_children(),
        0,
        "expected host reconciliation to reap the timed-out child"
    );
}

/// Cancellation assigns its stop worker before the native interrupt write can wait. Dropping the
/// caller during that write must still bound the silent vendor turn and reap its app-server.
#[tokio::test(start_paused = true)]
async fn aborting_a_cancel_future_still_bounds_a_silent_native_turn() {
    let transcript = Transcript::load("interrupt");
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(transcript.as_process_intercepting(|frame| {
        let method = frame.get("method").and_then(serde_json::Value::as_str);
        let id = frame.get("id").cloned().unwrap_or(serde_json::Value::Null);
        match method {
            Some("turn/start") => Some(vec![
                serde_json::json!({"id": id, "result": {"turn": {"id": "silent-turn"}}})
                    .to_string(),
            ]),
            // Codex accepted the interrupt but never tells us the turn ended.
            Some("turn/interrupt") => Some(vec![
                serde_json::json!({"id": id, "result": {}}).to_string(),
            ]),
            _ => None,
        }
    }));
    let interrupt_write_gate = FakeGate::closed();
    let gated = Arc::new(GatedLauncher::new(
        Arc::clone(&launcher),
        None,
        Some(Arc::clone(&interrupt_write_gate)),
    ));
    let mut limits = replay_limits();
    limits.kill_grace = std::time::Duration::from_secs(1);
    limits.shutdown_timeout = std::time::Duration::from_secs(1);
    let host = host_context(
        gated,
        None,
        limits,
        mango_external_agents::CancelToken::new(),
        None,
    );
    let session: Arc<dyn Session> = Arc::from(
        CodexHarness::new()
            .open_session(&host, OpenSession::new("chat-1"))
            .await
            .expect("expected a session"),
    );
    let mut lifecycle = session.subscribe();
    let _turn = session
        .start_turn(TurnRequest::new("turn-1", "keep running"))
        .await
        .expect("expected a live native turn");

    let cancelling_session = Arc::clone(&session);
    let cancel =
        tokio::spawn(async move { cancelling_session.cancel(CancelReason::Requested).await });
    interrupt_write_gate.wait_until_entered().await;
    cancel.abort();
    let _ = cancel.await;

    for _ in 0..10 {
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
    }
    assert_eq!(
        status_once_settled(&mut lifecycle).await,
        SessionStatus::Closed,
        "expected the owned stop worker to close the silent native turn without its caller",
    );
    assert_eq!(
        launcher.live_children(),
        0,
        "expected bounded stop escalation to reap the silent app-server"
    );

    interrupt_write_gate.open();
}

/// Transcript pressure after `start_turn` returned is a terminal stream failure, not a reason to
/// leave the accepted native prompt running. The recorded notifications are released only after
/// the handle exists, then two unread payloads exercise a one-event budget.
#[tokio::test]
async fn post_acceptance_transcript_overflow_cancels_and_reaps_the_native_turn() {
    let transcript = Transcript::load("turn");
    let thread_id = transcript
        .thread_id()
        .expect("expected the recorded transcript to name its thread");
    let notifications_released = Arc::new(AtomicBool::new(false));
    let released = Arc::clone(&notifications_released);
    let notification_count = Arc::new(AtomicUsize::new(0));
    let emitted = Arc::clone(&notification_count);
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(transcript.as_process_intercepting(move |frame| {
        let method = frame.get("method").and_then(serde_json::Value::as_str);
        let id = frame.get("id").cloned().unwrap_or(serde_json::Value::Null);
        match method {
            Some("turn/start") => Some(vec![
                serde_json::json!({
                    "id": id,
                    "result": {"turn": {"id": "overflow-turn"}},
                })
                .to_string(),
            ]),
            Some("account/rateLimits/read") if released.load(Ordering::Acquire) => {
                // Release one payload per gate call. This lets the handler commit the first
                // payload before the second reaches its stream, so the test observes stream
                // overflow rather than the JSON-RPC notification queue's own backpressure.
                let delta = match emitted.fetch_add(1, Ordering::AcqRel) {
                    0 => "first unread payload",
                    _ => "second unread payload",
                };
                Some(vec![
                    serde_json::json!({
                        "method": "item/agentMessage/delta",
                        "params": {
                            "threadId": thread_id,
                            "turnId": "overflow-turn",
                            "delta": delta,
                        },
                    })
                    .to_string(),
                    serde_json::json!({"id": id, "result": {"rateLimits": null}}).to_string(),
                ])
            }
            // The prompt stays active after accepting the interrupt, forcing the session's owned
            // shutdown path to reap the child rather than wait for a vendor terminal.
            Some("turn/interrupt") => Some(vec![
                serde_json::json!({"id": id, "result": {}}).to_string(),
            ]),
            _ => None,
        }
    }));
    let mut limits = replay_limits();
    limits.turn_channel_capacity = 1;
    let (host, _) = with_launcher_limits(Arc::clone(&launcher), None, limits);
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let mut lifecycle = session.subscribe();
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "keep the native prompt active"))
        .await
        .expect("expected an accepted native turn");

    assert!(matches!(
        turn.recv().await.map(|event| event.kind),
        Some(EventKind::TurnStarted { .. })
    ));
    notifications_released.store(true, Ordering::Release);
    session
        .refresh_account_usage()
        .await
        .expect("expected the first gate call to queue one unread payload");
    session
        .refresh_account_usage()
        .await
        .expect("expected the second gate call to release the overflow payload");
    tokio::task::yield_now().await;

    let first = turn
        .recv()
        .await
        .expect("expected the first unread payload");
    assert!(
        matches!(first.kind, EventKind::TextDelta { ref text } if text == "first unread payload"),
        "expected the first unread payload before the reserved terminal, received {first:?}"
    );
    let terminal = turn
        .recv()
        .await
        .expect("expected the reserved overflow terminal");
    assert!(matches!(
        terminal.kind,
        EventKind::Error { error } if error.code.as_str() == "stream-overflow"
    ));
    assert!(turn.recv().await.is_none(), "expected one terminal only");
    assert!(
        launcher
            .written()
            .iter()
            .any(|line| { line.contains("\"turn/interrupt\"") && line.contains("overflow-turn") }),
        "expected stream overflow to interrupt the still-active native prompt"
    );
    assert_eq!(
        status_once_settled(&mut lifecycle).await,
        SessionStatus::Closed,
        "expected overflow recovery to finish the session's owned teardown"
    );
    assert_eq!(
        launcher.live_children(),
        0,
        "expected overflow recovery to reap the app-server"
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

/// A server that accepts the turn it is asked to start and then immediately fails it.
///
/// The recording has no such conversation: every captured turn the server accepted, it also ran.
struct AcceptedThenFailedServer {
    thread_id: String,
    started: AtomicBool,
}

impl AcceptedThenFailedServer {
    const NATIVE_TURN_ID: &'static str = "accepted-then-failed";

    fn respond(&self, frame: &serde_json::Value) -> Option<Vec<String>> {
        let method = frame.get("method").and_then(serde_json::Value::as_str);
        if method != Some("turn/start") || self.started.swap(true, Ordering::SeqCst) {
            return None;
        }
        let id = frame.get("id").cloned().unwrap_or(serde_json::Value::Null);
        Some(vec![
            serde_json::json!({
                "id": id,
                "result": {"turn": {"id": Self::NATIVE_TURN_ID}},
            })
            .to_string(),
            serde_json::json!({
                "method": "turn/completed",
                "params": {
                    "threadId": self.thread_id,
                    "turn": {
                        "id": Self::NATIVE_TURN_ID,
                        "status": "failed",
                        "error": {"message": "upstream refused", "additionalDetails": "429"},
                    },
                },
            })
            .to_string(),
        ])
    }
}

/// A start the server accepted and then failed is not a start it refused.
///
/// A host's retry loop keys on that difference: `start_turn` returning `Ok` means the prompt
/// reached the vendor, so the failure that follows arrives on the stream and the dispatch stays
/// `Accepted` — replaying it is the host's decision, not a free one. The turn ends exactly once,
/// on the failure itself, and `terminal_status` reports it without the transcript being read.
#[tokio::test]
async fn a_turn_the_server_accepted_and_then_failed_ends_once_on_that_failure() {
    let transcript = Transcript::load("turn");
    let server = Arc::new(AcceptedThenFailedServer {
        thread_id: transcript
            .thread_id()
            .expect("expected the recording to name its thread"),
        started: AtomicBool::new(false),
    });
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(transcript.as_process_intercepting(move |frame| server.respond(frame)));
    let (host, _launcher) = with_launcher(Arc::clone(&launcher), None);
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");

    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "do something"))
        .await
        .expect("expected the server's acceptance before its failure");

    assert_eq!(
        turn.dispatch(),
        mango_external_agents::Dispatch::Accepted,
        "expected an accepted start to stay accepted through its failure"
    );
    assert_eq!(
        turn.native_turn_id(),
        AcceptedThenFailedServer::NATIVE_TURN_ID,
        "expected the accepted turn to carry the vendor's handle"
    );

    let events = drain(&mut turn).await;
    assert_eq!(
        events.len(),
        2,
        "expected the acceptance announcement and then the failure, received {events:#?}"
    );
    assert!(
        matches!(events[0], EventKind::TurnStarted { .. }),
        "expected the accepted start to be announced, received {:#?}",
        events[0]
    );
    assert!(
        matches!(
            &events[1],
            EventKind::Error { error }
                if error.code.as_str() == "codex-turn-failed"
                    && error.message.contains("upstream refused")
        ),
        "expected the vendor's failure to end the turn, received {:#?}",
        events[1]
    );
    assert!(
        matches!(
            turn.terminal_status(),
            Some(mango_external_agents::TerminalStatus::Failed { code })
                if code.as_str() == "codex-turn-failed"
        ),
        "expected the failure to be observable as the committed terminal, received {:?}",
        turn.terminal_status()
    );
    assert!(
        turn.recv().await.is_none(),
        "expected nothing after the turn's one terminal"
    );
}

/// A cancellation that raced a start has a reaper waiting for the start owner's admission slot.
/// An explicit refusal releases that slot, so the reaper must wake and leave a replacement alone.
#[tokio::test(start_paused = true)]
async fn a_cancelled_start_that_is_later_refused_does_not_shutdown_its_replacement() {
    let transcript = Transcript::load("turn");
    let server = Arc::new(DelayedFirstStartServer {
        answer: DelayedStartAnswer::Error,
        complete_before_answer: false,
        thread_id: transcript
            .thread_id()
            .expect("expected the recording to name its thread"),
        first_request_id: std::sync::Mutex::new(None),
        starts: AtomicUsize::new(0),
    });
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(transcript.as_process_intercepting(move |frame| server.respond(frame)));
    let mut limits = replay_limits();
    limits.shutdown_timeout = std::time::Duration::from_secs(1);
    let (host, _) = with_launcher_limits(Arc::clone(&launcher), None, limits);
    let session: Arc<dyn Session> = Arc::from(
        CodexHarness::new()
            .open_session(&host, OpenSession::new("chat-1"))
            .await
            .expect("expected a session"),
    );

    let first_session = Arc::clone(&session);
    let first = tokio::spawn(async move {
        first_session
            .start_turn(TurnRequest::new("turn-1", "one"))
            .await
    });
    wait_for_turn_start(&launcher).await;
    session
        .cancel(CancelReason::Requested)
        .await
        .expect("expected pre-acknowledgement cancellation to latch");
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    session
        .refresh_account_usage()
        .await
        .expect("expected the trigger request to release the held refusal");
    first
        .await
        .expect("expected the first start task")
        .expect_err("expected the held start refusal");

    let replacement = session
        .start_turn(TurnRequest::new("turn-2", "two"))
        .await
        .expect("expected the refused start to release admission");
    tokio::time::advance(limits.shutdown_timeout).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }

    assert!(
        replacement.terminal_status().is_none(),
        "expected the first owner's expired reaper not to cancel the replacement"
    );
    drop(replacement);
    session
        .close(CloseReason::Shutdown)
        .await
        .expect("expected cleanup after the ownership check");
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

    cancellation
        .await
        .expect("expected cancellation task to finish")
        .expect("expected bounded escalation to reap the silent native turn");
}

/// An unrecoverable terminal cannot merely fail its stream: the poisoned connection owns a
/// native child until its shutdown watcher reaps it.
#[tokio::test]
async fn an_unroutable_malformed_terminal_reaps_the_poisoned_session() {
    let transcript = Transcript::load("interrupt");
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(transcript.as_process_intercepting(move |frame| {
        let method = frame.get("method").and_then(serde_json::Value::as_str);
        let id = frame.get("id").cloned().unwrap_or(serde_json::Value::Null);
        (method == Some("turn/start")).then(|| {
            vec![
                serde_json::json!({
                    "id": id,
                    "result": {"turn": {"id": "malformed-turn"}},
                })
                .to_string(),
                serde_json::json!({
                    "method": "turn/completed",
                    "params": {"unexpected": true},
                })
                .to_string(),
            ]
        })
    }));
    let (host, launcher) = with_launcher(launcher, None);
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let mut lifecycle = session.subscribe();
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "one"))
        .await
        .expect("expected a turn before its malformed terminal");

    let event = turn
        .recv()
        .await
        .expect("expected malformed terminal to fail the stream");
    assert!(matches!(event.kind, EventKind::Error { .. }));
    assert_eq!(
        status_once_settled(&mut lifecycle).await,
        SessionStatus::Closed,
        "expected a poisoned session to finish its owned teardown"
    );
    assert_eq!(
        launcher.live_children(),
        0,
        "expected a malformed terminal to reap the app-server rather than only fail its stream"
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

/// Foreign traffic on the shared connection is not this turn's progress.
///
/// A subagent's thread, a detached review's and an account-level quota update all arrive on the
/// same pipe. Restarting the idle deadline for them let steady unrelated traffic keep a genuinely
/// silent turn alive for as long as the connection lasted, which is the one thing
/// `Limits::idle_timeout` exists to bound.
#[tokio::test(start_paused = true)]
async fn traffic_for_another_conversation_does_not_extend_this_turns_idle_deadline() {
    let transcript = Transcript::load("interrupt");
    let thread_id = transcript
        .thread_id()
        .expect("expected the recorded thread id");
    let announcer = Announcer::new();
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(
        transcript
            .as_process_intercepting(move |frame| {
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
            })
            .announcing(announcer.clone()),
    );
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

    // Another conversation speaking every three seconds against this turn's five second deadline.
    // Under `start_paused` the runtime advances to the nearest timer, so the noise task and the
    // idle watcher compete on virtual time exactly as two real peers would on wall-clock time.
    let noise = tokio::spawn({
        let announcer = announcer.clone();
        async move {
            for round in 0..40_u32 {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                announcer.announce(
                    serde_json::json!({
                        "method": "turn/started",
                        "params": {
                            "threadId": format!("detached-review-{round}"),
                            "turn": {"id": format!("detached-turn-{round}")},
                        },
                    })
                    .to_string(),
                );
            }
        }
    });

    // Bounded, so a deadline that never fires reads as a missing interrupt rather than as a hang.
    let mut interrupted = false;
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        if launcher
            .written()
            .iter()
            .any(|line| line.contains("\"turn/interrupt\"") && line.contains("idle-turn"))
        {
            interrupted = true;
            break;
        }
    }
    noise.abort();
    assert!(
        interrupted,
        "expected the silent turn's idle deadline to expire while another conversation talked, received {} frames and no interrupt",
        launcher.written().len()
    );

    let events = drain(&mut turn).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            EventKind::Cancelled {
                reason: CancelReason::Timeout
            }
        )),
        "expected the expired deadline to name its timeout reason, received {events:#?}"
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

/// A host shutdown resolves an open question round exactly once, like a turn cancellation does.
///
/// `release_pending_for` sends `QuestionAnswer::Cancelled { reported: true }` on the pending
/// entry's oneshot ahead of a shutdown, and the waiter's `select!` is biased toward `waiting`, so
/// it takes that oneshot arm rather than the nobody-answered `None` arm; the `reported` bit is
/// what stops it from publishing a second time. Unlike the unit test for this path, the schedule
/// here is not forced — it exercises the same exactly-once guarantee under whichever ordering the
/// real runtime picks.
#[tokio::test]
async fn host_shutdown_resolves_an_open_question_round_exactly_once() {
    let transcript = Transcript::load("turn");
    let thread_id = transcript
        .thread_id()
        .expect("expected the turn recording to name its thread");
    let push = MidTurnPush::new(
        thread_id.clone(),
        "item/tool/requestUserInput",
        serde_json::json!({
            "threadId": thread_id,
            "turnId": MID_TURN_PUSH_NATIVE_TURN_ID,
            "itemId": "ask-1",
            "isBlocking": false,
            "questions": [{"id": "note", "header": "Note", "question": "Anything else?"}],
        }),
    );
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(transcript.as_process_intercepting(move |frame| push.respond(frame)));
    let cancel = mango_external_agents::CancelToken::new();
    let (host, _launcher) = with_launcher_limits_and_cancel(
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
        .start_turn(TurnRequest::new("turn-1", "do something"))
        .await
        .expect("expected a turn");
    await_question(&mut turn).await;

    cancel.cancel();
    let events = drain(&mut turn).await;

    let resolutions = events
        .iter()
        .filter(|kind| matches!(kind, EventKind::QuestionResolved { .. }))
        .count();
    assert_eq!(
        resolutions, 1,
        "expected exactly one resolution on shutdown, received {events:#?}"
    );
    assert!(
        events.iter().any(|kind| matches!(
            kind,
            EventKind::QuestionResolved {
                outcome: QuestionOutcome::Cancelled,
                ..
            }
        )),
        "expected shutdown to record the round as cancelled, received {events:#?}"
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
                                "cwd": workspace_path().to_string_lossy(),
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

#[tokio::test]
async fn listing_without_a_user_conversation_uses_only_the_authorized_workspace() {
    let (host, launcher) = host_replaying(&["handshake"]);
    let page = CodexHarness::new()
        .list_sessions(&host, SessionQuery::default())
        .await
        .expect("expected a picker page before any conversation is open");

    assert!(!page.sessions.is_empty());
    assert!(page.next_cursor.is_some());
    let written = launcher.written();
    assert!(written.iter().any(|line| line.contains("thread/list")));
    assert!(!written.iter().any(|line| line.contains("thread/start")));
    let list = written
        .iter()
        .find(|line| line.contains("thread/list"))
        .expect("expected a list request");
    let frame: serde_json::Value = serde_json::from_str(list).expect("expected a JSON request");
    assert_eq!(
        frame["params"]["cwd"],
        workspace_path().to_string_lossy().into_owned()
    );
    assert_eq!(frame["params"]["limit"], 50);
}

/// A server that identifies an app-server older than the pinned protocol at initialization.
struct OldProbeHandshakeServer;

impl OldProbeHandshakeServer {
    fn process(self) -> FakeProcess {
        Transcript::load("handshake").as_process_intercepting(|frame| {
            (frame["method"] == "initialize").then(|| {
                vec![
                    serde_json::json!({
                        "id": frame["id"],
                        "result": {
                            "userAgent": "codex/0.147.0 (Linux)",
                            "platformOs": "linux",
                        },
                    })
                    .to_string(),
                ]
            })
        })
    }
}

#[tokio::test]
async fn picker_refuses_an_app_server_below_the_pinned_handshake_version() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(OldProbeHandshakeServer.process());
    let (host, _) = with_launcher(Arc::clone(&launcher), None);

    let result = CodexHarness::new()
        .list_sessions(&host, SessionQuery::default())
        .await;

    assert!(
        matches!(
            result,
            Err(ref error)
                if matches!(
                    error.cause(),
                    mango_external_agents::Error::VersionGate {
                        found,
                        minimum,
                    } if found == "0.147.0"
                        && minimum == mango_agent_codex::MINIMUM_CODEX_VERSION
                )
        ),
        "expected the handshake version gate, received {result:?}"
    );
    assert!(
        !launcher
            .written()
            .iter()
            .any(|line| line.contains("thread/list")),
        "expected no picker request after the version gate"
    );
    assert_eq!(
        launcher.live_children(),
        0,
        "expected the probe child reaped"
    );
}

/// A fake list answer that includes rows the host never authorized.
struct MixedWorkspaceListServer;

impl MixedWorkspaceListServer {
    fn process(self) -> FakeProcess {
        Transcript::load("handshake").as_process_intercepting(|frame| {
            (frame["method"] == "thread/list").then(|| {
                vec![
                    serde_json::json!({
                        "id": frame["id"],
                        "result": {
                            "data": [
                                {"id": "local", "cwd": frame["params"]["cwd"], "preview": "allowed"},
                                {"id": "foreign", "cwd": "/private", "preview": "secret"},
                                {"id": "unscoped", "preview": "unknown"},
                            ],
                            "nextCursor": null,
                        },
                    })
                    .to_string(),
                ]
            })
        })
    }
}

/// A picker response that ignored the page size the host supplied.
struct OversizedListServer;

impl OversizedListServer {
    fn process(self) -> FakeProcess {
        Transcript::load("handshake").as_process_intercepting(|frame| {
            (frame["method"] == "thread/list").then(|| {
                vec![
                    serde_json::json!({
                        "id": frame["id"],
                        "result": {
                            "data": [
                                {"id": "first", "cwd": frame["params"]["cwd"]},
                                {"id": "second", "cwd": frame["params"]["cwd"]},
                            ],
                            "nextCursor": "after-second",
                        },
                    })
                    .to_string(),
                ]
            })
        })
    }
}

#[tokio::test]
async fn listing_refuses_a_vendor_page_that_exceeds_the_requested_limit() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(OversizedListServer.process());
    let (host, _) = with_launcher(Arc::clone(&launcher), None);

    let result = CodexHarness::new()
        .list_sessions(
            &host,
            SessionQuery {
                limit: Some(1),
                ..SessionQuery::default()
            },
        )
        .await;

    assert!(
        matches!(
            result,
            Err(ref error)
                if matches!(
                    error.cause(),
                    mango_external_agents::Error::LimitExceeded {
                        subject: "sessions in a Codex thread/list response",
                        limit: 1,
                        received: 2,
                    }
                )
        ),
        "expected an over-limit response refusal, received {result:?}"
    );
    assert_eq!(
        launcher.live_children(),
        0,
        "expected the picker child reaped"
    );
}

#[tokio::test]
async fn listing_discards_rows_outside_the_authorized_workspace() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(MixedWorkspaceListServer.process());
    let (host, _) = with_launcher(launcher, None);
    let page = CodexHarness::new()
        .list_sessions(&host, SessionQuery::default())
        .await
        .expect("expected a filtered picker page");
    assert_eq!(page.sessions.len(), 1);
    assert_eq!(page.sessions[0].native_session_id, "local");
    assert_eq!(page.sessions[0].preview.as_deref(), Some("allowed"));
}

/// A responsive app-server that leaves the picker request unanswered.
struct UnansweredListServer;

impl UnansweredListServer {
    fn process(self) -> FakeProcess {
        Transcript::load("handshake")
            .as_process_intercepting(|frame| (frame["method"] == "thread/list").then(Vec::new))
    }
}

#[tokio::test]
async fn canceling_a_picker_request_reaps_its_probe_child() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(UnansweredListServer.process());
    let (host, _) = with_launcher(Arc::clone(&launcher), None);
    let task = tokio::spawn(async move {
        CodexHarness::new()
            .list_sessions(&host, SessionQuery::default())
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if launcher
                .written()
                .iter()
                .any(|line| line.contains("thread/list"))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected the picker request to reach the fake app-server");
    assert_eq!(launcher.live_children(), 1);
    task.abort();
    let _ = task.await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if launcher.live_children() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected cancellation to kill the probe child");
}

#[tokio::test]
async fn listing_a_different_workspace_is_refused_before_spawning() {
    let (host, launcher) = host_replaying(&[]);
    let query = SessionQuery {
        workspace_path: Some("/another-workspace".into()),
        ..SessionQuery::default()
    };
    let result = CodexHarness::new().list_sessions(&host, query).await;
    assert!(
        result.is_err(),
        "expected a workspace authorization refusal"
    );
    assert!(launcher.launches().is_empty(), "expected no child process");
}

/// Codex receives the host working directory in every child launch and thread request. A relative
/// or dotted path would let the child resolve it against ambient state, so reject it before any
/// Codex child starts.
#[tokio::test]
async fn a_non_absolute_or_lexically_non_normalized_workspace_is_refused_before_spawning_codex() {
    for cwd in [
        std::path::PathBuf::from("relative-workspace"),
        std::env::temp_dir()
            .join("mango-agent-codex")
            .join("..")
            .join("workspace"),
    ] {
        let launcher = Arc::new(FakeLauncher::new());
        let host = HostContext::builder()
            .launcher(launcher.clone())
            .cwd(cwd)
            .client_info("mango-test", "0.0.1")
            .environment(EnvSource::from_pairs([("PATH", "/usr/bin")]))
            .build()
            .expect("expected a host context that leaves Codex validation to the harness");

        let results = [
            CodexHarness::new()
                .open_session(&host, OpenSession::new("chat-1"))
                .await
                .map(|_| ()),
            CodexHarness::new()
                .list_sessions(&host, SessionQuery::default())
                .await
                .map(|_| ()),
            CodexHarness::new().discover(&host).await.map(|_| ()),
        ];
        for result in results {
            assert!(
                matches!(result, Err(ref error) if matches!(error.cause(), mango_external_agents::Error::HostConfiguration { expected: "an absolute, lexically normalized UTF-8 workspace path", .. })),
                "expected a typed workspace refusal, received {result:?}"
            );
        }
        assert!(
            launcher.launches().is_empty(),
            "expected no Codex child for an unauthorized workspace"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_non_utf8_workspace_is_refused_before_opening_or_listing_codex() {
    use std::os::unix::ffi::OsStringExt;

    let launcher = Arc::new(FakeLauncher::new());
    let cwd = std::path::PathBuf::from(std::ffi::OsString::from_vec(b"/workspace/\xff".to_vec()));
    let host = HostContext::builder()
        .launcher(launcher.clone())
        .cwd(cwd)
        .client_info("mango-test", "0.0.1")
        .environment(EnvSource::from_pairs([("PATH", "/usr/bin")]))
        .build()
        .expect("expected a host with an opaque Unix path");

    for result in [
        CodexHarness::new()
            .open_session(&host, OpenSession::new("chat-1"))
            .await
            .map(|_| ()),
        CodexHarness::new()
            .list_sessions(&host, SessionQuery::default())
            .await
            .map(|_| ()),
    ] {
        assert!(
            matches!(result, Err(error) if matches!(error.cause(), mango_external_agents::Error::HostConfiguration { expected: "an absolute, lexically normalized UTF-8 workspace path", .. }))
        );
    }
    assert!(launcher.launches().is_empty());
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
    assert_eq!(
        frame["params"]["cwd"],
        workspace_path().to_string_lossy().into_owned()
    );
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
        .cwd(workspace_path())
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

#[tokio::test]
async fn a_probe_cleanup_failure_reaches_discovery_with_its_host_control() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(version_answer());
    launcher.push(FakeProcess::responding(|_| Vec::new()));
    let host = HostContext::builder()
        .launcher(Arc::new(CleanupRequiredProbeLauncher {
            inner: Arc::clone(&launcher),
        }))
        .cwd(workspace_path())
        .client_info("mango-test", "0.0.1")
        .build()
        .expect("expected a host");

    let error = CodexHarness::new()
        .discover(&host)
        .await
        .expect_err("expected failed app-server cleanup to reach discovery");
    assert!(
        matches!(error, mango_external_agents::Error::CleanupRequired { .. }),
        "expected cleanup-required rather than unknown discovery, received {error:?}"
    );
    assert!(
        error.cleanup_control().is_some(),
        "expected discovery to retain the host cleanup control"
    );
}

/// An old build reports the gate verdict rather than crashing, and claims no capabilities.
#[tokio::test]
async fn an_older_codex_on_the_path_reports_the_gate_verdict() {
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(FakeProcess::transcript(["codex-cli 0.147.0"]));
    let host = HostContext::builder()
        .launcher(launcher.clone())
        .cwd(workspace_path())
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
    // Once Codex declares `Capability::Questions` the suite starts a third turn of its own —
    // `check_questions`, between `check_turn` and `check_cancelled_turn` — so `turn/start` calls
    // are counted by exact position rather than "first or not": the second now belongs to the
    // question round, and only the third gets the old synthetic cancel id.
    const CONFORMANCE_QUESTION_REQUEST_ID: i64 = 77_001;
    let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let responder_starts = Arc::clone(&starts);
    let question_thread_id = thread_id.clone();
    launcher.push(transcript.as_process_intercepting(move |frame| {
        let method = frame.get("method").and_then(serde_json::Value::as_str);
        let id = frame.get("id").cloned().unwrap_or(serde_json::Value::Null);
        match method {
            Some("turn/start") => {
                match responder_starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                    // `check_turn`: let the real recorded fixture answer.
                    0 => None,
                    // `check_questions`: a round-trip for the suite's own `check_questions` to
                    // exercise, since a required capability covered only by a skip does not count.
                    1 => Some(vec![
                        serde_json::json!({
                            "id": id,
                            "result": {"turn": {"id": "conformance-question"}},
                        })
                        .to_string(),
                        serde_json::json!({
                            "id": CONFORMANCE_QUESTION_REQUEST_ID,
                            "method": "item/tool/requestUserInput",
                            "params": {
                                "threadId": question_thread_id,
                                "turnId": "conformance-question",
                                "itemId": "conformance-question-item",
                                "isBlocking": false,
                                "questions": [{
                                    "id": "conformance-question-1",
                                    "header": "Conformance",
                                    "question": "Anything to add?",
                                }],
                            },
                        })
                        .to_string(),
                    ]),
                    // `check_cancelled_turn`: the existing synthetic cancel id.
                    _ => Some(vec![
                        serde_json::json!({
                            "id": id,
                            "result": {"turn": {"id": "conformance-cancel"}},
                        })
                        .to_string(),
                    ]),
                }
            }
            // The client's answer to the pushed question: complete that turn so `check_questions`
            // sees a terminal rather than waiting out its own timeout.
            None if frame.get("id")
                == Some(&serde_json::json!(CONFORMANCE_QUESTION_REQUEST_ID)) =>
            {
                Some(vec![
                    serde_json::json!({
                        "method": "turn/completed",
                        "params": {
                            "threadId": question_thread_id,
                            "turn": {"id": "conformance-question", "status": "completed"},
                        },
                    })
                    .to_string(),
                ])
            }
            // Named by whichever turn is actually being interrupted: `check_questions` answers
            // and cancels back to back, so its own turn can still be active when this arrives,
            // racing the completion its answer already triggered.
            Some("turn/interrupt") => {
                let interrupted_turn_id = frame
                    .pointer("/params/turnId")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("conformance-cancel")
                    .to_owned();
                Some(vec![
                    serde_json::json!({"id": id, "result": {}}).to_string(),
                    serde_json::json!({
                        "method": "turn/completed",
                        "params": {
                            "threadId": thread_id,
                            "turn": {"id": interrupted_turn_id, "status": "interrupted"},
                        },
                    })
                    .to_string(),
                ])
            }
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
    // A declared capability covered only by a skip is a capability nothing exercised. The
    // recorded conversation pushes one `item/tool/requestUserInput` for exactly this reason.
    assert!(
        report
            .skipped()
            .iter()
            .all(|check| !check.name.contains("question")),
        "expected the question round trip to run rather than skip, received {:?}",
        report.skipped()
    );
}

/// Invalid host MCP requests fail before either a fresh or resumed Codex session launches.
#[tokio::test]
async fn invalid_host_mcp_servers_are_refused_before_spawning_codex() {
    for mode in [
        None,
        Some(mango_external_agents::ResumeMode::Strict),
        Some(mango_external_agents::ResumeMode::Fallback),
    ] {
        let (host, launcher) = host_replaying(&["handshake"]);
        let request = OpenSession::new("chat-1").with_mcp_servers(vec![
            McpServer::stdio("docs", "docs-mcp"),
            McpServer::stdio("docs", "another-mcp"),
        ]);

        let request = match mode {
            Some(mode) => request.resuming("existing-thread", mode),
            None => request,
        };
        let result = CodexHarness::new().open_session(&host, request).await;

        assert_eq!(
            launcher.launches().len(),
            0,
            "expected duplicate MCP names to be refused before spawning Codex"
        );
        let error = match result {
            Ok(_) => panic!("expected the typed MCP configuration refusal"),
            Err(error) => error,
        };
        assert!(
            matches!(
                error.cause(),
                mango_external_agents::Error::HostConfiguration {
                    expected: "unique MCP server names",
                    received,
                } if received == "duplicate MCP server at index 1"
            ),
            "expected the typed MCP configuration refusal"
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
        0,
        "expected the refusal before launching the vendor"
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

/// The synthetic `id` every [`MidTurnPush`] raises its request under.
///
/// Fixed rather than random: every test using [`open_with_mid_turn_push`] owns its own session and
/// launcher, so nothing else on the wire can collide with it.
const MID_TURN_PUSH_REQUEST_ID: i64 = 90_210;

/// The native turn id [`MidTurnPush`] answers `turn/start` with.
const MID_TURN_PUSH_NATIVE_TURN_ID: &str = "vendor-turn-1";

/// A server that answers `turn/start` immediately, pushes one out-of-band request right after,
/// and completes the turn once that request's answer arrives.
///
/// The three server-request families this drives landed after the fixtures under
/// `fixtures/codex/` were captured, so there is no real transcript to replay one from. Writing one
/// by hand would put words in the app-server's mouth — `support::Transcript::as_process_intercepting`
/// says the same — so this only shapes the wire the way a real mid-turn approval already does (see
/// the `approval` fixture): the push rides the `turn/start` response, and the reply completes the
/// turn.
struct MidTurnPush {
    thread_id: String,
    method: &'static str,
    params: serde_json::Value,
    pushed: AtomicBool,
    answered: AtomicBool,
}

impl MidTurnPush {
    fn new(thread_id: String, method: &'static str, params: serde_json::Value) -> Self {
        Self {
            thread_id,
            method,
            params,
            pushed: AtomicBool::new(false),
            answered: AtomicBool::new(false),
        }
    }

    fn respond(&self, frame: &serde_json::Value) -> Option<Vec<String>> {
        let method = frame.get("method").and_then(serde_json::Value::as_str);
        if method == Some("turn/start") && !self.pushed.swap(true, Ordering::SeqCst) {
            let id = frame.get("id").cloned().unwrap_or(serde_json::Value::Null);
            return Some(vec![
                serde_json::json!({
                    "id": id,
                    "result": {"turn": {"id": MID_TURN_PUSH_NATIVE_TURN_ID}},
                })
                .to_string(),
                serde_json::json!({
                    "id": MID_TURN_PUSH_REQUEST_ID,
                    "method": self.method,
                    "params": self.params,
                })
                .to_string(),
            ]);
        }
        if method == Some("turn/interrupt") && !self.answered.swap(true, Ordering::SeqCst) {
            let id = frame.get("id").cloned().unwrap_or(serde_json::Value::Null);
            return Some(vec![
                serde_json::json!({"id": id, "result": {}}).to_string(),
                serde_json::json!({
                    "method": "turn/completed",
                    "params": {
                        "threadId": self.thread_id,
                        "turn": {"id": MID_TURN_PUSH_NATIVE_TURN_ID, "status": "interrupted"},
                    },
                })
                .to_string(),
            ]);
        }
        if method.is_none()
            && frame.get("id") == Some(&serde_json::json!(MID_TURN_PUSH_REQUEST_ID))
            && !self.answered.swap(true, Ordering::SeqCst)
        {
            return Some(vec![
                serde_json::json!({
                    "method": "turn/completed",
                    "params": {
                        "threadId": self.thread_id,
                        "turn": {"id": MID_TURN_PUSH_NATIVE_TURN_ID, "status": "completed"},
                    },
                })
                .to_string(),
            ]);
        }
        None
    }
}

/// Opens a session and starts a turn against a server that pushes one out-of-band request.
///
/// `build_params` receives the thread and native turn id the push will use, so a test can write a
/// request whose own `threadId`/`turnId` match what the session will actually see.
async fn open_with_mid_turn_push_and_clock(
    method: &'static str,
    build_params: impl FnOnce(&str, &str) -> serde_json::Value,
    clock: Option<Arc<dyn Clock>>,
) -> (
    Box<dyn Session>,
    Arc<FakeLauncher>,
    mango_external_agents::TurnStream,
) {
    let transcript = Transcript::load("turn");
    let thread_id = transcript
        .thread_id()
        .expect("expected the turn recording to name its thread");
    let params = build_params(&thread_id, MID_TURN_PUSH_NATIVE_TURN_ID);
    let push = MidTurnPush::new(thread_id, method, params);
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(transcript.as_process_intercepting(move |frame| push.respond(frame)));
    let (host, launcher) = match clock {
        Some(clock) => with_launcher_and_clock(launcher, None, clock),
        None => with_launcher(launcher, None),
    };
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    let turn = session
        .start_turn(TurnRequest::new("turn-1", "do something"))
        .await
        .expect("expected a turn");
    (session, launcher, turn)
}

/// The common case of [`open_with_mid_turn_push_and_clock`]: the real clock.
async fn open_with_mid_turn_push(
    method: &'static str,
    build_params: impl FnOnce(&str, &str) -> serde_json::Value,
) -> (
    Box<dyn Session>,
    Arc<FakeLauncher>,
    mango_external_agents::TurnStream,
) {
    open_with_mid_turn_push_and_clock(method, build_params, None).await
}

/// Reads a turn until the vendor asks a round of questions.
async fn await_question(
    turn: &mut mango_external_agents::TurnStream,
) -> mango_external_agents::QuestionRequest {
    let asked = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(event) = turn.recv().await {
            if let EventKind::QuestionAsked { request } = event.kind {
                return Some(request);
            }
        }
        None
    })
    .await
    .expect("expected the question round within the deadline");
    asked.expect("expected the question round to reach the host")
}

/// The wire frame this harness sent in answer to a [`MidTurnPush`]'s request.
fn mid_turn_push_answer(launcher: &FakeLauncher) -> serde_json::Value {
    let line = launcher
        .written()
        .into_iter()
        .find(|line| {
            let Ok(frame) = serde_json::from_str::<serde_json::Value>(line) else {
                return false;
            };
            frame.get("method").is_none()
                && frame.get("id") == Some(&serde_json::json!(MID_TURN_PUSH_REQUEST_ID))
        })
        .unwrap_or_else(|| panic!("expected an answer to the pushed request, received none"));
    serde_json::from_str(&line).expect("expected a JSON frame")
}

/// An MCP elicitation is an arbitrary form; this library renders none, so it is declined natively
/// and no part of it ever reaches the host.
#[tokio::test]
async fn an_elicitation_is_declined_natively_and_never_put_to_the_host() {
    let (_session, launcher, mut turn) =
        open_with_mid_turn_push("mcpServer/elicitation/request", |thread_id, turn_id| {
            serde_json::json!({
                "threadId": thread_id,
                "turnId": turn_id,
                "elicitationId": "elicit-1",
                "message": "enter your api token",
                "requestedSchema": {"type": "object"},
            })
        })
        .await;

    let events = drain(&mut turn).await;
    assert!(
        !events
            .iter()
            .any(|kind| matches!(kind, EventKind::QuestionAsked { .. })),
        "expected a form never to be put to the host, received {events:#?}"
    );
    assert!(
        events.iter().any(|kind| matches!(
            kind,
            EventKind::QuestionResolved {
                outcome: QuestionOutcome::Refused {
                    reason: UnsupportedQuestion::ArbitraryForm
                },
                ..
            }
        )),
        "expected a native refusal to be recorded, received {events:#?}"
    );

    let answer = mid_turn_push_answer(&launcher);
    assert_eq!(answer["result"], serde_json::json!({"action": "decline"}));
}

/// A url-mode elicitation carries no `turnId` of its own at this pin, and is still declined.
///
/// The correlation gate every other server request passes through would answer this one with the
/// JSON-RPC error the native decline exists to stop sending, and leave the app-server told that
/// this client is broken rather than that the form was refused.
#[tokio::test]
async fn a_url_mode_elicitation_with_no_turn_id_is_still_declined_natively() {
    let (_session, launcher, mut turn) =
        open_with_mid_turn_push("mcpServer/elicitation/request", |thread_id, _turn_id| {
            serde_json::json!({
                "threadId": thread_id,
                "mode": "url",
                "elicitationId": "elicit-1",
                "message": "open this",
                "url": "https://example.test/form",
            })
        })
        .await;

    let events = drain(&mut turn).await;
    let resolutions = events
        .iter()
        .filter(|kind| {
            matches!(
                kind,
                EventKind::QuestionResolved {
                    outcome: QuestionOutcome::Refused {
                        reason: UnsupportedQuestion::ArbitraryForm
                    },
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        resolutions, 1,
        "expected the form refused exactly once on the active turn, received {events:#?}"
    );

    let answer = mid_turn_push_answer(&launcher);
    assert_eq!(
        answer.get("error"),
        None,
        "expected a native decline rather than a JSON-RPC error, received {answer}"
    );
    assert_eq!(answer["result"], serde_json::json!({"action": "decline"}));
}

/// A `turnId` the server writes as `null` is a missing correlation, not a malformed frame.
#[tokio::test]
async fn an_elicitation_whose_turn_id_is_null_is_declined_rather_than_refused() {
    let (_session, launcher, mut turn) =
        open_with_mid_turn_push("mcpServer/elicitation/request", |thread_id, _turn_id| {
            serde_json::json!({
                "threadId": thread_id,
                "turnId": serde_json::Value::Null,
                "message": "enter your api token",
                "requestedSchema": {"type": "object"},
            })
        })
        .await;

    let events = drain(&mut turn).await;
    assert!(
        events.iter().any(|kind| matches!(
            kind,
            EventKind::QuestionResolved {
                outcome: QuestionOutcome::Refused {
                    reason: UnsupportedQuestion::ArbitraryForm
                },
                ..
            }
        )),
        "expected a native refusal to be recorded, received {events:#?}"
    );

    let answer = mid_turn_push_answer(&launcher);
    assert_eq!(
        answer.get("error"),
        None,
        "expected a native decline rather than a JSON-RPC error, received {answer}"
    );
    assert_eq!(answer["result"], serde_json::json!({"action": "decline"}));
}

/// Each of the three permission decisions round-trips with its own scope: a turn grant, a session
/// grant, and a denial that grants nothing.
#[tokio::test]
async fn a_permissions_decision_round_trips_its_scope() {
    for (option_id, expected_wire) in [
        (
            "grant:turn",
            serde_json::json!({"permissions": {"fs": {"read": true}}, "scope": "turn"}),
        ),
        (
            "grant:session",
            serde_json::json!({"permissions": {"fs": {"read": true}}, "scope": "session"}),
        ),
        ("deny", serde_json::json!({"permissions": {}})),
    ] {
        let (session, launcher, mut turn) =
            open_with_mid_turn_push("item/permissions/requestApproval", |thread_id, turn_id| {
                serde_json::json!({
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "itemId": "perm-1",
                    "cwd": "/workspace",
                    "reason": "needs filesystem access",
                    "permissions": {"fs": {"read": true}},
                })
            })
            .await;

        let request = await_approval(&mut turn).await;
        // The authority a grant hands over is rendered before anybody can grant it: a host or a
        // broker choosing `grant:turn` off the title and the agent's own reason alone would be
        // granting a profile it was never shown.
        let detail = request
            .detail
            .as_deref()
            .expect("expected the requested profile in the detail");
        assert!(
            detail.contains(r#"{"fs":{"read":true}}"#),
            "expected the requested profile in the detail, received {detail:?}"
        );
        let response = request
            .respond(option_id, DecisionSource::User)
            .unwrap_or_else(|_| panic!("expected {option_id} among the offered options"));
        session
            .respond(response)
            .await
            .expect("expected the decision to land");
        let events = drain(&mut turn).await;
        assert!(
            events.iter().any(|kind| matches!(
                kind,
                EventKind::ApprovalResolved { decision, .. } if decision.option_id == option_id
            )),
            "expected {option_id} to be recorded, received {events:#?}"
        );

        let answer = mid_turn_push_answer(&launcher);
        assert_eq!(
            answer["result"], expected_wire,
            "expected {option_id} to echo {expected_wire}, received {answer}"
        );
    }
}

/// A round of three questions: a choice round-tripping its native label, a free-text answer and a
/// decline — all in one round trip, the way a vendor asking three things at once is answered.
#[tokio::test]
async fn a_question_round_answers_a_choice_free_text_and_a_decline() {
    let (session, launcher, mut turn) =
        open_with_mid_turn_push("item/tool/requestUserInput", |thread_id, turn_id| {
            serde_json::json!({
                "threadId": thread_id,
                "turnId": turn_id,
                "itemId": "ask-1",
                // Not blocking: the round offers a decline, and a required question refuses one.
                "isBlocking": false,
                "questions": [
                    {
                        "id": "branch",
                        "header": "Branch",
                        "question": "Which branch?",
                        "options": [
                            {"label": "main"},
                            {"label": "next", "description": "the other one"},
                        ],
                    },
                    {"id": "note", "header": "Note", "question": "Anything else?"},
                    {"id": "extra", "header": "Extra", "question": "Want more?"},
                ],
            })
        })
        .await;

    let request = await_question(&mut turn).await;
    assert_eq!(request.questions.len(), 3);
    assert!(request.questions.iter().all(|question| !question.required));
    let branch = request
        .question(&QuestionId::new("branch"))
        .expect("expected the branch question");
    assert!(
        matches!(
            &branch.form,
            mango_external_agents::QuestionForm::Choice { options, .. }
                if options.iter().any(|option| option.id == QuestionOptionId::new("next"))
        ),
        "expected the vendor's own label as the option's native id, received {:?}",
        branch.form
    );

    let response = QuestionResponse::new(
        request.interaction.id.clone(),
        vec![
            QuestionAnswerValue::new(
                QuestionId::new("branch"),
                AnswerValue::chosen(QuestionOptionId::new("next")),
            ),
            QuestionAnswerValue::new(QuestionId::new("note"), AnswerValue::text("ship it")),
            QuestionAnswerValue::new(QuestionId::new("extra"), AnswerValue::Declined),
        ],
    );
    session
        .answer(response)
        .await
        .expect("expected the answer to land");

    let events = drain(&mut turn).await;
    assert!(
        events.iter().any(|kind| matches!(
            kind,
            EventKind::QuestionResolved {
                outcome: QuestionOutcome::Answered { .. },
                ..
            }
        )),
        "expected the round resolved as answered, received {events:#?}"
    );

    let answer = mid_turn_push_answer(&launcher);
    assert_eq!(
        answer["result"],
        serde_json::json!({"answers": {
            "branch": {"answers": ["next"]},
            "note": {"answers": ["ship it"]},
            "extra": {"answers": []},
        }})
    );
}

/// `required` is a round-level fact on the wire (`isBlocking`), not a per-question one, so every
/// question in a blocking round carries it.
#[tokio::test]
async fn a_blocking_round_marks_every_question_required() {
    let (session, _launcher, mut turn) =
        open_with_mid_turn_push("item/tool/requestUserInput", |thread_id, turn_id| {
            serde_json::json!({
                "threadId": thread_id,
                "turnId": turn_id,
                "itemId": "ask-1",
                "isBlocking": true,
                "questions": [
                    {"id": "note", "header": "Note", "question": "Anything else?"},
                ],
            })
        })
        .await;

    let request = await_question(&mut turn).await;
    assert!(request.questions.iter().all(|question| question.required));

    session
        .answer(QuestionResponse::new(
            request.interaction.id.clone(),
            vec![QuestionAnswerValue::new(
                QuestionId::new("note"),
                AnswerValue::text("done"),
            )],
        ))
        .await
        .expect("expected the answer to land");
    drain(&mut turn).await;
}

/// A host cannot invent a choice: an answer naming an option the round never offered is refused
/// before anything reaches the vendor, and the round stays open for a corrected answer.
#[tokio::test]
async fn an_invalid_answer_is_refused_before_reaching_the_wire() {
    let (session, launcher, mut turn) =
        open_with_mid_turn_push("item/tool/requestUserInput", |thread_id, turn_id| {
            serde_json::json!({
                "threadId": thread_id,
                "turnId": turn_id,
                "itemId": "ask-1",
                "isBlocking": false,
                "questions": [
                    {
                        "id": "branch",
                        "header": "Branch",
                        "question": "Which branch?",
                        "options": [{"label": "main"}],
                    },
                ],
            })
        })
        .await;

    let request = await_question(&mut turn).await;
    let before = launcher.written().len();

    let error = session
        .answer(QuestionResponse::new(
            request.interaction.id.clone(),
            vec![QuestionAnswerValue::new(
                QuestionId::new("branch"),
                AnswerValue::chosen(QuestionOptionId::new("trunk")),
            )],
        ))
        .await
        .expect_err("expected an option the round never offered to be refused");
    assert!(
        matches!(error.cause(), mango_external_agents::Error::Protocol { .. }),
        "expected a protocol refusal, received {error:?}"
    );
    assert_eq!(
        launcher.written().len(),
        before,
        "expected nothing to reach the vendor for an answer refused before the wire"
    );

    // The round is still open: a corrected answer still lands.
    session
        .answer(QuestionResponse::new(
            request.interaction.id.clone(),
            vec![QuestionAnswerValue::new(
                QuestionId::new("branch"),
                AnswerValue::chosen(QuestionOptionId::new("main")),
            )],
        ))
        .await
        .expect("expected the corrected answer to land");
    drain(&mut turn).await;
}

/// A round with any question marked `isSecret` is refused whole and natively: no part of it, not
/// even the questions that were not secret, ever reaches the host.
#[tokio::test]
async fn a_secret_question_refuses_the_whole_round_natively() {
    let (_session, launcher, mut turn) =
        open_with_mid_turn_push("item/tool/requestUserInput", |thread_id, turn_id| {
            serde_json::json!({
                "threadId": thread_id,
                "turnId": turn_id,
                "itemId": "ask-1",
                "isBlocking": true,
                "questions": [
                    {"id": "note", "header": "Note", "question": "Say more?"},
                    {
                        "id": "token",
                        "header": "Token",
                        "question": "What is your API token?",
                        "isSecret": true,
                    },
                ],
            })
        })
        .await;

    let events = drain(&mut turn).await;
    assert!(
        !events
            .iter()
            .any(|kind| matches!(kind, EventKind::QuestionAsked { .. })),
        "expected no part of a secret round to reach the host, received {events:#?}"
    );
    assert!(
        events.iter().any(|kind| matches!(
            kind,
            EventKind::QuestionResolved {
                outcome: QuestionOutcome::Refused {
                    reason: UnsupportedQuestion::SecretCollection
                },
                ..
            }
        )),
        "expected a native refusal to be recorded, received {events:#?}"
    );

    let answer = mid_turn_push_answer(&launcher);
    assert_eq!(answer["result"], serde_json::json!({"answers": {}}));
}

/// Cancelling the turn a question round belongs to resolves the round before the turn's own
/// terminal, never after: a host reading events in order must see the round settled first.
#[tokio::test]
async fn cancelling_the_turn_resolves_its_open_question_round_first() {
    let (session, launcher, mut turn) =
        open_with_mid_turn_push("item/tool/requestUserInput", |thread_id, turn_id| {
            serde_json::json!({
                "threadId": thread_id,
                "turnId": turn_id,
                "itemId": "ask-1",
                "isBlocking": false,
                "questions": [{"id": "note", "header": "Note", "question": "Anything else?"}],
            })
        })
        .await;

    await_question(&mut turn).await;
    session
        .cancel(CancelReason::Requested)
        .await
        .expect("expected the cancellation to land");
    let events = drain(&mut turn).await;

    let resolved_at = events.iter().position(|kind| {
        matches!(
            kind,
            EventKind::QuestionResolved {
                outcome: QuestionOutcome::Cancelled,
                ..
            }
        )
    });
    let terminal_at = events
        .iter()
        .position(|kind| matches!(kind, EventKind::Completed));
    assert!(
        resolved_at.is_some() && terminal_at.is_some() && resolved_at < terminal_at,
        "expected the round cancelled before the turn's terminal, received {events:#?}"
    );

    // Exactly one, on the same terms as the expiry: the canceller reports the round and the
    // waiter it wakes reports nothing, so a host closing its dialog on the first resolution is
    // never handed a second identical one before the terminal.
    let resolutions = events
        .iter()
        .filter(|kind| matches!(kind, EventKind::QuestionResolved { .. }))
        .count();
    assert_eq!(
        resolutions, 1,
        "expected exactly one resolution, received {events:#?}"
    );

    let answer = mid_turn_push_answer(&launcher);
    assert_eq!(answer["result"], serde_json::json!({"answers": {}}));
}

/// An unanswered round is resolved exactly once, at its own deadline — never by the general
/// `request_timeout`, and never twice: a late answer after the fact is refused, not applied.
#[tokio::test(start_paused = true)]
async fn an_unanswered_question_round_expires_exactly_once() {
    let (session, launcher, mut turn) = open_with_mid_turn_push_and_clock(
        "item/tool/requestUserInput",
        |thread_id, turn_id| {
            serde_json::json!({
                "threadId": thread_id,
                "turnId": turn_id,
                "itemId": "ask-1",
                "isBlocking": false,
                "questions": [{"id": "note", "header": "Note", "question": "Anything else?"}],
            })
        },
        Some(Arc::new(FixedClock(SystemTime::UNIX_EPOCH))),
    )
    .await;

    let request = await_question(&mut turn).await;
    tokio::time::advance(approval_timeout()).await;
    let events = drain(&mut turn).await;

    // Exactly one resolution: a deadline that fired twice, or that raced a second path into
    // reporting the same round, would show up here as a second `QuestionResolved`.
    let resolutions = events
        .iter()
        .filter(|kind| matches!(kind, EventKind::QuestionResolved { .. }))
        .count();
    assert_eq!(
        resolutions, 1,
        "expected exactly one resolution, received {events:#?}"
    );
    assert!(
        events.iter().any(|kind| matches!(
            kind,
            EventKind::QuestionResolved {
                outcome: QuestionOutcome::Expired,
                ..
            }
        )),
        "expected the deadline to resolve the round as expired, received {events:#?}"
    );

    let answer = mid_turn_push_answer(&launcher);
    assert_eq!(answer["result"], serde_json::json!({"answers": {}}));

    // Exactly once: a late answer after expiry is refused, not silently applied.
    let error = session
        .answer(QuestionResponse::new(
            request.interaction.id.clone(),
            vec![QuestionAnswerValue::new(
                QuestionId::new("note"),
                AnswerValue::text("too late"),
            )],
        ))
        .await
        .expect_err("expected a late answer to be refused");
    assert!(
        matches!(error.cause(), mango_external_agents::Error::Protocol { .. }),
        "expected a protocol refusal, received {error:?}"
    );
}
