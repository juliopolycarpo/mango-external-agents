//! How long a Codex turn this side asked to stop may take to settle, and what ends the wait.
//!
//! Every test runs on a paused clock: the fake app-server answers `turn/start`, acknowledges
//! `turn/interrupt` and reports `turn/completed` only when the test releases it, so a late answer
//! is a timer the runtime reaches deterministically rather than a wall-clock race. The recorded
//! `turn` transcript still answers the handshake; only the three calls under test are held.

// Shared with the replay suite; this binary uses only the intercepting replay.
#[allow(dead_code)]
mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use mango_agent_codex::CodexHarness;
use mango_external_agents::testing::{Announcer, FakeLauncher};
use mango_external_agents::{
    CancelReason, CloseReason, EnvSource, Error, ExitStatus, Harness, HostContext,
    InterruptOutcome, LaunchSpec, Limits, ManagedProcess, OpenSession, ProcessControl,
    ProcessLauncher, Session, SessionStatus, TerminalStatus, TurnRequest,
};
use serde_json::{Value, json};
use support::{Transcript, workspace_path};
use tokio::sync::Semaphore;

/// Longer than the old `kill_grace` (2 s) and `shutdown_timeout` (5 s) defaults that used to end
/// a stop, and shorter than every deadline that now governs one.
const SLOW: Duration = Duration::from_secs(10);

/// How the fake app-server answers `turn/interrupt`.
#[derive(Clone, Copy)]
enum InterruptAnswer {
    /// Nothing until the test calls [`HeldTurnServer::ack_interrupt`].
    Held,
    /// An immediate acknowledgement; the terminal waits for [`HeldTurnServer::complete`].
    AckOnly,
    /// An immediate acknowledgement followed by the interrupted terminal.
    AckAndComplete,
    /// A JSON-RPC error, while the turn keeps running.
    Refused,
}

/// A Codex app-server peer whose turn answers are released by the test.
///
/// Holding an answer is the whole point: the real server may take as long as it needs to stop a
/// running tool, and the harness must not read that silence as proof the turn is gone.
struct HeldTurnServer {
    announcer: Announcer,
    thread_id: String,
    hold_first_start: bool,
    interrupt_answer: InterruptAnswer,
    held_start: Mutex<Option<Value>>,
    held_interrupt: Mutex<Option<Value>>,
    starts: AtomicUsize,
    interrupts: AtomicUsize,
}

impl HeldTurnServer {
    fn new(thread_id: String, hold_first_start: bool, interrupt_answer: InterruptAnswer) -> Self {
        Self {
            announcer: Announcer::new(),
            thread_id,
            hold_first_start,
            interrupt_answer,
            held_start: Mutex::new(None),
            held_interrupt: Mutex::new(None),
            starts: AtomicUsize::new(0),
            interrupts: AtomicUsize::new(0),
        }
    }

