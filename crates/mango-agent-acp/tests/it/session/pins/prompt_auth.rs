//! What a host is told when an agent answers `session/prompt` with ACP's authentication-required
//! error, and what it is not told.

use super::*;
use mango_external_agents::VendorError;
use mango_external_agents::normalize::TextLimit;

/// ACP's "authentication required" code.
const AUTH_REQUIRED: i32 = -32_000;

/// Credential-shaped text an agent might put in its own error, in each place it can.
const AGENT_SECRET: &str = "sk-agent-secret-9f2c";

/// The profile's own sign-in command, host-owned text.
const LOGIN_HINT: &str = "fake-acp auth login";

fn signed_in_profile(hint: &str) -> Arc<AcpProfile> {
    Arc::new(
        AcpProfile::custom("fake", ["fake-acp", "acp"], VENDOR).with_login_hint(hint.to_owned()),
    )
}

/// A session over `agent` under a profile whose login command is `hint`.
async fn open_with_hint(agent: FakeAcpAgent, hint: &str) -> Box<dyn Session> {
    let launcher = FakeLauncher::new();
    launcher.push(agent.process());
    AcpHarness::new(signed_in_profile(hint))
        .open_session(
            &host(&launcher),
            OpenSession::new("chat-1").with_configuration(permissive()),
        )
        .await
        .unwrap_or_else(|error| panic!("expected a session | received: {error:?}"))
}

/// Runs one turn to its terminal and returns the events, ending in the terminal.
async fn one_turn(session: &dyn Session, turn_id: &str) -> Vec<EventKind> {
    let mut turn = start_when_free(session, turn_id, "try").await;
    drain(&mut turn).await
}

/// The failure a turn ended with.
#[track_caller]
fn terminal_failure(events: &[EventKind]) -> &VendorError {
    let Some(EventKind::Error { error }) = events.last() else {
        panic!(
            "expected the turn to end in EventKind::Error | received: {:?}",
            event_labels(events)
        );
    };
    error
}

/// A mid-session `-32000` is a distinct, stable code on the terminal, so a host branches on it
/// the way it branches on `Error::AuthRequired` from `open_session`, and its message carries the
/// profile's own sign-in command.
#[tokio::test]
async fn an_auth_required_prompt_ends_the_turn_with_the_profiles_login_hint() {
    let session = open_with_hint(
        FakeAcpAgent::new().refusing_prompt(AUTH_REQUIRED, "token expired"),
        LOGIN_HINT,
    )
    .await;

    let events = one_turn(session.as_ref(), "turn-1").await;
    assert_events(
        "the turn whose prompt needs a sign-in",
        &events,
        &["TurnStarted", "Error(acp-auth-required)"],
    );
    let failure = terminal_failure(&events);
    let received = (
        failure.request_id.as_deref(),
        failure.vendor_code.as_deref(),
        failure.retryable,
        failure.message.contains(LOGIN_HINT),
    );
    let expected = (Some("session/prompt"), Some("-32000"), false, true);
    assert_eq!(
        received, expected,
        "expected (request, agent code, retryable, message names {LOGIN_HINT:?}): {expected:?} | received: {received:?} with message {:?}",
        failure.message
    );
}

/// The agent's own words are not shown: the sign-in command is the profile's, and an agent's
/// message or `data` can carry whatever it was holding when the token failed.
#[tokio::test]
async fn credential_shaped_text_in_the_agents_auth_error_never_reaches_the_host() {
    let message =
        format!("token expired Authorization: Bearer {AGENT_SECRET} API_KEY={AGENT_SECRET}");
    let session = open_with_hint(
        FakeAcpAgent::new().refusing_prompt(AUTH_REQUIRED, message),
        LOGIN_HINT,
    )
    .await;

    let events = one_turn(session.as_ref(), "turn-1").await;
    let failure = terminal_failure(&events);
    let shown = serde_json::to_string(failure).unwrap_or_default();
    assert!(
        !shown.contains(AGENT_SECRET),
        "expected no credential-shaped agent text in the terminal | received: {shown}"
    );
    assert!(
        !shown.is_empty() && failure.message.contains(LOGIN_HINT),
        "expected the terminal to still carry the hint | received: {shown}"
    );
}

