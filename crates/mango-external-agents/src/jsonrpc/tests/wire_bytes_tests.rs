//! The exact bytes of every frame this side writes. The members go out in one fixed order,
//! `jsonrpc, id, method, params` and `jsonrpc, id, result | error`, and typed params and errors
//! keep their declared field order, whether or not some crate in the build turns on
//! `serde_json/preserve_order` (which switches `serde_json::Map` from sorted to insertion
//! order). `mango-agent-acp` turns it on, so `cargo nextest run --workspace` runs these with it,
//! and `cargo nextest run -p mango-external-agents` and the packaged core's run in
//! `scripts/test-prerelease-package.sh` run them without it.
//!
//! Every input here is a typed value or a `Value` with at most one member, so the expected
//! bytes do not depend on that switch either.

use super::outbox_tests::{options, sent, within};
use super::*;
use crate::jsonrpc::WireOptions;
use serde::Serialize;

const CANCEL: &str = "$/cancel_request";

/// Params whose declared field order is not their sorted order, so a frame that went through a
/// sorted map shows it.
#[derive(Serialize)]
struct PromptParams {
    session: &'static str,
    prompt: &'static str,
    budget: u32,
}

const PROMPT: PromptParams = PromptParams {
    session: "s-1",
    prompt: "hi",
    budget: 2,
};

/// The client's options, with or without the `"jsonrpc":"2.0"` member.
fn speaking(versioned: bool) -> ClientOptions {
    let mut speaking = options();
    speaking.include_version_header = versioned;
    speaking
}

fn connect_answering(
    versioned: bool,
    wire: WireOptions,
    answer: ServerRequestOutcome,
) -> (ScriptedLink, Arc<Client>) {
    let link = ScriptedLink::new();
    let handler = RecordingHandler::arc(Some(answer));
    let client = Arc::new(Client::connect_with(
        link.clone().into_link(),
        handler as Arc<dyn PeerHandler>,
        speaking(versioned),
        wire,
    ));
    (link, client)
}

/// `"jsonrpc":"2.0",` when the client writes the version member, nothing otherwise.
fn version(versioned: bool) -> &'static str {
    if versioned { r#""jsonrpc":"2.0","# } else { "" }
}

fn assert_written(link: &ScriptedLink, expected: &[String], case: &str) {
    let written = link.sent();
    assert_eq!(
        written, expected,
        "expected these exact frames for {case}: {expected:?} | received {written:?}"
    );
}