    fn respond(&self, frame: &Value) -> Option<Vec<String>> {
        let method = frame.get("method").and_then(Value::as_str)?;
        let id = frame.get("id").cloned().unwrap_or(Value::Null);
        match method {
            "turn/start" => {
                let start = self.starts.fetch_add(1, Ordering::SeqCst);
                if start == 0 && self.hold_first_start {
                    *self
                        .held_start
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner) = Some(id);
                    return Some(Vec::new());
                }
                Some(vec![start_answer(&id, &native_turn(start))])
            }
            "turn/interrupt" => {
                self.interrupts.fetch_add(1, Ordering::SeqCst);
                let turn = frame
                    .pointer("/params/turnId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                Some(match self.interrupt_answer {
                    InterruptAnswer::Held => {
                        *self
                            .held_interrupt
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner) = Some(id);
                        Vec::new()
                    }
                    InterruptAnswer::AckOnly => vec![ack(&id)],
                    InterruptAnswer::AckAndComplete => {
                        vec![ack(&id), self.completed(&turn, "interrupted")]
                    }
                    InterruptAnswer::Refused => vec![
                        json!({
                            "id": id,
                            "error": {"code": -32603, "message": "interrupt refused"},
                        })
                        .to_string(),
                    ],
                })
            }
            _ => None,
        }
    }

    /// Delivers the held first `turn/start` answer, naming `vendor-turn-1`.
    fn answer_start(&self) {
        let id = self
            .held_start
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .expect("expected a held turn/start request");
        self.announcer.announce(start_answer(&id, &native_turn(0)));
    }

    /// Delivers the held `turn/interrupt` acknowledgement.
    fn ack_interrupt(&self) {
        let id = self
            .held_interrupt
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .expect("expected a held turn/interrupt request");
        self.announcer.announce(ack(&id));
    }

    /// Reports that the vendor turn ended.
    fn complete(&self, turn: &str, status: &str) {
        self.announcer.announce(self.completed(turn, status));
    }

    /// Reports activity on `turn`, which names it before any `turn/start` answer does.
    fn item_started(&self, turn: &str) {
        self.announcer.announce(
            json!({
                "method": "item/started",
                "params": {
                    "threadId": self.thread_id,
                    "turnId": turn,
                    "item": {"clientId": null, "content": [], "id": "item-1", "type": "userMessage"},
                },
            })
            .to_string(),
        );
    }

    /// A terminal that names no thread or turn, which poisons the connection.
    fn unroutable_terminal(&self) {
        self.announcer.announce(
            json!({"method": "turn/completed", "params": {"unexpected": true}}).to_string(),
        );
    }

    fn completed(&self, turn: &str, status: &str) -> String {
        json!({
            "method": "turn/completed",
            "params": {"threadId": self.thread_id, "turn": {"id": turn, "status": status}},
        })
        .to_string()
    }

    fn interrupts(&self) -> usize {
        self.interrupts.load(Ordering::SeqCst)
    }
}

fn native_turn(start: usize) -> String {
    format!("vendor-turn-{}", start + 1)
}

fn start_answer(id: &Value, turn: &str) -> String {
    json!({"id": id, "result": {"turn": {"id": turn}}}).to_string()
}

fn ack(id: &Value) -> String {
    json!({"id": id, "result": {}}).to_string()
}

/// What a forced termination of the fake child does.
#[derive(Clone)]
enum KillBehaviour {
    /// Ends the child at once.
    Reaps,
    /// Waits for one permit, so the moment before reaping is observable.
    Gated(Arc<Semaphore>),
    /// Fails, and the child survives its closed stdin: the host has to reconcile it.
    Refused,
}

/// Counts forced terminations of the children it launches.
struct CountingLauncher {
    inner: Arc<FakeLauncher>,
    kills: Arc<AtomicUsize>,
    kill: KillBehaviour,
    write_gate: Option<WriteGate>,
}

/// Holds the one stdin write that names `method` until the test adds a permit.
///
/// While held, the client's single writer is busy, so every later frame waits its turn and has
/// not been written at all.
#[derive(Clone)]
struct WriteGate {
    method: &'static str,
    gate: Arc<Semaphore>,
}

/// A fake child's stdin that stops at the gated write.
struct GatedStdin {
    inner: Box<dyn mango_external_agents::ByteSink>,
    write_gate: WriteGate,
}

#[async_trait::async_trait]
impl mango_external_agents::ByteSink for GatedStdin {
    async fn write_all(&mut self, bytes: &[u8]) -> mango_external_agents::Result<()> {
        let needle = format!("\"{}\"", self.write_gate.method);
        if String::from_utf8_lossy(bytes).contains(&needle) {
            self.write_gate
                .gate
                .acquire()
                .await
                .expect("the fake write gate must stay open")
                .forget();
        }
        self.inner.write_all(bytes).await
    }

    async fn close(&mut self) -> mango_external_agents::Result<()> {
        self.inner.close().await
    }
}

#[async_trait::async_trait]
impl ProcessLauncher for CountingLauncher {
    async fn spawn(&self, spec: LaunchSpec) -> mango_external_agents::Result<ManagedProcess> {
        let mut child = self.inner.spawn(spec).await?;
        if let Some(write_gate) = &self.write_gate {
            let write_gate = write_gate.clone();
            child.stdin = child.stdin.take().map(|inner| {
                Box::new(GatedStdin { inner, write_gate })
                    as Box<dyn mango_external_agents::ByteSink>
            });
        }
        if matches!(self.kill, KillBehaviour::Refused) {
            child.stdin = child.stdin.take().map(|inner| {
                Box::new(SurvivingStdin { inner }) as Box<dyn mango_external_agents::ByteSink>
            });
        }
        child.control = Arc::new(CountingControl {
            inner: child.control,
            kills: Arc::clone(&self.kills),
            kill: self.kill.clone(),
        });
        Ok(child)
    }
}

