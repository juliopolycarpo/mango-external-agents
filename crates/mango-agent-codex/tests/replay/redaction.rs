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
    message: String,
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
                    "error": {"code": -32603, "message": self.message},
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
                            "error": {"message": self.message},
                        },
                    },
                })
                .to_string(),
            ],
        })
    }
}

async fn session_over(failure: Failure) -> Box<dyn Session> {
    session_saying(failure, CREDENTIAL_SHAPED).await
}

/// A session whose server fails the one turn it is asked for, with `message`.
async fn session_saying(failure: Failure, message: &str) -> Box<dyn Session> {
    let transcript = Transcript::load("turn");
    let server = Arc::new(FailingServer {
        thread_id: transcript
            .thread_id()
            .expect("expected the recording to name its thread"),
        failure,
        message: message.to_owned(),
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

/// The app-server's missing-rollout answer is recognised by its exact text, and a thread id that is
/// itself credential-shaped is redacted out of that text on its way to the harness, so the
/// comparison is made against the text in the form that arrives.
#[tokio::test]
async fn a_credential_shaped_thread_id_still_falls_back_to_a_fresh_thread() {
    let launcher = Arc::new(FakeLauncher::new());
    let missing = "api_key=0123456789abcdef";
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

    assert!(
        !session.snapshot().resumed && session.snapshot().fallback_reason.is_some(),
        "expected a fresh thread with a fallback reason | received: resumed {} reason {:?}",
        session.snapshot().resumed,
        session.snapshot().fallback_reason
    );
    assert!(
        launcher
            .written()
            .iter()
            .any(|line| line.contains("thread/start")),
        "expected thread/start after the missing rollout | received: {:?}",
        launcher.written()
    );
}

/// Every shape the redactor reads, through both of the doors a server's message uses, and an
/// ordinary server message through the failed turn unchanged.
#[tokio::test]
async fn every_shape_in_the_corpus_is_redacted_through_both_doors() {
    use mango_external_agents::testing::redaction::{BENIGN, LEAKS};
    for leak in LEAKS {
        let session = session_saying(Failure::TurnFailed, leak.text).await;
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "try"))
            .await
            .expect("expected the server's acceptance before its failure");
        let events = drain(&mut turn).await;
        let Some(EventKind::Error { error }) = events.last() else {
            panic!("expected the turn to end in an error | received: {events:#?}");
        };
        assert!(
            !error.message.contains(leak.secret),
            "expected a failed turn for {:?} without {:?} | received: {:?}",
            leak.text,
            leak.secret,
            error.message
        );

        let session = session_saying(Failure::StartRefused, leak.text).await;
        let Err(failure) = session.start_turn(TurnRequest::new("turn-1", "try")).await else {
            panic!("expected the server's refusal of turn/start to fail the start");
        };
        let Error::Vendor(error) = failure.cause() else {
            panic!("expected a vendor error | received: {failure:?}");
        };
        assert!(
            !error.message.contains(leak.secret),
            "expected a refused request for {:?} without {:?} | received: {:?}",
            leak.text,
            leak.secret,
            error.message
        );
        for kept in leak.keeps {
            assert!(
                error.message.contains(kept),
                "expected a refused request for {:?} to keep {kept:?} | received: {:?}",
                leak.text,
                error.message
            );
        }
    }
    for text in BENIGN {
        let session = session_saying(Failure::StartRefused, text).await;
        let Err(failure) = session.start_turn(TurnRequest::new("turn-1", "try")).await else {
            panic!("expected the server's refusal of turn/start to fail the start");
        };
        let Error::Vendor(error) = failure.cause() else {
            panic!("expected a vendor error | received: {failure:?}");
        };
        assert_eq!(
            &error.message, text,
            "expected an ordinary refusal to reach the host unchanged"
        );
    }
}
