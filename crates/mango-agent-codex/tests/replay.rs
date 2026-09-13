//! The harness driven against transcripts a real `codex app-server` produced.
//!
//! Nothing here spawns anything: `mea capture` recorded the conversations, and a fake child
//! replays them. That is the only way to test a dialect on a machine where the vendor's CLI is not
//! installed — which is every machine, in CI.

mod support;

use std::sync::Arc;

use mango_agent_codex::CodexHarness;
use mango_external_agents::event::EventKind;
use mango_external_agents::permission::{
    BrokerDecision, DecisionSource, PermissionBroker, PermissionOptionKind, PermissionRequest,
    PermissionResponse,
};
use mango_external_agents::testing::{FakeLauncher, FakeProcess};
use mango_external_agents::{
    ApprovalRouting, CancelReason, CloseReason, Configuration, EnvSource, Harness, HostContext,
    OpenSession, PermissionLevel, Session, SessionQuery, Steer, TurnRequest,
};
use support::Transcript;

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
    with_launcher_limits(
        launcher,
        broker,
        mango_external_agents::Limits {
            // A call the replay has no answer for is a bug in the fixture or in the harness, and
            // the default two minutes would report it as a test that hangs rather than one that
            // fails.
            request_timeout: std::time::Duration::from_secs(5),
            ..mango_external_agents::Limits::default()
        },
    )
}

