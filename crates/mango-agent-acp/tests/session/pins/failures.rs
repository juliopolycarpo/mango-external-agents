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

/// How the closed-link message of a turn on the `fake` profile begins, whichever way the prompt
/// task learned the link was gone.
const DEAD_PROMPT_PREFIX: &str = "fake: a transport that closed under session/prompt; ";

/// An agent whose stdout ends while a prompt is in flight has not been cancelled by anyone: the
/// turn fails as a closed link, with the agent's redacted stderr on the typed message, and the
/// session ends `Closed` with its child reaped.
///
/// The current-thread runtime this runs on always shows the prompt task the prompt's own failure
/// first. The multi-thread tests below repeat the death where the other order also happens.
#[tokio::test]
async fn an_agent_whose_output_ends_mid_prompt_fails_the_turn_as_a_closed_link() {
    let (session, launcher, mut turn) = turn_on_an_agent_that_just_died().await;
    let mut lifecycle = session.subscribe();

    let rest = drain(&mut turn).await;
    assert_events("the rest of the turn", &rest, &DEAD_AGENT_TURN);
    let Some(EventKind::Error { error }) = rest.last() else {
        unreachable!("the events above end in an error");
    };
    assert_closed_link_with_stderr(error, DEAD_PROMPT_PREFIX);

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

/// How many times a multi-thread test repeats a death.
///
/// A smoke test, not the gate. Before the fix, 11 of 50 runs of 200 rounds reported a
/// cancellation when run alone, and fewer under a loaded machine, so one run of this count
/// catches the race about one time in five. The gates that do not depend on scheduling are the
/// unit tests beside `prompt_ending`, which pin the decision for every order, and
/// `a_request_past_its_deadline_fails_the_turn_in_flight_as_a_closed_link` below, which reaches
/// the same decision on a current-thread runtime.
const DEATH_ROUNDS: usize = 200;

/// Asserts `events` end a turn as the closed link carrying the stderr tail, after `before`.
fn assert_turn_died_as_a_closed_link(what: &str, events: &[EventKind], before: &[&str]) {
    let expected: Vec<&str> = before.iter().chain(&DEAD_AGENT_TURN).copied().collect();
    assert_events(what, events, &expected);
    let Some(EventKind::Error { error }) = events.last() else {
        unreachable!("the events above end in an error");
    };
    assert_closed_link_with_stderr(error, DEAD_PROMPT_PREFIX);
}

/// The same death on a multi-thread runtime, repeated so every order the prompt task can observe
/// it in shows up in one run.
///
/// The prompt task waits on the prompt's own failure and on the end of the connection. The
/// lifecycle watcher can wind the loop down before that task is polled, so either may be seen
/// first. Nobody asked for a cancel, so both orders end the turn as the closed link, with the
/// agent's stderr: a host is never told it stopped a turn whose agent died.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_death_mid_prompt_is_never_reported_as_a_cancellation() {
    for round in 0..DEATH_ROUNDS {
        let (_session, _launcher, mut turn) = turn_on_an_agent_that_just_died().await;
        assert_turn_died_as_a_closed_link(
            &format!("the rest of the turn in round {round}"),
            &drain(&mut turn).await,
            &[],
        );
    }
}

/// An agent that dies the moment it reads the prompt, before it reports anything, ends the turn
/// the same way: the connection can be over before the prompt task first looks at it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_death_under_the_prompt_it_just_read_is_never_reported_as_a_cancellation() {
    for round in 0..DEATH_ROUNDS {
        let (session, _launcher) =
            open_under(dying_under("session/prompt"), Limits::default()).await;
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "start working"))
            .await
            .unwrap_or_else(|error| {
                panic!("expected a turn in round {round} | received: {error:?}")
            });
        assert_turn_died_as_a_closed_link(
            &format!("the turn in round {round}"),
            &drain(&mut turn).await,
            &["TurnStarted"],
        );
    }
}

/// A launcher whose child closes its stdout right behind the first line containing `after`:
/// that line is read whole, and the next read is the end of the stream.
///
/// Models an agent that writes its last answer and exits, with the answer still in the pipe.
#[derive(Clone)]
struct LeavingLauncher {
    inner: FakeLauncher,
    after: &'static str,
}

#[async_trait::async_trait]
impl ProcessLauncher for LeavingLauncher {
    async fn spawn(
        &self,
        spec: mango_external_agents::LaunchSpec,
    ) -> mango_external_agents::Result<ManagedProcess> {
        let mut process = self.inner.spawn(spec).await?;
        process.stdout = Box::new(LeavingStdout {
            inner: process.stdout,
            after: self.after,
            left: false,
        });
        Ok(process)
    }
}

