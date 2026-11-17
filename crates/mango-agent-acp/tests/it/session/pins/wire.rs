//! What reaches the agent's stdin, and in which order, around a cancel, an answer and traffic the
//! client has no handler for.

use super::*;

/// An agent that holds its prompt open and says nothing, so only a cancel ends the turn.
fn silent_agent() -> FakeAcpAgent {
    FakeAcpAgent::new()
        .with_updates(Vec::new())
        .staying_silent()
}

/// What a cancelled turn that streamed nothing shows a host.
const CANCELLED_TURN: [&str; 3] = ["TurnStarted", "Cancelled(Requested)", "Completed"];

/// What a turn that streamed `before` and `after` shows a host.
const TWO_CHUNK_TURN: [&str; 4] = [
    "TurnStarted",
    "TextDelta(before)",
    "TextDelta(after)",
    "Completed",
];

/// ACP requires a cancelling client to answer every pending `session/request_permission` with the
/// `cancelled` outcome. The order matters to an agent that reads its stdin in sequence: the
/// withdrawal is written first, so the tool call waiting on it has returned by the time the agent
/// reads `session/cancel`.
#[tokio::test]
async fn a_cancel_with_a_question_parked_writes_the_withdrawal_and_then_session_cancel() {
    let (session, launcher) = open(
        FakeAcpAgent::new().asking_for_approval(Approval::Once),
        permissive(),
    )
    .await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "delete the build"))
        .await
        .expect("expected a turn");
    read_until(&mut turn, "the approval request", |kind| {
        matches!(kind, EventKind::ApprovalRequested { .. })
    })
    .await;

    session
        .cancel(CancelReason::Requested)
        .await
        .expect("expected the cancel to be queued");

    let expected = ["outcome cancelled for 9001", "session/cancel"];
    eventually("both cancel frames on the wire", &launcher, || {
        (wire_after(&launcher, "session/prompt").len() >= expected.len()).then_some(())
    })
    .await;
    assert_wire_after(&launcher, "session/prompt", &expected);
    let events = drain(&mut turn).await;
    let closing = &events[events.len().saturating_sub(2)..];
    assert_events(
        "the turn's last two events",
        closing,
        &["Cancelled(Requested)", "Completed"],
    );
}

/// A cancel requested as soon as `start_turn` returns still follows the prompt it stops: the
/// agent reads `session/prompt` and then exactly one `session/cancel`.
#[tokio::test]
async fn a_cancel_right_after_start_turn_is_written_after_the_prompt() {
    let (session, launcher) = open(silent_agent(), permissive()).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "wait"))
        .await
        .expect("expected a turn");
    session
        .cancel(CancelReason::Requested)
        .await
        .expect("expected the cancel to be queued");

    assert_events("events", &drain(&mut turn).await, &CANCELLED_TURN);
    assert_wire_after(
        &launcher,
        "session/new",
        &["session/prompt", "session/cancel"],
    );
}

/// `TurnStarted` is published before the prompt is written, so a host can cancel a turn whose
/// prompt is not on the wire yet. That early `session/cancel` names no prompt the agent knows, so
/// a second one is written behind the prompt, and it is the one that ends the turn: the agent
/// here ends a turn only for a cancel it reads after the prompt.
///
/// The whole sequence is pinned as it is written today, the early frame included. What a host
/// depends on is the last frame; a client that stops writing the first one changes this
/// expectation knowingly.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_cancel_that_beats_the_prompt_to_the_wire_is_repeated_after_it() {
    let launcher = FakeLauncher::new();
    launcher.push(silent_agent().process());
    let clock = Arc::new(TurnStartedClock::default());
    let host = host_with_clock(&launcher, Arc::clone(&clock) as Arc<dyn Clock>);
    let session: Arc<dyn Session> = Arc::from(
        AcpHarness::new(profile())
            .open_session(&host, OpenSession::new("chat-1"))
            .await
            .expect("expected a session"),
    );

    // Armed after the open, so the blocked read is the stamp on `TurnStarted`.
    clock.arm();
    let starting = {
        let session = Arc::clone(&session);
        tokio::spawn(async move { session.start_turn(TurnRequest::new("turn-1", "wait")).await })
    };
    clock.wait_until_blocked();
    session
        .cancel(CancelReason::Requested)
        .await
        .expect("expected the early cancel to be queued");
    clock.release();

    let mut turn = starting
        .await
        .expect("expected the start task to join")
        .unwrap_or_else(|error| panic!("expected an accepted turn | received: {error:?}"));
    assert_events("events", &drain(&mut turn).await, &CANCELLED_TURN);
    assert_wire_after(
        &launcher,
        "session/new",
        &["session/cancel", "session/prompt", "session/cancel"],
    );
}