/// Keeps a fake child alive when its app-server link closes, so only a kill could end it.
struct SurvivingStdin {
    inner: Box<dyn mango_external_agents::ByteSink>,
}

#[async_trait::async_trait]
impl mango_external_agents::ByteSink for SurvivingStdin {
    async fn write_all(&mut self, bytes: &[u8]) -> mango_external_agents::Result<()> {
        self.inner.write_all(bytes).await
    }

    async fn close(&mut self) -> mango_external_agents::Result<()> {
        Ok(())
    }
}

/// A fake child's control that records every kill before acting on it.
struct CountingControl {
    inner: Arc<dyn ProcessControl>,
    kills: Arc<AtomicUsize>,
    kill: KillBehaviour,
}

#[async_trait::async_trait]
impl ProcessControl for CountingControl {
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
        self.kills.fetch_add(1, Ordering::SeqCst);
        match &self.kill {
            KillBehaviour::Reaps => {}
            KillBehaviour::Gated(gate) => gate
                .acquire()
                .await
                .expect("the fake kill gate must stay open")
                .forget(),
            KillBehaviour::Refused => {
                return Err(Error::Closed {
                    subject: "fake app-server process",
                });
            }
        }
        self.inner.kill(reason).await
    }
}

/// One opened session over a [`HeldTurnServer`].
struct Fixture {
    session: Arc<dyn Session>,
    server: Arc<HeldTurnServer>,
    launcher: Arc<FakeLauncher>,
    kills: Arc<AtomicUsize>,
    limits: Limits,
}

impl Fixture {
    async fn open(
        hold_first_start: bool,
        interrupt_answer: InterruptAnswer,
        kill: KillBehaviour,
    ) -> Self {
        Self::open_with_write_gate(hold_first_start, interrupt_answer, kill, None).await
    }

    async fn open_with_write_gate(
        hold_first_start: bool,
        interrupt_answer: InterruptAnswer,
        kill: KillBehaviour,
        write_gate: Option<WriteGate>,
    ) -> Self {
        let transcript = Transcript::load("turn");
        let server = Arc::new(HeldTurnServer::new(
            transcript
                .thread_id()
                .expect("expected the recording to name its thread"),
            hold_first_start,
            interrupt_answer,
        ));
        let responder = Arc::clone(&server);
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(
            transcript
                .as_process_intercepting(move |frame| responder.respond(frame))
                .announcing(server.announcer.clone()),
        );
        let kills = Arc::new(AtomicUsize::new(0));
        let limits = Limits::default();
        let host = HostContext::builder()
            .launcher(Arc::new(CountingLauncher {
                inner: Arc::clone(&launcher),
                kills: Arc::clone(&kills),
                kill,
                write_gate,
            }))
            .cwd(workspace_path())
            .client_info("mango-test", "0.0.1")
            .environment(EnvSource::from_pairs([
                ("PATH", "/usr/bin"),
                ("CODEX_HOME", "/home/user/.codex"),
            ]))
            .limits(limits)
            .build()
            .expect("expected a valid host context");
        let session = CodexHarness::new()
            .open_session(&host, OpenSession::new("chat-1"))
            .await
            .expect("expected a session");
        Self {
            session: Arc::from(session),
            server,
            launcher,
            kills,
            limits,
        }
    }

    fn kills(&self) -> usize {
        self.kills.load(Ordering::SeqCst)
    }

    fn status(&self) -> SessionStatus {
        self.session.subscribe().current().status
    }

    fn spawn_cancel(&self) -> tokio::task::JoinHandle<mango_external_agents::Result<()>> {
        let session = Arc::clone(&self.session);
        tokio::spawn(async move { session.cancel(CancelReason::Requested).await })
    }

