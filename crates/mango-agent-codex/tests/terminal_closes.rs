//! What a host is left holding when a Codex turn is cancelled while it still has work open.
//!
//! The fake app-server announces a reasoning phase and a running command, then answers the
//! interrupt with an interrupted `turn/completed` and nothing for the two open items, which is the
//! one shape a host can always produce: it can cancel at any moment. The host must see both closed
//! before the terminal, or its transcript keeps a spinner nobody will ever stop.

// Shared with the replay suite; this binary uses only the intercepting replay.
#[allow(dead_code)]
mod support;

use std::sync::Arc;
use std::time::Duration;

use mango_agent_codex::CodexHarness;
use mango_external_agents::event::ActivityStatus;
use mango_external_agents::testing::FakeLauncher;
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

/// Names the events a host receives, with each activity close's status, for a failure message.
fn shape(events: &[EventKind]) -> Vec<String> {
    events
        .iter()
        .map(|kind| match kind {
            EventKind::ActivityCompleted { call_id, result } => {
                format!("ActivityCompleted({call_id}, {:?})", result.status)
            }
            EventKind::ActivityStarted { call_id, .. } => format!("ActivityStarted({call_id})"),
            other => format!("{other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn a_host_cancel_closes_the_open_activity_and_reasoning_before_the_terminal() {
    let transcript = Transcript::load("turn");
    let thread = transcript
        .thread_id()
        .expect("expected the recording to name its thread");
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(transcript.as_process_intercepting(move |frame| {
        let id = frame["id"].clone();
        match frame.get("method").and_then(Value::as_str) {
            Some("turn/start") => Some(vec![
                json!({"id": id, "result": {"turn": {"id": "real-turn"}}}).to_string(),
                json!({"method": "item/started", "params": {"threadId": thread,
                    "turnId": "real-turn", "item": {"type": "reasoning", "id": "r1",
                    "summary": [], "content": []}}})
                .to_string(),
                json!({"method": "item/started", "params": {"threadId": thread,
                    "turnId": "real-turn", "item": {"type": "commandExecution", "id": "c1",
                    "command": "sleep 1000", "status": "inProgress"}}})
                .to_string(),
            ]),
            Some("turn/interrupt") => Some(vec![
                json!({"id": id, "result": {}}).to_string(),
                json!({"method": "turn/completed", "params": {"threadId": thread,
                    "turn": {"id": "real-turn", "status": "interrupted"}}})
                .to_string(),
            ]),
            _ => None,
        }
    }));
    let session: Arc<dyn Session> = Arc::from(
        CodexHarness::new()
            .open_session(&host(launcher), OpenSession::new("chat-1"))
            .await
            .expect("expected a session"),
    );
    let mut turn = session
        .start_turn(TurnRequest::new("host-turn", "hello"))
        .await
        .expect("expected the turn to start");

    let mut events = Vec::new();
    let mut cancelled = false;
    while let Some(event) = tokio::time::timeout(Duration::from_secs(10), turn.recv())
        .await
        .expect("expected the turn to end within 10s of its cancel")
    {
        let started = matches!(event.kind, EventKind::ActivityStarted { .. });
        events.push(event.kind);
        if started && !cancelled {
            cancelled = true;
            let canceller = Arc::clone(&session);
            tokio::spawn(async move { canceller.cancel(CancelReason::Requested).await });
        }
    }
    session
        .close(CloseReason::Shutdown)
        .await
        .expect("expected the session to close");

    let shape = shape(&events);
    let closes: Vec<&String> = shape
        .iter()
        .filter(|name| name.starts_with("ActivityCompleted") || *name == "ReasoningEnded")
        .collect();
    assert_eq!(
        closes,
        [
            &format!("ActivityCompleted(c1, {:?})", ActivityStatus::Cancelled),
            &String::from("ReasoningEnded")
        ],
        "expected one activity close and one reasoning close before the terminal | received {shape:?}"
    );
    let terminal = shape.iter().position(|name| name == "Completed");
    let last_close = shape.iter().rposition(|name| closes.contains(&name));
    assert!(
        terminal > last_close,
        "expected both closes before the terminal | received {shape:?}"
    );
}
