//! What an agent's own words are shown as: redacted of credential-shaped text, with the code and
//! the words around the credentials kept.

use super::*;

/// What an agent that failed while describing the request it was making might write, in the three
/// shapes the core's redactor corpus uses.
const CREDENTIAL_SHAPED: &str = "request refused: Authorization: Bearer top-secret-token API_KEY=another-secret redis://app:password@db/main unreachable";

/// The secrets inside [`CREDENTIAL_SHAPED`], none of which may reach a host.
const SECRETS: [&str; 3] = ["top-secret-token", "another-secret", "password"];

#[track_caller]
fn assert_redacted(what: &str, text: &str) {
    for secret in SECRETS {
        assert!(
            !text.contains(secret),
            "expected {what} without {secret:?} | received: {text:?}"
        );
    }
}

#[track_caller]
fn assert_words_kept(what: &str, text: &str) {
    assert!(
        text.contains("request refused: ") && text.contains("unreachable"),
        "expected the words around the credentials to stay readable in {what} | received: {text:?}"
    );
}

/// The failure a refused prompt ended the turn with, for an agent whose error is `error`.
async fn refused_prompt(error: serde_json::Value) -> mango_external_agents::VendorError {
    let refusing = ScriptedPeer::new(move |frame| match frame["method"].as_str() {
        Some("session/prompt") => vec![
            serde_json::json!({ "jsonrpc": "2.0", "id": frame["id"], "error": error }).to_string(),
        ],
        _ => Vec::new(),
    });
    let (session, _launcher) = open_under(refusing.process(), Limits::default()).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "try"))
        .await
        .expect("expected a turn");
    let events = drain(&mut turn).await;
    let Some(EventKind::Error { error }) = events.last() else {
        panic!(
            "expected the turn to end in an error | received: {:?}",
            event_labels(&events)
        );
    };
    error.clone()
}

/// The agent's message is the part of a refusal a person reads, and it is where an agent that
/// printed the request it was making leaves the header it sent.
#[tokio::test]
async fn an_agents_prompt_error_is_redacted_and_keeps_its_code_and_words() {
    let failure =
        refused_prompt(serde_json::json!({ "code": -31_000, "message": CREDENTIAL_SHAPED })).await;

    assert_redacted("a prompt error's message", &failure.message);
    assert_words_kept("a prompt error's message", &failure.message);
    let received = (
        failure.code.as_str(),
        failure.request_id.as_deref(),
        failure.vendor_code.as_deref(),
    );
    let expected = ("acp-request-failed", Some("session/prompt"), Some("-31000"));
    assert_eq!(
        received, expected,
        "expected (code, request, agent code): {expected:?} | received: {received:?}"
    );
}

/// `data` is appended to the message so an agent's explanation survives, so it is redacted with it.
#[tokio::test]
async fn what_an_agent_put_in_data_is_redacted_with_its_message() {
    let failure = refused_prompt(serde_json::json!({
        "code": -31_000,
        "message": "model busy",
        "data": { "detail": CREDENTIAL_SHAPED },
    }))
    .await;

    assert_redacted("a prompt error's data", &failure.message);
    assert!(
        failure.message.starts_with("model busy ("),
        "expected the message to lead and the data to follow | received: {:?}",
        failure.message
    );
    assert_words_kept("a prompt error's data", &failure.message);
}

/// A refusal of `session/new` is returned as an error rather than streamed, so it does not pass
/// the event sink, and has to be redacted where it is made.
#[tokio::test]
async fn an_agents_setup_error_is_redacted_and_keeps_its_code_and_words() {
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeAcpAgent::new()
            .refusing_new_session(-31_004, CREDENTIAL_SHAPED)
            .process(),
    );
    let error = refusal(
        AcpHarness::new(profile())
            .open_session(&host(&launcher), OpenSession::new("chat-1"))
            .await,
    );
    let Error::Vendor(failure) = error.cause() else {
        panic!("expected Error::Vendor | received: {error:?}");
    };

    assert_redacted("a setup error's message", &failure.message);
    assert_words_kept("a setup error's message", &failure.message);
    let received = (
        failure.code.as_str(),
        failure.request_id.as_deref(),
        failure.vendor_code.as_deref(),
    );
    let expected = ("acp-request-failed", Some("session/new"), Some("-31004"));
    assert_eq!(
        received, expected,
        "expected (code, request, agent code): {expected:?} | received: {received:?}"
    );
}