struct LeavingStdout {
    inner: Box<dyn mango_external_agents::process::ByteSource>,
    after: &'static str,
    left: bool,
}

#[async_trait::async_trait]
impl mango_external_agents::process::ByteSource for LeavingStdout {
    async fn next_chunk(&mut self) -> mango_external_agents::Result<Option<Vec<u8>>> {
        if self.left {
            return Ok(None);
        }
        let chunk = self.inner.next_chunk().await?;
        self.left = chunk
            .as_deref()
            .is_some_and(|bytes| String::from_utf8_lossy(bytes).contains(self.after));
        Ok(chunk)
    }
}

/// An agent that answers its prompt and exits at once leaves the prompt task two things to see
/// at the same moment: the answer and the end of the connection. Whichever it looks at first,
/// the agent did answer, so the turn completes; it is not reported as a link that closed under
/// the prompt. Repeated on a multi-thread runtime, where both orders show up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_that_answers_and_exits_at_once_completes_the_turn() {
    for round in 0..DEATH_ROUNDS {
        let fake = FakeLauncher::new();
        fake.push(FakeAcpAgent::new().with_updates(Vec::new()).process());
        let launcher = LeavingLauncher {
            inner: fake,
            after: "stopReason",
        };
        let host = HostContext::builder()
            .launcher(Arc::new(launcher))
            .cwd(std::env::temp_dir())
            .client_info("mea-tests", "0.1.0")
            .build()
            .expect("expected a host");
        let session = AcpHarness::new(profile())
            .open_session(
                &host,
                OpenSession::new("chat-1").with_configuration(permissive()),
            )
            .await
            .unwrap_or_else(|error| panic!("expected a session | received: {error:?}"));
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "say nothing"))
            .await
            .unwrap_or_else(|error| {
                panic!("expected a turn in round {round} | received: {error:?}")
            });
        assert_events(
            &format!("the turn in round {round}"),
            &drain(&mut turn).await,
            &["TurnStarted", "Completed"],
        );
    }
}

/// A request that is not a prompt waits on its own reply alone, with nothing racing it, so an
/// agent that died under `session/list` fails the call as a closed link naming that request, with
/// the agent's stderr, and never as the agent's own refusal. The reply is failed at EOF before
/// anything can wind the connection down, so there is one order here and one round covers it.
#[tokio::test]
async fn an_agent_death_under_a_request_is_never_reported_as_the_agents_refusal() {
    let (session, _launcher) = open_under(
        dying_peer("session/list", serde_json::json!({ "list": {} })),
        Limits::default(),
    )
    .await;
    let error = refusal(session.list_sessions(Default::default()).await);
    let Error::Vendor(failure) = error.cause() else {
        panic!("expected Error::Vendor | received: {error:?}");
    };
    assert_closed_link_with_stderr(failure, "a transport that closed under session/list");
}

/// A request still waiting when the connection is wound down under it loses its reply with the
/// connection rather than at EOF. The agent refused nothing, so that is the closed link naming
/// the request too, and never `acp-request-failed`. Here the connection ends because the host
/// dropped another request it had sent.
#[tokio::test(start_paused = true)]
async fn a_request_whose_reply_is_dropped_with_the_connection_fails_as_a_closed_link() {
    let (session, _launcher) = open_under(
        FakeAcpAgent::new().holding_listing().process(),
        Limits::default(),
    )
    .await;
    let kept = session.list_sessions(Default::default());
    let dropped = session.list_sessions(Default::default());
    tokio::pin!(kept);
    let first = tokio::time::timeout(Duration::from_millis(1), async {
        tokio::select! {
            biased;
            kept = &mut kept => Some(kept),
            _ = dropped => None,
        }
    })
    .await;
    assert!(
        first.is_err(),
        "expected both requests still pending | received: {first:?}"
    );

    let error = refusal(kept.await);
    let Error::Vendor(failure) = error.cause() else {
        panic!("expected Error::Vendor | received: {error:?}");
    };
    let received = (failure.code.as_str(), failure.message.as_str());
    let expected = (
        "acp-link-closed",
        "a transport that closed under session/list",
    );
    assert_eq!(
        received, expected,
        "expected (code, message): {expected:?} | received: {received:?}"
    );
}

