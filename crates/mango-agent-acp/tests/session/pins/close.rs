//! What `close` writes to the agent, and how long it may take.

use super::*;

/// Asserts the session is `Closed` and its child reaped.
#[track_caller]
fn assert_closed_and_reaped(session: &dyn Session, launcher: &FakeLauncher) {
    assert_status(
        "after close",
        session.snapshot().status,
        SessionStatus::Closed,
    );
    assert_no_live_children("after close", launcher);
}

/// An agent that did not advertise `session/close` is told nothing: closing an idle session
/// writes no frame at all, and the agent learns of it from its stdin ending.
#[tokio::test]
async fn closing_an_idle_session_writes_nothing_when_the_agent_has_no_close_method() {
    let (session, launcher) = open(FakeAcpAgent::new(), permissive()).await;
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected a clean close");
    assert_wire_after(&launcher, "session/new", &[]);
    assert_closed_and_reaped(session.as_ref(), &launcher);
}

/// An agent that advertised `session/close` is sent exactly that request, naming the session,
/// before its stdin ends.
#[tokio::test]
async fn closing_writes_session_close_when_the_agent_advertised_it() {
    let (session, launcher) = open(FakeAcpAgent::new().closing_sessions(), permissive()).await;
    session
        .close(CloseReason::Requested)
        .await
        .expect("expected a clean close");
    assert_wire_after(&launcher, "session/new", &["session/close"]);
    let received: Vec<serde_json::Value> = wire(&launcher)
        .iter()
        .filter(|frame| frame["method"] == "session/close")
        .map(|frame| frame["params"]["sessionId"].clone())
        .collect();
    let expected = [serde_json::json!("sess_fake")];
    assert_eq!(
        received, expected,
        "expected the session each session/close names: {expected:?} | received: {received:?}"
    );
    assert_closed_and_reaped(session.as_ref(), &launcher);
}

/// Closing during a turn sends the agent no `session/cancel`: the turn ends cancelled with the
/// close's own reason, and the agent is stopped by its stdin ending and then its process.
#[tokio::test]
async fn closing_during_a_turn_writes_no_cancel_and_ends_the_turn_with_the_close_reason() {
    let (session, launcher) = open(
        FakeAcpAgent::new()
            .with_updates(Vec::new())
            .staying_silent(),
        permissive(),
    )
    .await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "wait"))
        .await
        .expect("expected a turn");
    eventually("the prompt on the wire", &launcher, || {
        wire_labels(&launcher)
            .contains(&String::from("session/prompt"))
            .then_some(())
    })
    .await;

    session
        .close(CloseReason::ConsentRevoked)
        .await
        .expect("expected a clean close");
    assert_events(
        "events",
        &drain(&mut turn).await,
        &["TurnStarted", "Cancelled(ConsentRevoked)", "Completed"],
    );
    assert_wire_after(&launcher, "session/prompt", &[]);
    assert_closed_and_reaped(session.as_ref(), &launcher);
}

/// An agent that advertised `session/close` and never answers it holds the close for
/// `shutdown_timeout` and no longer: the handshake is abandoned at that bound, the child is
/// reaped and the close still succeeds.
#[tokio::test(start_paused = true)]
async fn an_agent_that_ignores_session_close_holds_the_close_for_the_shutdown_timeout() {
    let shutdown_timeout = Duration::from_secs(3);
    let peer = ScriptedPeer::new(|_| Vec::new())
        .with_session_capabilities(serde_json::json!({ "close": {} }));
    let (session, launcher) = open_under(
        peer.process(),
        Limits {
            shutdown_timeout,
            ..Limits::default()
        },
    )
    .await;

    let begun = tokio::time::Instant::now();
    let closed = session.close(CloseReason::Requested).await;
    let received = (closed.is_ok(), begun.elapsed());
    let expected = (true, shutdown_timeout);
    assert_eq!(
        received, expected,
        "expected (close succeeded, time taken): {expected:?} | received: {received:?} from {closed:?}"
    );
    assert_first_frame_after(&launcher, "session/new", "session/close");
    assert_closed_and_reaped(session.as_ref(), &launcher);
}