/// A request and a notification put the version first, then the id, the method and the params,
/// and the params keep their declared field order. Params that serialise to `null` are no member.
#[tokio::test]
async fn requests_and_notifications_keep_one_member_order() {
    for versioned in [true, false] {
        let (link, client) = connect_answering(
            versioned,
            WireOptions::new(),
            ServerRequestOutcome::Answer(json!("yes")),
        );
        let request = client
            .submit_request("session/prompt", PROMPT, RequestOptions::new())
            .expect("expected the request queued");
        let bare = client
            .submit_request("ping", (), RequestOptions::new())
            .expect("expected the bare request queued");
        let notified = client
            .submit_notification("session/update", PROMPT)
            .expect("expected the notification queued");
        let bare_notified = client
            .submit_notification("initialized", Option::<u32>::None)
            .expect("expected the bare notification queued");
        within("the notification written", bare_notified)
            .await
            .expect("expected the frames written");

        let v = version(versioned);
        let params = r#""params":{"session":"s-1","prompt":"hi","budget":2}"#;
        assert_written(
            &link,
            &[
                format!(r#"{{{v}"id":"1","method":"session/prompt",{params}}}"#),
                format!(r#"{{{v}"id":"2","method":"ping"}}"#),
                format!(r#"{{{v}"method":"session/update",{params}}}"#),
                format!(r#"{{{v}"method":"initialized"}}"#),
            ],
            &format!("requests and notifications (versioned: {versioned})"),
        );
        drop((request, bare, notified));
        client.close().await.expect("expected a clean close");
    }
}

/// An answer to the peer's question puts the version first, then the id as it arrived, then
/// the result.
#[tokio::test]
async fn an_answer_keeps_one_member_order() {
    for versioned in [true, false] {
        let (link, client) = connect_answering(
            versioned,
            WireOptions::new(),
            ServerRequestOutcome::Answer(json!({ "granted": true })),
        );
        link.push_line(r#"{"jsonrpc":"2.0","id":7,"method":"ask","params":{}}"#);
        sent(&link, 1).await;

        let v = version(versioned);
        assert_written(
            &link,
            &[format!(r#"{{{v}"id":7,"result":{{"granted":true}}}}"#)],
            &format!("an answer (versioned: {versioned})"),
        );
        client.close().await.expect("expected a clean close");
    }
}

/// A refusal of the peer's question puts the version first, then the id, then the error, whose
/// members go `code, message, data`.
#[tokio::test]
async fn a_refusal_keeps_one_member_order() {
    for versioned in [true, false] {
        let refusal = JsonRpcError {
            code: -32000,
            message: String::from("refused"),
            data: Some(json!({ "reason": "policy" })),
        };
        let (link, client) = connect_answering(
            versioned,
            WireOptions::new(),
            ServerRequestOutcome::Failure(refusal),
        );
        link.push_line(r#"{"jsonrpc":"2.0","id":"q-1","method":"ask","params":{}}"#);
        sent(&link, 1).await;

        let v = version(versioned);
        let error = r#"{"code":-32000,"message":"refused","data":{"reason":"policy"}}"#;
        assert_written(
            &link,
            &[format!(r#"{{{v}"id":"q-1","error":{error}}}"#)],
            &format!("a refusal (versioned: {versioned})"),
        );
        client.close().await.expect("expected a clean close");
    }
}

/// The errors this side writes unasked, for a line that is not JSON and for JSON that is not a
/// request, put the version first, then the id, then the error.
#[tokio::test]
async fn unasked_errors_keep_one_member_order() {
    for versioned in [true, false] {
        let (link, client) = connect_answering(
            versioned,
            WireOptions::new()
                .with_parse_error_replies(4)
                .with_invalid_request_replies(4),
            ServerRequestOutcome::Answer(json!("yes")),
        );
        link.push_line("{oops");
        sent(&link, 1).await;
        link.push_line(r#"{"jsonrpc":"2.0","id":9,"method":5}"#);
        sent(&link, 2).await;

        let v = version(versioned);
        assert_written(
            &link,
            &[
                format!(r#"{{{v}"id":null,"error":{{"code":-32700,"message":"Parse error"}}}}"#),
                format!(r#"{{{v}"id":9,"error":{{"code":-32600,"message":"Invalid Request"}}}}"#),
            ],
            &format!("unasked errors (versioned: {versioned})"),
        );
        client.close().await.expect("expected a clean close");
    }
}

/// The cancel this side sends for a request it gave up is a notification like any other.
#[tokio::test]
async fn the_cancel_for_a_request_given_up_keeps_one_member_order() {
    for versioned in [true, false] {
        let (link, client) = connect_answering(
            versioned,
            WireOptions::new().with_cancel_on_abandon(CANCEL),
            ServerRequestOutcome::Answer(json!("yes")),
        );
        let (written, reply) = client
            .submit_request("ask", PROMPT, RequestOptions::new())
            .expect("expected the request queued")
            .into_parts();
        within("the request written", written)
            .await
            .expect("expected the request written");
        drop(reply);
        sent(&link, 2).await;

        let v = version(versioned);
        let cancel = link.sent().pop().expect("expected a second frame");
        let expected = format!(r#"{{{v}"method":"{CANCEL}","params":{{"requestId":"1"}}}}"#);
        assert_eq!(
            cancel, expected,
            "expected the cancel's exact bytes (versioned: {versioned}): {expected} | received {cancel}"
        );
        client.close().await.expect("expected a clean close");
    }
}
