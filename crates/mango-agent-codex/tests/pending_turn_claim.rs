//! Which frames may name a turn while its `turn/start` answer has not arrived.
//!
//! The connection also carries the vendor's subagents' threads, and their frames name turns of
//! their own. Until the start answer comes back the session has no native id, so the first frame
//! that names one used to be taken as the session's own turn, whichever thread it belonged to. The
//! fake app-server holds the start answer and writes foreign-thread traffic ahead of it.

// Shared with the replay suite; this binary uses only the intercepting replay.
#[allow(dead_code)]
mod support;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use mango_agent_codex::CodexHarness;
use mango_external_agents::testing::{Announcer, FakeLauncher};
use mango_external_agents::{
    CancelReason, CloseReason, EnvSource, EventKind, Harness, HostContext, OpenSession, Session,
    TurnRequest,
};
use serde_json::{Value, json};
use support::{Transcript, workspace_path};

fn host(launcher: Arc<FakeLauncher>) -> HostContext {
    HostContext::builder()
        .launcher(launcher)
        .cwd(workspace_path())
        .client_info("mango-test", "0.0.1")
        .environment(EnvSource::from_pairs([
            ("PATH", "/usr/bin"),
            ("CODEX_HOME", "/home/user/.codex"),
        ]))
        .build()
        .expect("expected a valid host context")
}

/// One frame of a subagent's thread, which is never this session's.
fn foreign(method: &str, params: Value) -> String {
    json!({"method": method, "params": params}).to_string()
}

/// The frames a subagent writes ahead of the start answer: a delta, an item and a terminal.
fn foreign_traffic() -> Vec<String> {
    vec![
        foreign(
            "item/agentMessage/delta",
            json!({"threadId": "foreign-thread", "turnId": "foreign-turn",
                "itemId": "foreign-item", "delta": "foreign-text"}),
        ),
        foreign(
            "item/started",
            json!({"threadId": "foreign-thread", "turnId": "foreign-turn",
                "item": {"type": "commandExecution", "id": "foreign-cmd",
                    "command": "ls", "status": "inProgress"}}),
        ),
        foreign(
            "turn/completed",
            json!({"threadId": "foreign-thread",
                "turn": {"id": "foreign-turn", "status": "completed"}}),
        ),
    ]
}

/// A session whose first `turn/start` answer is held until the test releases it.
struct HeldStart {
    session: Arc<dyn Session>,
    announcer: Announcer,
    held_id: Arc<Mutex<Option<Value>>>,
    interrupts: Arc<Mutex<Vec<Value>>>,
    thread: String,
}

impl HeldStart {
    /// `before_answer` is what the server writes right after it reads the start request, and
    /// `interrupt_answer` how it answers a `turn/interrupt`.
    async fn open(
        before_answer: impl Fn(&str) -> Vec<String> + Send + Sync + 'static,
        interrupt_answer: impl Fn(&Value, &str) -> Vec<String> + Send + Sync + 'static,
    ) -> Self {
        let transcript = Transcript::load("turn");
        let thread = transcript
            .thread_id()
            .expect("expected the recording to name its thread");
        let held_id = Arc::new(Mutex::new(None::<Value>));
        let interrupts = Arc::new(Mutex::new(Vec::<Value>::new()));
        let announcer = Announcer::new();
        let launcher = Arc::new(FakeLauncher::new());
        let (held, seen, own) = (
            Arc::clone(&held_id),
            Arc::clone(&interrupts),
            thread.clone(),
        );
        launcher.push(
            transcript
                .as_process_intercepting(move |frame| {
                    match frame.get("method").and_then(Value::as_str) {
                        Some("turn/start") => {
                            *held.lock().unwrap_or_else(PoisonError::into_inner) =
                                Some(frame["id"].clone());
                            Some(before_answer(&own))
                        }
                        Some("turn/interrupt") => {
                            seen.lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .push(frame.clone());
                            Some(interrupt_answer(frame, &own))
                        }
                        _ => None,
                    }
                })
                .announcing(announcer.clone()),
        );
        let session: Arc<dyn Session> = Arc::from(
            CodexHarness::new()
                .open_session(&host(launcher), OpenSession::new("chat-1"))
                .await
                .expect("expected a session"),
        );
        Self {
            session,
            announcer,
            held_id,
            interrupts,
            thread,
        }
    }