/// Two of a host's own tasks can answer the same question. The second answer is accepted, not
/// failed, and never reaches the agent: the decision the agent acted on is the one already sent.
#[tokio::test]
async fn a_second_answer_to_a_settled_question_is_accepted_and_writes_nothing() {
    let (session, launcher) = open(
        FakeAcpAgent::new().asking_for_approval(Approval::Once),
        permissive(),
    )
    .await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "delete the build"))
        .await
        .expect("expected a turn");
    let seen = read_until(&mut turn, "the approval request", |kind| {
        matches!(kind, EventKind::ApprovalRequested { .. })
    })
    .await;
    let Some(EventKind::ApprovalRequested { request }) = seen.last() else {
        unreachable!("read_until returned at the approval request");
    };
    let refusal = request.deny().expect("expected a refusing option");

    session
        .respond(refusal.clone())
        .await
        .expect("expected the first answer to be accepted");
    let events = drain(&mut turn).await;
    assert_events(
        "the answered turn's last event",
        &events[events.len().saturating_sub(1)..],
        &["Completed"],
    );
    let late = session.respond(refusal).await;
    assert!(
        late.is_ok(),
        "expected the second answer: Ok(()) | received: {late:?}"
    );

    // Closing ends the writer, so what is on the wire now is all there will ever be.
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected a clean close");
    assert_wire_after(&launcher, "session/prompt", &["outcome selected for 9001"]);
}

/// A JSON-RPC batch is processed member by member, in order: updates ahead of the prompt's own
/// response in the same array all reach the host before the turn completes.
#[tokio::test]
async fn a_batch_carrying_updates_and_the_prompt_response_is_processed_in_order() {
    let texts: Vec<String> = (0..24).map(|index| format!("chunk {index}")).collect();
    let (session, _launcher) = open(
        FakeAcpAgent::new()
            .with_updates(texts.iter().map(|text| text_chunk(text)).collect())
            .batching_the_response_with_updates(),
        permissive(),
    )
    .await;
    let mut expected = vec![String::from("TurnStarted")];
    expected.extend(texts.iter().map(|text| format!("TextDelta({text})")));
    expected.push(String::from("Completed"));
    let expected: Vec<&str> = expected.iter().map(String::as_str).collect();

    for turn_id in ["turn-1", "turn-2"] {
        let mut turn = start_when_free(session.as_ref(), turn_id, "stream then finish").await;
        assert_events(turn_id, &drain(&mut turn).await, &expected);
    }
    assert_status(
        "after two batched turns",
        session.snapshot().status,
        SessionStatus::Ready,
    );
}

/// Traffic the client has no handler for costs the turn nothing: a notification with a method
/// nobody knows is dropped without an answer, and a response to an id the client never used is
/// dropped too. The updates around them arrive, the turn completes, and nothing is written back.
#[tokio::test]
async fn an_unknown_notification_and_a_response_to_an_unknown_id_are_ignored() {
    let strays = [
        serde_json::json!({
            "jsonrpc": "2.0", "method": "session/unsupported",
            "params": { "sessionId": "sess_fake" }
        }),
        serde_json::json!({ "jsonrpc": "2.0", "method": "$/vendor/heartbeat" }),
        serde_json::json!({ "jsonrpc": "2.0", "id": "never-sent", "result": { "ok": true } }),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 4_242,
            "error": { "code": -32603, "message": "late failure" }
        }),
    ];
    let agent = strays.iter().fold(
        FakeAcpAgent::new().with_updates(vec![text_chunk("before"), text_chunk("after")]),
        |agent, stray| agent.writing_mid_turn(1, stray.to_string()),
    );
    let (session, launcher) = open(agent, permissive()).await;

    for turn_id in ["turn-1", "turn-2"] {
        let mut turn = start_when_free(session.as_ref(), turn_id, "say two things").await;
        assert_events(turn_id, &drain(&mut turn).await, &TWO_CHUNK_TURN);
    }
    assert_status(
        "after stray frames",
        session.snapshot().status,
        SessionStatus::Ready,
    );

    // Closing ends the writer, so a reply to a stray frame would be on the wire by now: the two
    // prompts are all the client wrote.
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected a clean close");
    assert_wire_after(
        &launcher,
        "session/new",
        &["session/prompt", "session/prompt"],
    );
}

