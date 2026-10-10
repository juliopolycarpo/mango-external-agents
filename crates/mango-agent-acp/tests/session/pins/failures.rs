//! What a host is told when the link, the agent or a deadline fails a request or a turn.

use super::*;

/// Credential-shaped text an agent might print before it dies, next to a line worth keeping.
const STDERR: &str = "panicked at src/main.rs:7\nAuthorization: Bearer top-secret-token";

/// Asserts `failure` is the closed-link failure carrying the redacted tail of [`STDERR`].
///
/// The correlation fields are part of the pin: a closed link is not an agent's refusal, so it
/// names no request and no vendor code, and nothing about it suggests signing in.
fn assert_closed_link_with_stderr(failure: &mango_external_agents::VendorError, prefix: &str) {
    let received = format!(
        "code {:?}, request_id {:?}, vendor_code {:?}, retryable {}, message {:?}",
        failure.code.as_str(),
        failure.request_id,
        failure.vendor_code,
        failure.retryable,
        failure.message
    );
    assert!(
        failure.code.as_str() == "acp-link-closed"
            && failure.request_id.is_none()
            && failure.vendor_code.is_none()
            && !failure.retryable,
        "expected code \"acp-link-closed\", request_id None, vendor_code None, retryable false | received: {received}"
    );
    assert!(
        failure.message.starts_with(prefix)
            && failure
                .message
                .contains("; the agent's stderr: panicked at src/main.rs:7"),
        "expected a message starting {prefix:?} and carrying the stderr tail | received: {received}"
    );
    assert!(
        !failure.message.contains("top-secret-token"),
        "expected the credential redacted from the stderr tail | received: {received}"
    );
}

/// A session with a prompt in flight on an agent whose stdout has just ended, and the turn it
/// left behind, read as far as the partial answer.
async fn turn_on_an_agent_that_just_died() -> (Box<dyn Session>, FakeLauncher, TurnStream) {
    let gone = CancelToken::new();
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeAcpAgent::new()
            .with_updates(vec![text_chunk("partial")])
            .never_finishing_turns()
            .process()
            .ending_stdout_when(gone.clone())
            .with_stderr(STDERR),
    );
    let session = AcpHarness::new(profile())
        .open_session(
            &host(&launcher),
            OpenSession::new("chat-1").with_configuration(permissive()),
        )
        .await
        .expect("expected a session");
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "start working"))
        .await
        .expect("expected a turn");
    read_until(&mut turn, "the partial answer", |kind| {
        matches!(kind, EventKind::TextDelta { .. })
    })
    .await;
    gone.cancel();
    (session, launcher, turn)
}

/// What is left of a turn, after its partial answer, once the agent's stdout has ended.
const DEAD_AGENT_TURN: [&str; 1] = ["Error(acp-link-closed)"];

/// An agent whose stdout ends while a prompt is in flight has not been cancelled by anyone: the
/// turn fails as a closed link, with the agent's redacted stderr on the typed message, and the
/// session ends `Closed` with its child reaped.
///
/// Deterministic on the current-thread runtime this runs on. On a multi-thread runtime the same
/// death is sometimes reported as a cancellation today; the ignored test below records that.
#[tokio::test]
async fn an_agent_whose_output_ends_mid_prompt_fails_the_turn_as_a_closed_link() {
    let (session, launcher, mut turn) = turn_on_an_agent_that_just_died().await;
    let mut lifecycle = session.subscribe();

    let rest = drain(&mut turn).await;
    assert_events("the rest of the turn", &rest, &DEAD_AGENT_TURN);
    let Some(EventKind::Error { error }) = rest.last() else {
        unreachable!("the events above end in an error");
    };
    assert_closed_link_with_stderr(error, "fake: ");

    assert_status(
        "after the link closed",
        status_once_settled(&mut lifecycle).await,
        SessionStatus::Closed,
    );
    assert_no_live_children("after the link closed", &launcher);
    let next = refusal(
        session
            .start_turn(TurnRequest::new("turn-2", "again"))
            .await,
    );
    assert!(
        matches!(next.cause(), Error::Closed { subject: "session" })
            && next.dispatch() == Dispatch::NotSubmitted,
        "expected the next turn: Closed {{ subject: \"session\" }}, NotSubmitted | received: {next:?} ({:?})",
        next.dispatch()
    );
}