    /// Waits until the server has read the start request, then lets the frames written ahead of
    /// its answer reach the session.
    async fn until_start_is_read(&self) {
        for _ in 0..400 {
            if self
                .held_id
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    /// Answers the held start with the session's own turn.
    fn answer_start(&self, native_turn_id: &str) {
        let id = self
            .held_id
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .expect("expected a start request to answer");
        self.announcer
            .announce(json!({"id": id, "result": {"turn": {"id": native_turn_id}}}).to_string());
    }

    fn interrupt_targets(&self) -> Vec<String> {
        self.interrupts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|frame| frame["params"]["turnId"].to_string())
            .collect()
    }
}

#[tokio::test]
async fn foreign_thread_traffic_before_the_start_answer_does_not_claim_the_turn() {
    let held = HeldStart::open(
        |own| {
            let mut frames = foreign_traffic();
            frames.push(foreign(
                "item/agentMessage/delta",
                json!({"threadId": own, "turnId": "real-turn", "itemId": "real-item",
                    "delta": "real-answer-before-response"}),
            ));
            frames
        },
        |_, _| Vec::new(),
    )
    .await;
    let starter = Arc::clone(&held.session);
    let starting = tokio::spawn(async move {
        starter
            .start_turn(TurnRequest::new("host-turn", "hello"))
            .await
    });
    held.until_start_is_read().await;
    held.answer_start("real-turn");
    let mut turn = starting
        .await
        .expect("expected the start task to finish")
        .expect("expected the turn to start");
    held.announcer.announce(
        json!({"method": "turn/completed", "params": {"threadId": held.thread,
            "turn": {"id": "real-turn", "status": "completed"}}})
        .to_string(),
    );

    let mut announced = String::new();
    let mut texts = Vec::new();
    while let Some(event) = tokio::time::timeout(Duration::from_secs(10), turn.recv())
        .await
        .expect("expected the turn to end within 10s of its completion")
    {
        match event.kind {
            EventKind::TurnStarted { native_turn_id } => announced = native_turn_id,
            EventKind::TextDelta { text } => texts.push(text),
            _ => {}
        }
    }
    held.session
        .close(CloseReason::Shutdown)
        .await
        .expect("expected the session to close");
    assert_eq!(
        (announced.as_str(), texts.as_slice()),
        (
            "real-turn",
            ["real-answer-before-response".to_owned()].as_slice()
        ),
        "expected the session's own native id and its own early text | received announced={announced} texts={texts:?}"
    );
}

#[tokio::test]
async fn a_cancel_in_the_start_window_targets_the_sessions_own_turn() {
    let held = HeldStart::open(
        |_| foreign_traffic(),
        |frame, own| {
            let id = frame["id"].clone();
            if frame["params"]["turnId"] == "real-turn" {
                vec![
                    json!({"id": id, "result": {}}).to_string(),
                    json!({"method": "turn/completed", "params": {"threadId": own,
                        "turn": {"id": "real-turn", "status": "interrupted"}}})
                    .to_string(),
                ]
            } else {
                vec![
                    json!({"id": id, "error": {"code": -32600, "message": "no such turn"}})
                        .to_string(),
                ]
            }
        },
    )
    .await;
    let starter = Arc::clone(&held.session);
    let starting = tokio::spawn(async move {
        starter
            .start_turn(TurnRequest::new("host-turn", "hello"))
            .await
    });
    held.until_start_is_read().await;
    let canceller = Arc::clone(&held.session);
    let cancel = tokio::spawn(async move { canceller.cancel(CancelReason::Requested).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    held.answer_start("real-turn");

    let cancelled = tokio::time::timeout(Duration::from_secs(10), cancel)
        .await
        .expect("expected the cancel to settle within 10s of the start answer")
        .expect("expected the cancel task to finish");
    let _ = tokio::time::timeout(Duration::from_secs(10), starting).await;
    let _ = held.session.close(CloseReason::Shutdown).await;

    let targets = held.interrupt_targets();
    assert!(
        !targets.iter().any(|target| target.contains("foreign-turn")),
        "expected no interrupt for another thread's turn | received targets={targets:?}"
    );
    assert!(
        targets.iter().any(|target| target.contains("real-turn")),
        "expected the session's own turn to be interrupted | received targets={targets:?} cancel={cancelled:?}"
    );
    assert!(
        cancelled.is_ok(),
        "expected the cancel to succeed against the session's own turn | received {cancelled:?}"
    );
}
