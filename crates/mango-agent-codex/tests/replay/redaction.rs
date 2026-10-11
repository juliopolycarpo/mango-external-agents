//! A vendor's error text reaches the host redacted of credential-shaped text, with the words
//! around the credentials kept.

use super::*;
use mango_external_agents::Error;

/// A message that describes the request the server was making, headers included.
const CREDENTIAL_SHAPED: &str = "request refused: Authorization: Bearer top-secret-token API_KEY=another-secret redis://app:password@db/main unreachable";

/// The secrets inside [`CREDENTIAL_SHAPED`], none of which may reach a host.
const SECRETS: [&str; 3] = ["top-secret-token", "another-secret", "password"];

/// How the server answers the one `turn/start` it is asked for.
#[derive(Clone, Copy)]
enum Failure {
    /// It accepts the turn and then ends it as failed, with the message in the turn's error.
    TurnFailed,
    /// It refuses the request itself, with the message in the JSON-RPC error.
    StartRefused,
}

struct FailingServer {
    thread_id: String,
    failure: Failure,
    started: AtomicBool,
}

impl FailingServer {
    fn respond(&self, frame: &serde_json::Value) -> Option<Vec<String>> {
        let method = frame.get("method").and_then(serde_json::Value::as_str);
        if method != Some("turn/start") || self.started.swap(true, Ordering::SeqCst) {
            return None;
        }
        let id = frame.get("id").cloned().unwrap_or(serde_json::Value::Null);
        Some(match self.failure {
            Failure::StartRefused => vec![
                serde_json::json!({
                    "id": id,
                    "error": {"code": -32603, "message": CREDENTIAL_SHAPED},
                })
                .to_string(),
            ],
            Failure::TurnFailed => vec![
                serde_json::json!({"id": id, "result": {"turn": {"id": "redacted-failure"}}})
                    .to_string(),
                serde_json::json!({
                    "method": "turn/completed",
                    "params": {
                        "threadId": self.thread_id,
                        "turn": {
                            "id": "redacted-failure",
                            "status": "failed",
                            "error": {"message": CREDENTIAL_SHAPED},
                        },
                    },
                })
                .to_string(),
            ],
        })
    }
}

async fn session_over(failure: Failure) -> Box<dyn Session> {
    let transcript = Transcript::load("turn");
    let server = Arc::new(FailingServer {
        thread_id: transcript
            .thread_id()
            .expect("expected the recording to name its thread"),
        failure,
        started: AtomicBool::new(false),
    });
    let launcher = Arc::new(FakeLauncher::new());
    launcher.push(transcript.as_process_intercepting(move |frame| server.respond(frame)));
    let (host, _launcher) = with_launcher(Arc::clone(&launcher), None);
    CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session")
}

#[track_caller]
fn assert_redacted(what: &str, message: &str) {
    for secret in SECRETS {
        assert!(
            !message.contains(secret),
            "expected {what} without {secret:?} | received: {message:?}"
        );
    }
    assert!(
        message.contains("request refused: ") && message.ends_with("unreachable"),
        "expected the words around the credentials to stay readable in {what} | received: {message:?}"
    );
}

#[tokio::test]
async fn a_failed_turns_message_is_redacted_before_the_host_sees_it() {
    let session = session_over(Failure::TurnFailed).await;

    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "try"))
        .await
        .expect("expected the server's acceptance before its failure");
    let events = drain(&mut turn).await;

    let Some(EventKind::Error { error }) = events.last() else {
        panic!("expected the turn to end in an error | received: {events:#?}");
    };
    assert_redacted("a failed turn's message", &error.message);
}

#[tokio::test]
async fn a_refused_requests_message_is_redacted_before_the_host_sees_it() {
    let session = session_over(Failure::StartRefused).await;

    let Err(failure) = session.start_turn(TurnRequest::new("turn-1", "try")).await else {
        panic!("expected the server's refusal of turn/start to fail the start");
    };
    let Error::Vendor(error) = failure.cause() else {
        panic!("expected a vendor error | received: {failure:?}");
    };
    assert_redacted("a refused request's message", &error.message);
}