/// The same death on a multi-thread runtime, repeated so the race it loses shows up in one run.
///
/// The prompt task waits on the prompt's own failure and on the end of the dispatch loop with
/// equal priority. When the lifecycle watcher has already wound the loop down by the time that
/// task is polled, it can take the loop's end, finds no failure and no cancel reason, and ends
/// the turn as `Cancelled(Requested)`: a host is told it stopped a turn whose agent died.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "an agent death that races the lifecycle watcher is reported as Cancelled(Requested) on a multi-thread runtime"]
async fn an_agent_death_mid_prompt_is_never_reported_as_a_cancellation() {
    for round in 0..200 {
        let (_session, _launcher, mut turn) = turn_on_an_agent_that_just_died().await;
        assert_events(
            &format!("the rest of the turn in round {round}"),
            &drain(&mut turn).await,
            &DEAD_AGENT_TURN,
        );
    }
}

/// A peer that answers the handshake up to `dies_under` and closes its stdout when it reads
/// that request, leaving [`STDERR`] behind.
fn dying_under(dies_under: &'static str) -> FakeProcess {
    let gone = CancelToken::new();
    let dying = gone.clone();
    let peer = ScriptedPeer::new(|_| Vec::new());
    FakeProcess::responding(move |line| {
        let method = serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|frame| frame["method"].as_str().map(str::to_owned));
        if method.as_deref() == Some(dies_under) {
            dying.cancel();
            return Vec::new();
        }
        peer.answer(line)
    })
    .ending_stdout_when(gone)
    .with_stderr(STDERR)
}

/// An agent that exits under `initialize` or `session/new` fails the open as a closed link
/// naming the request it died under, with its redacted stderr. It is never a sign-in hint: an
/// agent that crashed has not said it wants credentials.
#[tokio::test]
async fn an_agent_that_exits_during_the_handshake_fails_the_open_as_a_closed_link() {
    for method in ["initialize", "session/new"] {
        let launcher = FakeLauncher::new();
        launcher.push(dying_under(method));
        let error = refusal(
            AcpHarness::new(profile())
                .open_session(&host(&launcher), OpenSession::new("chat-1"))
                .await,
        );
        let Error::Vendor(failure) = error.cause() else {
            panic!("expected Error::Vendor for an exit under {method} | received: {error:?}");
        };
        assert_closed_link_with_stderr(failure, &format!("a transport that closed under {method}"));
        assert_no_live_children(&format!("after an exit under {method}"), &launcher);
    }
}

/// An agent's own refusal of `session/new` reaches the host as `acp-request-failed`, correlated
/// to the method, with the agent's numeric code as the vendor code and its message kept. A code
/// outside the reserved JSON-RPC range may be retried; one inside it may not.
#[tokio::test]
async fn an_agents_refusal_of_session_new_keeps_its_method_code_and_message() {
    for (code, retryable) in [(-31_004, true), (-32_603, false)] {
        let launcher = FakeLauncher::new();
        launcher.push(
            FakeAcpAgent::new()
                .refusing_new_session(code, "no workspace")
                .process(),
        );
        let error = refusal(
            AcpHarness::new(profile())
                .open_session(&host(&launcher), OpenSession::new("chat-1"))
                .await,
        );
        let Error::Vendor(failure) = error.cause() else {
            panic!("expected Error::Vendor for code {code} | received: {error:?}");
        };
        let received = (
            failure.code.as_str(),
            failure.request_id.as_deref(),
            failure.vendor_code.clone(),
            failure.message.as_str(),
            failure.retryable,
        );
        let expected = (
            "acp-request-failed",
            Some("session/new"),
            Some(code.to_string()),
            "no workspace",
            retryable,
        );
        assert_eq!(
            received, expected,
            "expected (code, request_id, vendor_code, message, retryable): {expected:?} | received: {received:?}"
        );
        assert_no_live_children("after a refused open", &launcher);
    }
}