/// A host that asked to stop the turn before the agent died is told the turn was cancelled, with
/// its own reason. The request is recorded when `cancel` is called, so it is in place whichever
/// way the prompt task then learns the agent is gone.
#[tokio::test]
async fn a_cancel_requested_before_the_agent_died_ends_the_turn_as_cancelled() {
    let (session, launcher) = open_under(dying_under("session/cancel"), Limits::default()).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "start working"))
        .await
        .expect("expected a turn");

    session
        .cancel(CancelReason::Requested)
        .await
        .expect("expected the cancel to be queued");

    assert_events(
        "the turn",
        &drain(&mut turn).await,
        &["TurnStarted", "Cancelled(Requested)", "Completed"],
    );
    assert_wire_after(&launcher, "session/prompt", &["session/cancel"]);
}

/// A peer that answers the handshake up to `dies_under` and closes its stdout when it reads
/// that request, leaving [`STDERR`] behind.
fn dying_under(dies_under: &'static str) -> FakeProcess {
    dying_peer(dies_under, serde_json::json!({}))
}

/// [`dying_under`], for a peer that advertises these `sessionCapabilities` first.
fn dying_peer(dies_under: &'static str, capabilities: serde_json::Value) -> FakeProcess {
    let gone = CancelToken::new();
    let dying = gone.clone();
    let peer = ScriptedPeer::new(|_| Vec::new()).with_session_capabilities(capabilities);
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

/// An agent that answers `session/prompt` with an error of its own has refused the prompt. The
/// turn fails as that refusal: `acp-request-failed`, naming the request, with the agent's code
/// and message. It is not reported as a link that closed, which is what a host is told when
/// the agent went away.
#[tokio::test]
async fn an_agents_own_error_on_the_prompt_fails_the_turn_as_its_refusal() {
    let refusing = ScriptedPeer::new(|frame| match frame["method"].as_str() {
        Some("session/prompt") => vec![
            serde_json::json!({
                "jsonrpc": "2.0", "id": frame["id"],
                "error": { "code": -31_000, "message": "model busy" }
            })
            .to_string(),
        ],
        _ => Vec::new(),
    });
    let (session, _launcher) = open_under(refusing.process(), Limits::default()).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "try"))
        .await
        .expect("expected a turn");

    let events = drain(&mut turn).await;
    assert_events(
        "the refused turn",
        &events,
        &["TurnStarted", "Error(acp-request-failed)"],
    );
    let Some(EventKind::Error { error }) = events.last() else {
        unreachable!("the events above end in an error");
    };
    let received = (
        error.request_id.as_deref(),
        error.vendor_code.as_deref(),
        error.message.as_str(),
    );
    let expected = (Some("session/prompt"), Some("-31000"), "model busy");
    assert_eq!(
        received, expected,
        "expected (request, the agent's code, message): {expected:?} | received: {received:?}"
    );
}

/// The JSON-RPC error an agent answers a prompt with.
fn prompt_refusal(id: &serde_json::Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0", "id": id, "error": { "code": -31_000, "message": "model busy" }
    })
    .to_string()
}

/// A `session/update` line for the fake session carrying `update`.
fn update_line(update: serde_json::Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0", "method": "session/update",
        "params": { "sessionId": "sess_fake", "update": update }
    })
    .to_string()
}