/// A custom profile's hint is caller-owned and can be long; the terminal bounds it like any other
/// error message and strips what a terminal would interpret.
#[tokio::test]
async fn a_long_or_hostile_login_hint_is_bounded_on_the_terminal() {
    let hint = format!("run\u{7}\u{1b}[31m login {}", "x".repeat(10_000));
    let session = open_with_hint(
        FakeAcpAgent::new().refusing_prompt(AUTH_REQUIRED, "token expired"),
        &hint,
    )
    .await;

    let events = one_turn(session.as_ref(), "turn-1").await;
    let failure = terminal_failure(&events);
    let bound = TextLimit::ErrorMessage.max_code_points();
    let received = (
        failure.message.chars().count() <= bound,
        failure.message.contains(['\u{7}', '\u{1b}']),
        failure.message.contains("login xxxx"),
    );
    assert_eq!(
        received,
        (true, false, true),
        "expected (within {bound} code points, no control characters, the hint's start kept) | received: {received:?}"
    );
}

/// After the sign-in error the session is still `Ready`: the agent answered, the connection is
/// as healthy as after any answer, and a host that signed the vendor CLI in out of band prompts
/// again on it.
#[tokio::test]
async fn the_session_stays_ready_and_takes_the_next_prompt_after_an_auth_error() {
    let session = open_with_hint(
        FakeAcpAgent::new().refusing_prompt(AUTH_REQUIRED, "token expired"),
        LOGIN_HINT,
    )
    .await;

    let first = one_turn(session.as_ref(), "turn-1").await;
    assert_status(
        "after the sign-in error",
        session.snapshot().status,
        SessionStatus::Ready,
    );
    let second = one_turn(session.as_ref(), "turn-2").await;
    assert_eq!(
        (event_labels(&first).pop(), event_labels(&second).pop()),
        (
            Some(String::from("Error(acp-auth-required)")),
            Some(String::from("Error(acp-auth-required)"))
        ),
        "expected both turns to end in the sign-in error"
    );
}

/// Only the authentication code is special: any other refusal of a prompt is unchanged, with the
/// agent's code and message.
#[tokio::test]
async fn every_other_prompt_error_stays_acp_request_failed() {
    for code in [-31_000, -32_603, -32_001] {
        let session = open_with_hint(
            FakeAcpAgent::new().refusing_prompt(code, "model busy"),
            LOGIN_HINT,
        )
        .await;

        let events = one_turn(session.as_ref(), "turn-1").await;
        let failure = terminal_failure(&events);
        let received = (
            failure.code.as_str(),
            failure.request_id.as_deref(),
            failure.vendor_code.clone(),
            failure.message.as_str(),
        );
        let expected = (
            "acp-request-failed",
            Some("session/prompt"),
            Some(code.to_string()),
            "model busy",
        );
        assert_eq!(
            received, expected,
            "expected (code, request, agent code, message) for {code}: {expected:?} | received: {received:?}"
        );
    }
}

/// A profile with no login command of its own carries its documentation link instead, as
/// `open_session` does for `Error::AuthRequired`: printing a plausible command would send a
/// person to a prompt that does not exist.
#[tokio::test]
async fn a_profile_without_a_login_command_gets_its_documentation_link() {
    let profile = Arc::new(AcpProfile::custom("fake", ["fake-acp", "acp"], VENDOR));
    let expected = profile.login_text();
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeAcpAgent::new()
            .refusing_prompt(AUTH_REQUIRED, "token expired")
            .process(),
    );
    let session = AcpHarness::new(profile)
        .open_session(
            &host(&launcher),
            OpenSession::new("chat-1").with_configuration(permissive()),
        )
        .await
        .unwrap_or_else(|error| panic!("expected a session | received: {error:?}"));

    let events = one_turn(session.as_ref(), "turn-1").await;
    let failure = terminal_failure(&events);
    assert!(
        expected.starts_with("see ") && failure.message.ends_with(&expected),
        "expected a message ending with the documentation text {expected:?} | received: {:?}",
        failure.message
    );
}