    async fn wait_for_written(&self, method: &str) {
        let needle = format!("\"{method}\"");
        tokio::time::timeout(Duration::from_secs(1), async {
            while !self
                .launcher
                .written()
                .iter()
                .any(|line| line.contains(&needle))
            {
                // A timer, not a yield: on a paused clock only a pending timer lets time advance,
                // so a yield loop would never reach this bound and hang instead of failing.
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("expected {method} to reach the fake app-server"));
    }

    fn assert_no_kill(&self, when: &str) {
        assert_eq!(
            self.kills(),
            0,
            "{when}: expected kills: 0 | received: {}",
            self.kills()
        );
    }

    fn assert_interrupts(&self, expected: usize, when: &str) {
        assert_eq!(
            self.server.interrupts(),
            expected,
            "{when}: expected turn/interrupt dispatches: {expected} | received: {}",
            self.server.interrupts()
        );
    }
}

async fn terminal_of(turn: &mut mango_external_agents::TurnStream) -> Option<TerminalStatus> {
    tokio::time::timeout(Duration::from_secs(5), async {
        while turn.recv().await.is_some() {}
    })
    .await
    .expect("expected the turn stream to end");
    turn.terminal_status()
}

/// An interrupt Codex acknowledges only after the old kill-grace and shutdown bounds is still a
/// settling turn, not a dead connection.
#[tokio::test(start_paused = true)]
async fn slow_interrupt_acknowledgement_settles_without_a_kill() {
    let fixture = Fixture::open(false, InterruptAnswer::Held, KillBehaviour::Reaps).await;
    let mut turn = fixture
        .session
        .start_turn(TurnRequest::new("turn-1", "run"))
        .await
        .expect("expected a live native turn");
    let cancel = fixture.spawn_cancel();
    fixture.wait_for_written("turn/interrupt").await;

    tokio::time::sleep(SLOW).await;
    fixture.assert_no_kill("while the interrupt acknowledgement was outstanding");
    fixture.server.ack_interrupt();
    fixture.server.complete("vendor-turn-1", "interrupted");

    tokio::time::timeout(Duration::from_secs(5), cancel)
        .await
        .expect("expected cancel to return once the turn settled")
        .expect("expected the cancel task to finish")
        .expect("expected a settled cancel to succeed");
    fixture.assert_no_kill("after the turn settled");
    fixture.assert_interrupts(1, "after a slow acknowledgement");
    assert_eq!(
        terminal_of(&mut turn).await,
        Some(TerminalStatus::Cancelled {
            reason: CancelReason::Requested
        }),
        "expected the interrupted turn to end as the host's cancellation"
    );
    assert_eq!(fixture.status(), SessionStatus::Ready);
    fixture
        .session
        .start_turn(TurnRequest::new("turn-2", "again"))
        .await
        .expect("expected the settled session to admit the next turn");
}

/// An acknowledged interrupt whose terminal arrives after the old shutdown bound is still settling.
#[tokio::test(start_paused = true)]
async fn delayed_terminal_after_acknowledgement_does_not_trigger_shutdown() {
    let fixture = Fixture::open(false, InterruptAnswer::AckOnly, KillBehaviour::Reaps).await;
    let mut turn = fixture
        .session
        .start_turn(TurnRequest::new("turn-1", "run"))
        .await
        .expect("expected a live native turn");
    let cancel = fixture.spawn_cancel();
    fixture.wait_for_written("turn/interrupt").await;

    tokio::time::sleep(SLOW).await;
    fixture.assert_no_kill("while the acknowledged turn was still settling");
    assert_eq!(
        fixture.status(),
        SessionStatus::Ready,
        "expected a settling turn to keep the session Ready"
    );
    fixture.server.complete("vendor-turn-1", "interrupted");

    tokio::time::timeout(Duration::from_secs(5), cancel)
        .await
        .expect("expected cancel to return once the turn settled")
        .expect("expected the cancel task to finish")
        .expect("expected a settled cancel to succeed");
    fixture.assert_no_kill("after the turn settled");
    fixture.assert_interrupts(1, "after a slow terminal");
    assert!(
        matches!(
            terminal_of(&mut turn).await,
            Some(TerminalStatus::Cancelled { .. })
        ),
        "expected the interrupted turn to end cancelled"
    );
}

/// A cancel that beats the `turn/start` answer waits for the id, then interrupts exactly once. The
/// late successful start is still returned to its caller.
#[tokio::test(start_paused = true)]
async fn cancel_before_a_slow_start_interrupts_once_the_turn_is_named() {
    let fixture = Fixture::open(true, InterruptAnswer::AckAndComplete, KillBehaviour::Reaps).await;
    let session = Arc::clone(&fixture.session);
    let start =
        tokio::spawn(async move { session.start_turn(TurnRequest::new("turn-1", "run")).await });
    fixture.wait_for_written("turn/start").await;

    let cancel = fixture.spawn_cancel();
    tokio::time::sleep(SLOW).await;
    fixture.assert_no_kill("while turn/start was unanswered");
    fixture.assert_interrupts(0, "before the turn had an id");
    // Cancel before a start answer records the stop and returns: its outcome belongs to the
    // start's caller, whose stream reports the terminal, and awaiting it here could wait on a
    // start future the same host task has not polled.
    assert!(
        cancel.is_finished(),
        "expected cancel before a start answer to return without waiting for the answer"
    );
    let competing = fixture
        .session
        .start_turn(TurnRequest::new("turn-2", "competing"))
        .await
        .expect_err("expected a competing start to be refused while the first is unresolved");
    assert!(
        matches!(competing.cause(), Error::Busy),
        "expected Busy while the first start is unresolved | received: {competing:?}"
    );

    fixture.server.answer_start();
    let mut turn = tokio::time::timeout(Duration::from_secs(5), start)
        .await
        .expect("expected the late start to return")
        .expect("expected the start task to finish")
        .expect("expected the late successful start to stay observable");
    assert_eq!(turn.native_turn_id(), "vendor-turn-1");
    tokio::time::timeout(Duration::from_secs(5), cancel)
        .await
        .expect("expected cancel to return")
        .expect("expected the cancel task to finish")
        .expect("expected cancel before start to succeed");
    assert!(
        matches!(
            terminal_of(&mut turn).await,
            Some(TerminalStatus::Cancelled { .. })
        ),
        "expected the late turn to end cancelled"
    );
    fixture.assert_interrupts(1, "after the start was named");
    fixture.assert_no_kill("after the late turn settled");
    assert_eq!(fixture.status(), SessionStatus::Ready);
}

/// While a stop is unresolved the session is not idle: a competing start would be read by Codex as
/// a steer of the live turn, so admission refuses it.
#[tokio::test(start_paused = true)]
async fn a_stopping_turn_refuses_a_competing_start() {
    let fixture = Fixture::open(false, InterruptAnswer::Held, KillBehaviour::Reaps).await;
    let _turn = fixture
        .session
        .start_turn(TurnRequest::new("turn-1", "run"))
        .await
        .expect("expected a live native turn");
    let cancel = fixture.spawn_cancel();
    fixture.wait_for_written("turn/interrupt").await;
    tokio::time::sleep(SLOW).await;

    let competing = fixture
        .session
        .start_turn(TurnRequest::new("turn-2", "competing"))
        .await
        .expect_err("expected a competing start to be refused while stopping");
    assert!(
        matches!(competing.cause(), Error::Busy),
        "expected Busy while the prior turn is stopping | received: {competing:?}"
    );
    fixture.assert_no_kill("while the stop was unresolved");

    fixture.server.ack_interrupt();
    fixture.server.complete("vendor-turn-1", "interrupted");
    tokio::time::timeout(Duration::from_secs(5), cancel)
        .await
        .expect("expected cancel to return")
        .expect("expected the cancel task to finish")
        .expect("expected cancel to succeed");
    assert_eq!(
        fixture.starts(),
        1,
        "expected only the first turn/start on the wire"
    );
}

impl Fixture {
    fn starts(&self) -> usize {
        self.server.starts.load(Ordering::SeqCst)
    }
}

/// Two cancels and a close racing one outstanding interrupt share it: one dispatch, one kill, and
/// close finishes inside the shutdown policy rather than the settle deadline.
#[tokio::test(start_paused = true)]
async fn repeated_cancel_and_close_share_one_stop() {
    let fixture = Fixture::open(false, InterruptAnswer::Held, KillBehaviour::Reaps).await;
    let _turn = fixture
        .session
        .start_turn(TurnRequest::new("turn-1", "run"))
        .await
        .expect("expected a live native turn");
    let first = fixture.spawn_cancel();
    let second = fixture.spawn_cancel();
    fixture.wait_for_written("turn/interrupt").await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    fixture.assert_interrupts(1, "after two cancels");

    let limits = fixture.limits;
    let bound = limits.kill_grace + limits.shutdown_timeout * 3;
    tokio::time::timeout(bound, fixture.session.close(CloseReason::Shutdown))
        .await
        .unwrap_or_else(|_| panic!("expected close within the shutdown policy of {bound:?}"))
        .expect("expected close to succeed");
    for cancel in [first, second] {
        let result = tokio::time::timeout(Duration::from_secs(5), cancel)
            .await
            .expect("expected each cancel waiter to finish after close")
            .expect("expected the cancel task to finish");
        assert!(
            result.is_ok(),
            "expected a cancel that close reaped cleanly to succeed | received: {result:?}"
        );
    }
    fixture.assert_interrupts(1, "after close raced the stop");
    assert_eq!(
        fixture.kills(),
        1,
        "expected kills: 1 | received: {}",
        fixture.kills()
    );
    assert_eq!(fixture.status(), SessionStatus::Closed);
}

/// When the documented settle deadline itself expires the session escalates: exactly one kill,
/// and the session reports Closed only once that kill has reaped the child.
#[tokio::test(start_paused = true)]
async fn settle_deadline_expiry_kills_once_and_closes_after_reap() {
    let kill_gate = Arc::new(Semaphore::new(0));
    let fixture = Fixture::open(
        false,
        InterruptAnswer::AckOnly,
        KillBehaviour::Gated(Arc::clone(&kill_gate)),
    )
    .await;
    let _turn = fixture
        .session
        .start_turn(TurnRequest::new("turn-1", "run"))
        .await
        .expect("expected a live native turn");
    let cancel = fixture.spawn_cancel();
    fixture.wait_for_written("turn/interrupt").await;

    let settle = fixture.limits.cancel_settle_timeout;
    tokio::time::sleep(settle - Duration::from_secs(1)).await;
    fixture.assert_no_kill("just before the settle deadline");

    tokio::time::timeout(settle, async {
        while fixture.kills() == 0 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("expected the settle deadline to escalate to a kill");
    assert_ne!(
        fixture.status(),
        SessionStatus::Closed,
        "expected the session not to report Closed before its child is reaped"
    );
    kill_gate.add_permits(1);
    let _ = tokio::time::timeout(Duration::from_secs(30), cancel)
        .await
        .expect("expected cancel to return after escalation");
    assert_eq!(
        fixture.kills(),
        1,
        "expected kills: 1 | received: {}",
        fixture.kills()
    );
    assert_eq!(fixture.status(), SessionStatus::Closed);
    assert_eq!(fixture.launcher.live_children(), 0);
}

/// Names the variant a cancel returned, for a failure message that shows what arrived instead.
fn describe(error: &Error) -> String {
    format!("{error:?}")
}

fn expect_cleanup_required(error: &Error, when: &str) {
    assert!(
        matches!(error.cause(), Error::CleanupRequired { .. }) || error.cleanup_control().is_some(),
        "{when}: expected Error::CleanupRequired carrying the process control | received: {}",
        describe(error)
    );
}

/// A refused interrupt escalates to shutdown; when that teardown cannot reap the child, the
/// cancel caller receives the cleanup control rather than the interrupt error alone.
#[tokio::test(start_paused = true)]
async fn refused_interrupt_whose_teardown_fails_returns_the_cleanup_control() {
    let fixture = Fixture::open(false, InterruptAnswer::Refused, KillBehaviour::Refused).await;
    let _turn = fixture
        .session
        .start_turn(TurnRequest::new("turn-1", "run"))
        .await
        .expect("expected a live native turn");

    let error = tokio::time::timeout(
        Duration::from_secs(120),
        fixture.session.cancel(CancelReason::Requested),
    )
    .await
    .expect("expected cancel to finish within the shutdown policy")
    .expect_err("expected cancel to report the failed teardown");
    expect_cleanup_required(&error, "after a refused interrupt and a failed kill");
}

/// When the settle deadline escalates and the kill fails, cancel returns the cleanup control.
#[tokio::test(start_paused = true)]
async fn settle_expiry_whose_teardown_fails_returns_the_cleanup_control() {
    let fixture = Fixture::open(false, InterruptAnswer::AckOnly, KillBehaviour::Refused).await;
    let _turn = fixture
        .session
        .start_turn(TurnRequest::new("turn-1", "run"))
        .await
        .expect("expected a live native turn");

    let error = tokio::time::timeout(
        fixture.limits.cancel_settle_timeout + Duration::from_secs(120),
        fixture.session.cancel(CancelReason::Requested),
    )
    .await
    .expect("expected cancel to finish after the settle deadline and the shutdown policy")
    .expect_err("expected cancel to report the failed teardown");
    expect_cleanup_required(&error, "after the settle deadline and a failed kill");
    assert_eq!(
        fixture.kills(),
        1,
        "expected kills: 1 | received: {}",
        fixture.kills()
    );
}

/// A close that ends a stopping turn and then cannot reap the child hands its cleanup control to
/// the cancel waiter as well, rather than letting that waiter report success.
#[tokio::test(start_paused = true)]
async fn close_racing_a_stop_that_cannot_reap_fails_the_cancel_waiter() {
    let fixture = Fixture::open(false, InterruptAnswer::Held, KillBehaviour::Refused).await;
    let _turn = fixture
        .session
        .start_turn(TurnRequest::new("turn-1", "run"))
        .await
        .expect("expected a live native turn");
    let cancel = fixture.spawn_cancel();
    fixture.wait_for_written("turn/interrupt").await;

    let closed = tokio::time::timeout(
        Duration::from_secs(120),
        fixture.session.close(CloseReason::Shutdown),
    )
    .await
    .expect("expected close within the shutdown policy");
    let close_error = closed.expect_err("expected close to report the failed reap");
    expect_cleanup_required(&close_error, "close");

    let cancelled = tokio::time::timeout(Duration::from_secs(120), cancel)
        .await
        .expect("expected the cancel waiter to finish after close")
        .expect("expected the cancel task to finish");
    match cancelled {
        Ok(()) => panic!(
            "expected the cancel waiter to receive Error::CleanupRequired | received: Ok(())"
        ),
        Err(error) => expect_cleanup_required(&error, "cancel waiter after a racing close"),
    }
}

/// A start whose answer was lost can still be named by a later notification. The stop then sends
/// its one interrupt and the turn settles, instead of being killed at the settle deadline.
#[tokio::test(start_paused = true)]
async fn an_unanswered_start_named_later_is_interrupted_not_killed() {
    let fixture = Fixture::open(true, InterruptAnswer::AckAndComplete, KillBehaviour::Reaps).await;
    let session = Arc::clone(&fixture.session);
    let start =
        tokio::spawn(async move { session.start_turn(TurnRequest::new("turn-1", "run")).await });
    fixture.wait_for_written("turn/start").await;
    let cancel = fixture.spawn_cancel();

    let turn = tokio::time::timeout(fixture.limits.request_timeout * 2, start)
        .await
        .expect("expected the start to give up at its request deadline")
        .expect("expected the start task to finish")
        .expect("expected an acceptance-unknown stream rather than an error");
    assert_eq!(
        turn.dispatch(),
        mango_external_agents::Dispatch::AcceptanceUnknown,
        "expected a lost start answer to leave acceptance unknown"
    );

    fixture.server.item_started("vendor-turn-1");
    tokio::time::sleep(SLOW).await;
    fixture.assert_interrupts(1, "after a notification named the unanswered start");
    fixture.assert_no_kill("after the named turn was interrupted");
    let _ = tokio::time::timeout(Duration::from_secs(5), cancel)
        .await
        .expect("expected cancel to return");
    assert_eq!(fixture.status(), SessionStatus::Ready);
}

/// Poison while a start is pending releases its slot and reaps the process, so the start's late
/// answer sends no interrupt: process teardown, not a protocol stop, ends that turn.
#[tokio::test(start_paused = true)]
async fn poison_during_a_pending_start_sends_no_interrupt() {
    let fixture = Fixture::open(true, InterruptAnswer::AckAndComplete, KillBehaviour::Reaps).await;
    let session = Arc::clone(&fixture.session);
    let start =
        tokio::spawn(async move { session.start_turn(TurnRequest::new("turn-1", "run")).await });
    fixture.wait_for_written("turn/start").await;

    fixture.server.unroutable_terminal();
    tokio::time::sleep(Duration::from_millis(10)).await;
    fixture.server.answer_start();
    let _ = tokio::time::timeout(Duration::from_secs(30), start)
        .await
        .expect("expected the start to return after poison");
    tokio::time::sleep(SLOW).await;
    fixture.assert_interrupts(0, "after poison and a late start answer");
    assert_eq!(
        fixture.launcher.live_children(),
        0,
        "expected the poisoned session to reap its app-server"
    );
}

/// A host may keep a `start_turn` future alive but stop polling it, for example a pinned future
/// that lost a `select!`. The stop worker cannot rely on that future to resolve the start: after
/// `request_timeout` it treats the start as unanswerable and moves on to settle and escalation.
#[tokio::test(start_paused = true)]
async fn a_stop_does_not_wait_on_an_unpolled_start_future() {
    let fixture = Fixture::open(true, InterruptAnswer::AckAndComplete, KillBehaviour::Reaps).await;
    let session = Arc::clone(&fixture.session);
    let mut start =
        Box::pin(async move { session.start_turn(TurnRequest::new("turn-1", "run")).await });
    tokio::select! {
        _ = &mut start => panic!("expected the held turn/start to stay unanswered"),
        () = fixture.wait_for_written("turn/start") => {}
    }
    // `start` stays alive and is never polled again.

    let bound = fixture.limits.request_timeout
        + fixture.limits.cancel_settle_timeout
        + fixture.limits.kill_grace
        + fixture.limits.shutdown_timeout * 4;
    let cancelled = tokio::time::timeout(bound, fixture.session.cancel(CancelReason::Requested));
    let _ = cancelled.await;
    let stopped = tokio::time::timeout(bound, async {
        while fixture.status() != SessionStatus::Closed {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    assert!(
        stopped.is_ok(),
        "expected the stop to escalate within {bound:?} | received: status {:?}, kills {}",
        fixture.status(),
        fixture.kills()
    );
    assert_eq!(
        fixture.kills(),
        1,
        "expected kills: 1 | received: {}",
        fixture.kills()
    );
    fixture.assert_interrupts(0, "for a start that was never named");
    drop(start);
}

/// A start dropped before its `turn/start` frame reached the wire cannot have a native turn, so
/// its owner is released at once: no settle wait, no kill, and the next start is admitted.
#[tokio::test(start_paused = true)]
async fn a_start_dropped_before_its_request_was_written_releases_at_once() {
    let gate = Arc::new(Semaphore::new(Semaphore::MAX_PERMITS));
    let fixture = Fixture::open_with_write_gate(
        false,
        InterruptAnswer::AckAndComplete,
        KillBehaviour::Reaps,
        Some(WriteGate {
            method: "account/rateLimits/read",
            gate: Arc::clone(&gate),
        }),
    )
    .await;
    gate.forget_permits(Semaphore::MAX_PERMITS);

    // Occupies the client's one writer, so the start below cannot write its frame.
    let usage_session = Arc::clone(&fixture.session);
    let usage = tokio::spawn(async move { usage_session.refresh_account_usage().await });
    tokio::time::sleep(Duration::from_millis(10)).await;

    let session = Arc::clone(&fixture.session);
    let mut start =
        Box::pin(async move { session.start_turn(TurnRequest::new("turn-1", "run")).await });
    tokio::select! {
        _ = &mut start => panic!("expected the start to wait behind the held write"),
        () = tokio::time::sleep(Duration::from_millis(10)) => {}
    }
    assert_eq!(
        fixture.starts(),
        0,
        "expected turn/start unwritten | received a written start"
    );
    drop(start);
    gate.add_permits(1_000);
    tokio::time::sleep(Duration::from_millis(10)).await;

    let next = tokio::time::timeout(
        Duration::from_secs(5),
        fixture
            .session
            .start_turn(TurnRequest::new("turn-2", "again")),
    )
    .await
    .expect("expected the next start to answer promptly");
    assert!(
        next.is_ok(),
        "expected the next start admitted | received: {:?}",
        next.as_ref().err()
    );
    fixture.assert_no_kill("after a start dropped before its write");
    usage.abort();
}