/// A `session/request_permission` line under `id` with one way to allow and one to refuse.
fn permission_ask(id: u32) -> String {
    serde_json::json!({
        "jsonrpc": "2.0", "id": id, "method": "session/request_permission",
        "params": {
            "sessionId": "sess_fake",
            "toolCall": { "toolCallId": "call_1", "kind": "execute", "title": "Run tests" },
            "options": [
                { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
                { "optionId": "reject", "name": "Reject", "kind": "reject_once" },
            ],
        }
    })
    .to_string()
}

/// An agent that refuses a prompt with an error of its own has answered: the connection is as
/// healthy as after any answer, so the session stays usable, as it does after a `refusal` or a
/// `max_tokens` stop. The failed turn leaves nothing behind for the next one: the question it
/// left parked is withdrawn, the call it left running is closed with the failed turn, and the
/// second turn on the same session starts clean and completes.
#[tokio::test]
async fn a_session_takes_another_turn_after_an_agents_error_on_the_prompt() {
    let prompts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = Arc::clone(&prompts);
    let peer = ScriptedPeer::new(move |frame| match frame["method"].as_str() {
        Some("session/prompt") => {
            match seen.fetch_add(1, std::sync::atomic::Ordering::AcqRel) {
                // The first prompt: a call left running, a question left open, then the error.
                0 => vec![
                    update_line(serde_json::json!({
                        "sessionUpdate": "tool_call", "toolCallId": "call_1",
                        "title": "Run tests", "kind": "execute", "status": "in_progress",
                    })),
                    permission_ask(9_001),
                    prompt_refusal(&frame["id"]),
                ],
                _ => vec![
                    update_line(serde_json::json!({
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": "second" }
                    })),
                    reply(
                        &frame["id"],
                        serde_json::json!({ "stopReason": "end_turn" }),
                    ),
                ],
            }
        }
        _ => Vec::new(),
    });
    let launcher = FakeLauncher::new();
    launcher.push(peer.process());
    let session = AcpHarness::new(profile())
        .open_session(
            &host(&launcher),
            OpenSession::new("chat-1").with_configuration(permissive()),
        )
        .await
        .expect("expected a session");

    let mut first = session
        .start_turn(TurnRequest::new("turn-1", "try"))
        .await
        .expect("expected the first turn");
    let events = drain(&mut first).await;
    let closed: Vec<String> = events
        .iter()
        .filter_map(|kind| match kind {
            EventKind::ActivityCompleted { call_id, result } => {
                Some(format!("{call_id} {:?}", result.status))
            }
            _ => None,
        })
        .collect();
    let labels = event_labels(&events);
    let received = (closed, labels.last().map(String::as_str));
    let expected = (
        vec![String::from("call_1 Failed")],
        Some("Error(acp-request-failed)"),
    );
    assert_eq!(
        received, expected,
        "expected (calls closed, terminal) of the refused turn: {expected:?} | received: {received:?} in {labels:?}"
    );
    eventually("the parked question withdrawn", &launcher, || {
        wire_labels(&launcher)
            .iter()
            .any(|label| label == "outcome cancelled for 9001")
            .then_some(())
    })
    .await;
    assert_status(
        "after the agent's error",
        session.snapshot().status,
        SessionStatus::Ready,
    );
    assert_eq!(
        launcher.live_children(),
        1,
        "expected the agent still running after its own error | received: {} children",
        launcher.live_children()
    );

    let mut second = start_when_free(session.as_ref(), "turn-2", "again").await;
    assert_events(
        "the second turn",
        &drain(&mut second).await,
        &["TurnStarted", "TextDelta(second)", "Completed"],
    );
    assert_status(
        "after the second turn",
        session.snapshot().status,
        SessionStatus::Ready,
    );
}

/// An agent that refuses a prompt and then floods the connection has still refused the prompt.
/// The turn reports the refusal it was given, with the agent's code, and not the budget the
/// agent went on to pass: that ends the connection, and the session with it, on its own.
#[tokio::test]
async fn an_agents_error_followed_by_a_flood_is_still_reported_as_the_error() {
    let peer = ScriptedPeer::new(|frame| match frame["method"].as_str() {
        Some("session/prompt") => {
            let heartbeat = serde_json::json!({ "jsonrpc": "2.0", "method": "$/vendor/heartbeat" });
            vec![
                prompt_refusal(&frame["id"]),
                serde_json::Value::Array(vec![heartbeat; 1_100]).to_string(),
            ]
        }
        _ => Vec::new(),
    });
    let (session, launcher) = open_under(peer.process(), Limits::default()).await;
    let mut lifecycle = session.subscribe();
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "try"))
        .await
        .expect("expected a turn");

    assert_events(
        "the refused turn",
        &drain(&mut turn).await,
        &["TurnStarted", "Error(acp-request-failed)"],
    );
    assert_status(
        "after the flood",
        status_once_settled(&mut lifecycle).await,
        SessionStatus::Closed,
    );
    assert_no_live_children("after the flood", &launcher);
}