/// Only `-32002` on `session/load` is a conclusive "that session is gone". Any other refusal
/// leaves the resume failed even under `ResumeMode::Fallback`: a fresh conversation in place of
/// one that may still exist would lose the history without cause.
#[tokio::test]
async fn a_load_refused_with_another_code_is_not_a_fallback() {
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeAcpAgent::new()
            .refusing_load_session(-31_000, "busy")
            .process(),
    );
    let error = refusal(
        AcpHarness::new(profile())
            .open_session(
                &host(&launcher),
                OpenSession::new("chat-1").resuming("sess_old", ResumeMode::Fallback),
            )
            .await,
    );
    let Error::Vendor(failure) = error.cause() else {
        panic!("expected Error::Vendor | received: {error:?}");
    };
    let received = (
        failure.code.as_str(),
        failure.request_id.as_deref(),
        failure.vendor_code.as_deref(),
    );
    let expected = ("acp-request-failed", Some("session/load"), Some("-31000"));
    assert_eq!(
        received, expected,
        "expected (code, request_id, vendor_code): {expected:?} | received: {received:?}"
    );
    // No `session/new` follows: the refused load is the last thing the agent was asked.
    assert_wire_after(&launcher, "initialize", &["session/load"]);
}

/// The code mangostudio reads from `mango_agent_acp::reducer`, held to its wire spelling.
#[test]
fn the_incomplete_turn_code_keeps_its_spelling() {
    let received = mango_agent_acp::reducer::TURN_INCOMPLETE_CODE;
    let expected = "vendor-turn-incomplete";
    assert_eq!(
        received, expected,
        "expected TURN_INCOMPLETE_CODE: {expected:?} | received: {received:?}"
    );
}

/// A request that is not a prompt is given `Limits::request_timeout`. Passing it is a typed
/// timeout naming the method and the deadline, and it ends the connection: admission closes at
/// once, the child is reaped and the session reaches `Closed`.
#[tokio::test(start_paused = true)]
async fn a_request_past_its_deadline_times_out_and_ends_the_connection() {
    let deadline = Duration::from_secs(5);
    let (session, launcher) = open_under(
        FakeAcpAgent::new().holding_listing().process(),
        Limits {
            request_timeout: deadline,
            ..Limits::default()
        },
    )
    .await;
    let mut lifecycle = session.subscribe();

    let begun = tokio::time::Instant::now();
    let error = refusal(session.list_sessions(Default::default()).await);
    let waited = begun.elapsed();
    let Error::Timeout { operation, after } = error.cause() else {
        panic!("expected Error::Timeout | received: {error:?}");
    };
    let received = (operation.as_str(), *after, waited);
    let expected = ("session/list on an ACP agent", deadline, deadline);
    assert_eq!(
        received, expected,
        "expected (operation, after, time waited): {expected:?} | received: {received:?}"
    );

    let next = refusal(session.list_sessions(Default::default()).await);
    assert!(
        matches!(
            next.cause(),
            Error::Closed {
                subject: "ACP connection"
            }
        ),
        "expected the next request: Closed {{ subject: \"ACP connection\" }} | received: {next:?}"
    );
    assert_status(
        "after a timed-out request",
        status_once_settled(&mut lifecycle).await,
        SessionStatus::Closed,
    );
    assert_no_live_children("after a timed-out request", &launcher);
    assert_first_frame_after(&launcher, "session/new", "session/list");
}

/// `session/prompt` has no deadline of its own. A turn that reports nothing for longer than
/// `request_timeout` keeps running, and what ends it is the idle deadline, as a cancellation
/// naming the timeout.
#[tokio::test(start_paused = true)]
async fn a_quiet_prompt_outlives_the_request_timeout_and_ends_at_the_idle_deadline() {
    let request_timeout = Duration::from_secs(5);
    let idle_timeout = Duration::from_secs(60);
    let (session, launcher) = open_under(
        FakeAcpAgent::new()
            .with_updates(Vec::new())
            .staying_silent()
            .process(),
        Limits {
            request_timeout,
            idle_timeout,
            ..Limits::default()
        },
    )
    .await;

    let begun = tokio::time::Instant::now();
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "think for a while"))
        .await
        .expect("expected a turn");
    let events = drain_within(&mut turn, 10 * idle_timeout).await;
    let waited = begun.elapsed();

    assert_events(
        "events",
        &events,
        &["TurnStarted", "Cancelled(Timeout)", "Completed"],
    );
    let expected = idle_timeout;
    assert_eq!(
        waited, expected,
        "expected the time the turn ran: {expected:?} | received: {waited:?} (request_timeout is {request_timeout:?})"
    );
    assert_status(
        "after an idle cancel the agent honoured",
        session.snapshot().status,
        SessionStatus::Ready,
    );
    assert_wire_after(&launcher, "session/prompt", &["session/cancel"]);
}