/// A notice is display-only agent text, shown beside an error.
#[tokio::test]
async fn an_agents_notice_is_redacted_and_keeps_its_words() {
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeAcpAgent::new()
            .with_updates(vec![serde_json::json!({
                "sessionUpdate": "notice",
                "severity": "warning",
                "title": "Sign-in failed: API_KEY=another-secret",
                "description": CREDENTIAL_SHAPED,
            })])
            .process(),
    );
    let session = AcpHarness::new(profile())
        .open_session(
            &host(&launcher),
            OpenSession::new("chat-1").with_configuration(permissive()),
        )
        .await
        .unwrap_or_else(|error| panic!("expected a session | received: {error:?}"));
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "try"))
        .await
        .expect("expected a turn");

    let events = drain(&mut turn).await;

    let Some(EventKind::Notice {
        title, description, ..
    }) = events
        .iter()
        .find(|kind| matches!(kind, EventKind::Notice { .. }))
    else {
        panic!("expected a notice | received: {:?}", event_labels(&events));
    };
    let description = description.clone().unwrap_or_default();
    assert_redacted("a notice title", title);
    assert_redacted("a notice description", &description);
    assert!(
        title.starts_with("Sign-in failed: "),
        "expected the title's words to stay readable | received: {title:?}"
    );
    assert_words_kept("a notice description", &description);
}

/// A JSON object names its keys in quotes, so `"api_key":"sk-..."` is not the `api_key=...` shape
/// the redactor reads. The agent's structured `data` is redacted by key before it is rendered,
/// whatever sits under the key and however the pair is written.
#[tokio::test]
async fn a_credential_in_data_is_redacted_and_its_siblings_are_kept() {
    let failure = refused_prompt(serde_json::json!({
        "code": -31_000,
        "message": "model busy",
        "data": {
            "api_key": "sk-live-0123456789",
            "headers": { "Authorization": "Bearer abc.def.ghi" },
            "attempts": [{ "refreshToken": "rt-0123456789", "path": "/repo/x.rs" }],
            "tokens": ["tk-0123456789"],
            "credentials": { "user": "u-0123456789", "pass": "pw-0123456789" },
            "env": [{ "name": "OPENAI_API_KEY", "value": "ev-0123456789" }],
            "note": "line one\nAuthorization:\nBearer nl-0123456789",
            "retry_after": 30,
        },
    }))
    .await;

    for secret in [
        "sk-live-0123456789",
        "abc.def.ghi",
        "rt-0123456789",
        "tk-0123456789",
        "u-0123456789",
        "pw-0123456789",
        "ev-0123456789",
        "nl-0123456789",
    ] {
        assert!(
            !failure.message.contains(secret),
            "expected data without {secret:?} | received: {:?}",
            failure.message
        );
    }
    for kept in ["model busy", "api_key", "/repo/x.rs", "\"retry_after\":30"] {
        assert!(
            failure.message.contains(kept),
            "expected data to keep {kept:?} | received: {:?}",
            failure.message
        );
    }
}

/// A message that puts a URL's password just before the bound, then its closing `@`: cut first,
/// the bound leaves `redis://app:hunter2`, which no rule reads as a credential.
fn password_at_the_bound(limit: mango_external_agents::normalize::TextLimit) -> String {
    let url = " redis://app:hunter2";
    format!(
        "{}{url}@db",
        "x".repeat(limit.max_code_points() - url.len())
    )
}

/// A refused setup request is returned as an error and never passes the sink, so the agent's
/// message is redacted and bounded where it is made, in that order, however large the frame was.
#[tokio::test]
async fn a_setup_error_is_redacted_before_it_is_bounded_and_then_bounded() {
    use mango_external_agents::normalize::TextLimit;
    let cases = [
        (
            "a password at the bound",
            password_at_the_bound(TextLimit::ErrorMessage),
        ),
        ("a very large message", "e".repeat(200_000)),
    ];
    for (what, message) in cases {
        let launcher = FakeLauncher::new();
        launcher.push(
            FakeAcpAgent::new()
                .refusing_new_session(-31_004, message)
                .process(),
        );
        let error = refusal(
            AcpHarness::new(profile())
                .open_session(&host(&launcher), OpenSession::new("chat-1"))
                .await,
        );
        let Error::Vendor(failure) = error.cause() else {
            panic!("expected Error::Vendor for {what} | received: {error:?}");
        };
        let bound = TextLimit::ErrorMessage.max_code_points();
        assert!(
            !failure.message.contains("hunter2") && failure.message.chars().count() <= bound,
            "expected {what} redacted and within {bound} code points | received {} code points ending {:?}",
            failure.message.chars().count(),
            &failure.message[failure.message.len().saturating_sub(40)..]
        );
    }
}