/// A request whose method the client has never heard of is answered with JSON-RPC `-32601` on
/// the id the agent used, and the turn it arrived in completes.
#[tokio::test]
async fn a_request_with_an_unknown_method_is_refused_as_method_not_found() {
    let ask = serde_json::json!({
        "jsonrpc": "2.0", "id": 999, "method": "session/unsupported",
        "params": { "sessionId": "sess_fake" }
    });
    let (session, launcher) = open(
        FakeAcpAgent::new()
            .with_updates(vec![text_chunk("before"), text_chunk("after")])
            .writing_mid_turn(1, ask.to_string()),
        permissive(),
    )
    .await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "say two things"))
        .await
        .expect("expected a turn");
    assert_events("events", &drain(&mut turn).await, &TWO_CHUNK_TURN);
    eventually("the refusal on the wire", &launcher, || {
        (!wire_after(&launcher, "session/prompt").is_empty()).then_some(())
    })
    .await;
    assert_wire_after(&launcher, "session/prompt", &["error -32601 for 999"]);
}

/// A `config_option_update` that arrives ahead of the response to the `session/set_config_option`
/// it overtakes is the newer fact, and it is applied once: the call returns it, the published
/// snapshot agrees, and the stale response neither replaces it nor makes the client ask again.
#[tokio::test]
async fn a_catalog_update_ahead_of_its_set_option_response_is_what_the_call_returns() {
    let launcher = FakeLauncher::new();
    launcher.push(InterleavingConfigAgent::process());
    let session = AcpHarness::new(profile())
        .open_session(&host(&launcher), OpenSession::new("chat-1"))
        .await
        .expect("expected a session");

    let outcome = session
        .configure(ConfigurationPatch::new().model(ConfigurationChange::Set(String::from("large"))))
        .await
        .unwrap_or_else(|error| panic!("expected the option accepted | received: {error:?}"));

    let snapshot = session.snapshot();
    let received = (
        outcome.state.observed.model.as_deref(),
        snapshot.configuration.observed.model.as_deref(),
        snapshot.catalog.options().len(),
    );
    let expected = (Some("newer"), Some("newer"), 1);
    assert_eq!(
        received, expected,
        "expected (returned model, published model, catalog rows): {expected:?} | received: {received:?}"
    );
    assert_wire_after(&launcher, "session/new", &["session/set_config_option"]);
}

/// A request abandoned at its deadline leaves one more frame behind it today: `$/cancel_request`
/// naming the id of the request nobody is waiting on, written before the connection ends.
///
/// Kept apart from the timeout's own pin so that a client which stops sending the notification
/// changes this test and no other.
#[tokio::test(start_paused = true)]
async fn a_timed_out_request_is_followed_by_cancel_request_naming_its_id() {
    let (session, launcher) = open_under(
        FakeAcpAgent::new().holding_listing().process(),
        Limits {
            request_timeout: Duration::from_secs(5),
            ..Limits::default()
        },
    )
    .await;
    let mut lifecycle = session.subscribe();
    let error = refusal(session.list_sessions(Default::default()).await);
    assert!(
        matches!(error.cause(), Error::Timeout { .. }),
        "expected Error::Timeout | received: {error:?}"
    );
    assert_status(
        "after the timeout",
        status_once_settled(&mut lifecycle).await,
        SessionStatus::Closed,
    );
    assert_abandoned_request_was_cancelled_on_the_wire(&launcher, "session/new", "session/list");
}

/// The same frame follows a `session/close` the agent never answered, once `close` gives up on it.
#[tokio::test(start_paused = true)]
async fn an_unanswered_session_close_is_followed_by_cancel_request_naming_its_id() {
    let peer = ScriptedPeer::new(|_| Vec::new())
        .with_session_capabilities(serde_json::json!({ "close": {} }));
    let (session, launcher) = open_under(peer.process(), Limits::default()).await;
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected the close to succeed");
    assert_abandoned_request_was_cancelled_on_the_wire(&launcher, "session/new", "session/close");
}