fn with_launcher_limits(
    launcher: Arc<FakeLauncher>,
    broker: Option<Arc<dyn PermissionBroker>>,
    limits: mango_external_agents::Limits,
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
        .limits(limits);
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
    assert!(!session.info().resumed);
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
        matches!(events.first(), Some(EventKind::SessionStarted { .. })),
        "expected the vendor session to be announced first, received {events:#?}"
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

/// A vendor session is opened once. Announcing it again on a later turn would tell a host the
/// conversation had just started on a turn where nothing did.
#[tokio::test]
async fn the_vendor_session_is_announced_on_the_first_turn_and_not_again() {
    let (session, _) = open("turn").await;

    let mut first = session
        .start_turn(TurnRequest::new("turn-1", "one"))
        .await
        .expect("expected a turn");
    let first = drain(&mut first).await;
    assert!(matches!(
        first.first(),
        Some(EventKind::SessionStarted { .. })
    ));

    let mut second = session
        .start_turn(TurnRequest::new("turn-2", "two"))
        .await
        .expect("expected a second turn");
    let second = drain(&mut second).await;
    assert!(
        !second
            .iter()
            .any(|kind| matches!(kind, EventKind::SessionStarted { .. })),
        "expected no second announcement, received {second:#?}"
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
        matches!(&error, mango_external_agents::Error::Vendor(vendor)
                 if vendor.code.as_str() == "codex-turn-already-running"),
        "expected the turn-already-running refusal, received {error:?}"
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
        matches!(&error, mango_external_agents::Error::Vendor(vendor)
                 if vendor.code.as_str() == "codex-turn-already-running"),
        "expected a running-turn refusal, received {error:?}"
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

/// A completion can clear a slot before the corresponding `turn/start` answer arrives. A delayed
/// success belongs to that finished start, even when the host reuses its own turn id meanwhile.
#[tokio::test]
async fn a_delayed_start_success_cannot_replace_the_live_turns_vendor_handle() {
    let (session, launcher) =
        open_with_delayed_first_start(DelayedStartAnswer::Success, true).await;
    let first_session = Arc::clone(&session);
    let first = tokio::spawn(async move {
        first_session
            .start_turn(TurnRequest::new("turn-reused", "one"))
            .await
    });
    wait_for_turn_start(&launcher).await;

    let mut second = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            match session
                .start_turn(TurnRequest::new("turn-reused", "two"))
                .await
            {
                Ok(turn) => break turn,
                Err(mango_external_agents::Error::Vendor(vendor))
                    if vendor.code.as_str() == "codex-turn-already-running" =>
                {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("expected the replacement turn, received {error:?}"),
            }
        }
    })
    .await
    .expect("expected the completion notification to free the first slot");
    assert_eq!(second.native_turn_id, "vendor-turn-2");

    session
        .refresh_account_usage()
        .await
        .expect("expected the trigger call to release the held answer");
    first
        .await
        .expect("expected the delayed start task to finish")
        .expect("expected the delayed start success");
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
                Err(mango_external_agents::Error::Vendor(vendor))
                    if vendor.code.as_str() == "codex-turn-already-running" =>
                {
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
        matches!(&error, mango_external_agents::Error::Vendor(vendor)
                 if vendor.code.as_str() == "codex-turn-already-running"),
        "expected a running-turn refusal, received {error:?}"
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

/// The same refusal, for a host that walked away rather than one that is waiting.
///
/// Dropping a `TurnStream` stops the host reading; it does not stop the vendor. The turn is still
/// running inside `codex app-server`, so the slot has to stay claimed — a free one would let the
/// next `turn/start` out, and the app-server reads that as a steer of the turn nobody is watching.
#[tokio::test]
async fn a_turn_whose_host_stopped_reading_still_holds_the_slot_against_a_steer() {
    let (session, launcher) = open("approval").await;
    let mut first = session
        .start_turn(TurnRequest::new("turn-1", "create mango.txt"))
        .await
        .expect("expected a turn");
    let asked = await_approval(&mut first).await;

    drop(first);
    // One event onto the closed stream, which is what a host that left looks like from here.
    session
        .respond(PermissionResponse {
            request_id: asked.id.clone(),
            option_id: asked
                .options
                .iter()
                .find(|option| option.kind == PermissionOptionKind::RejectOnce)
                .map(|option| option.id.clone())
                .expect("expected a refusal among the recorded options"),
            source: DecisionSource::User,
        })
        .await
        .expect("expected the refusal to reach the server");

    let before = launcher.written().len();
    let error = session
        .start_turn(TurnRequest::new("turn-2", "two"))
        .await
        .expect_err("expected a refusal, received a second turn on a live one");
    assert!(
        matches!(&error, mango_external_agents::Error::Vendor(vendor)
                 if vendor.code.as_str() == "codex-turn-already-running"),
        "expected the turn-already-running refusal, received {error:?}"
    );
    assert_eq!(
        launcher.written().len(),
        before,
        "expected no turn/start to reach the vendor"
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
    let answered = session
        .respond(PermissionResponse {
            request_id: asked.id.clone(),
            option_id: asked
                .options
                .iter()
                .find(|option| option.kind == PermissionOptionKind::RejectOnce)
                .map(|option| option.id.clone())
                .expect("expected a refusal among the recorded options"),
            source: DecisionSource::User,
        })
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

/// A host that stops reading its turn must not be able to stop the session from closing.
///
/// The turn channel is bounded, so an unread stream parks whatever is feeding it. If that park
/// happens while the turn lock is held, every other caller of that lock — `cancel`, `close`, the
/// next `start_turn` — parks behind a host that is never coming back, and a close that cannot
/// return leaves a `codex app-server` running for the life of the process.
///
/// The capacity is exactly what `start_turn` emits before the vendor says anything: one
/// `SessionStarted`. The channel is therefore full the instant the turn is returned, and the
/// pump's first real event parks. A larger capacity and the test proves nothing; a smaller one and
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
        matches!(error, mango_external_agents::Error::Closed { .. }),
        "expected a closed-session refusal, received {error:?}"
    );
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
    let (host, _launcher) = with_launcher(launcher, None);
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

    let events = drain(&mut review.turn).await;
    assert!(
        matches!(events.last(), Some(EventKind::Completed)),
        "expected the review to end like any other turn, received {events:#?}"
    );
    assert!(
        !events
            .iter()
            .any(|kind| matches!(kind, EventKind::SessionStarted { .. })),
        "expected a review not to announce the vendor session, received {events:#?}"
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
    let request = OpenSession::new("chat-1").with_configuration(Configuration {
        level: PermissionLevel::Default,
        routing: ApprovalRouting::AutoReview,
        ..Configuration::default()
    });
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

/// The announcement rides the first turn's stream, so a first turn the server refuses takes it
/// with it. The next turn to actually start is the one that carries it.
#[tokio::test]
async fn a_first_turn_the_server_refused_does_not_take_the_session_announcement_with_it() {
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

    session
        .start_turn(TurnRequest::new("turn-1", "one"))
        .await
        .expect_err("expected the server's refusal to reach the caller");

    let mut second = session
        .start_turn(TurnRequest::new("turn-2", "two"))
        .await
        .expect("expected the next turn to run");
    let events = drain(&mut second).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, EventKind::SessionStarted { .. })),
        "expected the first turn that actually started to announce the session, received {events:#?}"
    );
}

/// A level is a sandbox and an approval policy together, and `turn/start` takes only the policy.
///
/// Both directions are wrong, and the narrowing one is the dangerous one: a turn asked to run
/// read-only on a full-access session would have kept the sandbox and lost the prompts, so a host
/// trying to fence off a risky turn would have handed it the most permissive cell there is.
#[tokio::test]
async fn a_turn_cannot_be_given_a_permission_level_only_half_of_which_would_apply() {
    let (host, launcher) = host_replaying(&["turn"]);
    let request = OpenSession::new("chat-1").with_configuration(Configuration {
        level: PermissionLevel::FullAccess,
        routing: ApprovalRouting::User,
        ..Configuration::default()
    });
    let session = CodexHarness::new()
        .open_session(&host, request)
        .await
        .expect("expected a session");

    let before = launcher.written().len();
    let error = session
        .start_turn(
            TurnRequest::new("turn-1", "read the tree").with_configuration(Configuration {
                level: PermissionLevel::ReadOnly,
                routing: ApprovalRouting::User,
                ..Configuration::default()
            }),
        )
        .await
        .expect_err("expected a refusal, received a turn at a level only half of which applies");

    assert!(
        matches!(&error, mango_external_agents::Error::Protocol { received, .. }
                 if received.contains("ReadOnly")),
        "expected the refusal to name the level it was given, received {error:?}"
    );
    assert_eq!(
        launcher.written().len(),
        before,
        "expected no turn/start to reach the vendor"
    );
}

/// The same session's own level is not a change, and still runs.
#[tokio::test]
async fn a_turn_that_repeats_the_sessions_own_level_is_the_turn_it_always_was() {
    let (host, _launcher) = host_replaying(&["turn"]);
    let request = OpenSession::new("chat-1").with_configuration(Configuration {
        level: PermissionLevel::Default,
        routing: ApprovalRouting::User,
        ..Configuration::default()
    });
    let session = CodexHarness::new()
        .open_session(&host, request)
        .await
        .expect("expected a session");

    let mut turn = session
        .start_turn(
            TurnRequest::new("turn-1", "create mango.txt").with_configuration(Configuration {
                level: PermissionLevel::Default,
                routing: ApprovalRouting::AutoReview,
                ..Configuration::default()
            }),
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
        mango_external_agents::Capabilities::none(),
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
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(version_answer());
    launcher.push(Transcript::load("handshake").as_process());
    launcher.push(Transcript::load("approval").as_process());
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
        assert!(
            matches!(result, Err(mango_external_agents::Error::HostConfiguration { expected, received })
                if expected.contains("MCP") && received == "1 host-supplied MCP server(s)"),
            "expected an MCP configuration refusal naming the received server count"
        );
    }
}