/// A cancel that races the agent's own error is still the host's cancel: the turn ends as
/// `Cancelled` with the reason the host gave, the agent having answered, and the session takes
/// the next turn.
#[tokio::test]
async fn a_cancel_answered_with_the_agents_error_ends_as_cancelled_and_keeps_the_session() {
    let prompt = Arc::new(Mutex::new(None::<serde_json::Value>));
    let held = Arc::clone(&prompt);
    let peer = ScriptedPeer::new(move |frame| {
        let mut held = held.lock().expect("expected the prompt slot");
        match frame["method"].as_str() {
            Some("session/prompt")
                if held.is_none() && frame["params"]["prompt"][0]["text"] == "wait" =>
            {
                *held = frame.get("id").cloned();
                Vec::new()
            }
            Some("session/prompt") => {
                vec![reply(
                    &frame["id"],
                    serde_json::json!({ "stopReason": "end_turn" }),
                )]
            }
            // The agent answers the cancel by failing the prompt it was holding.
            Some("session/cancel") => held.take().iter().map(prompt_refusal).collect(),
            _ => Vec::new(),
        }
    });
    let (session, _launcher) = open_under(peer.process(), Limits::default()).await;

    let mut first = session
        .start_turn(TurnRequest::new("turn-1", "wait"))
        .await
        .expect("expected the first turn");
    session
        .cancel(CancelReason::Requested)
        .await
        .expect("expected the cancel to be queued");
    assert_events(
        "the cancelled turn",
        &drain(&mut first).await,
        &["TurnStarted", "Cancelled(Requested)", "Completed"],
    );
    assert_status(
        "after the cancel",
        session.snapshot().status,
        SessionStatus::Ready,
    );

    let mut second = start_when_free(session.as_ref(), "turn-2", "again").await;
    assert_events(
        "the turn after the cancel",
        &drain(&mut second).await,
        &["TurnStarted", "Completed"],
    );
}

/// A result this client cannot read is not the agent's refusal: the agent believes it answered
/// and nothing says what it thinks the session's state is. The turn fails and the session ends,
/// as it does when the link closes.
#[tokio::test]
async fn a_prompt_result_that_cannot_be_read_still_ends_the_session() {
    let peer = ScriptedPeer::new(|frame| match frame["method"].as_str() {
        Some("session/prompt") => vec![reply(&frame["id"], serde_json::json!({ "stopReason": 7 }))],
        _ => Vec::new(),
    });
    let (session, launcher) = open_under(peer.process(), Limits::default()).await;
    let mut lifecycle = session.subscribe();
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "try"))
        .await
        .expect("expected a turn");

    let events = drain(&mut turn).await;
    assert_events(
        "the unreadable turn",
        &events,
        &["TurnStarted", "Error(acp-request-failed)"],
    );
    let Some(EventKind::Error { error }) = events.last() else {
        unreachable!("the events above end in an error");
    };
    let received = (error.request_id.as_deref(), error.vendor_code.as_deref());
    let expected = (Some("session/prompt"), Some("-32603"));
    assert_eq!(
        received, expected,
        "expected (request, code) of the unreadable result: {expected:?} | received: {received:?}"
    );
    assert_status(
        "after an unreadable result",
        status_once_settled(&mut lifecycle).await,
        SessionStatus::Closed,
    );
    assert_no_live_children("after an unreadable result", &launcher);
    let next = refusal(
        session
            .start_turn(TurnRequest::new("turn-2", "again"))
            .await,
    );
    assert!(
        matches!(next.cause(), Error::Closed { subject: "session" }),
        "expected the next turn: Closed {{ subject: \"session\" }} | received: {next:?}"
    );
}

/// A request that passed its deadline ends the connection under a turn that was still running.
/// Nobody asked to stop that turn, so it ends as the closed link and not as a cancellation the
/// host never requested. The message says which request's deadline closed the link, so the
/// turn does not fail for no visible reason.
#[tokio::test(start_paused = true)]
async fn a_request_past_its_deadline_fails_the_turn_in_flight_as_a_closed_link() {
    let deadline = Duration::from_secs(5);
    let (session, launcher) = open_under(
        FakeAcpAgent::new()
            .holding_listing()
            .with_updates(Vec::new())
            .staying_silent()
            .process(),
        Limits {
            request_timeout: deadline,
            ..Limits::default()
        },
    )
    .await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "think for a while"))
        .await
        .expect("expected a turn");

    let error = refusal(session.list_sessions(Default::default()).await);
    assert!(
        matches!(error.cause(), Error::Timeout { .. }),
        "expected the listing: Error::Timeout | received: {error:?}"
    );

    let events = drain_within(&mut turn, 10 * deadline).await;
    assert_events(
        "the turn in flight",
        &events,
        &["TurnStarted", "Error(acp-link-closed)"],
    );
    let Some(EventKind::Error { error }) = events.last() else {
        unreachable!("the events above end in an error");
    };
    let expected = "fake: a transport that closed under session/prompt after session/list got no answer within 5s";
    assert_eq!(
        error.message, expected,
        "expected the failure message: {expected:?} | received: {:?}",
        error.message
    );
    assert_no_live_children("after a timed-out request", &launcher);
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
