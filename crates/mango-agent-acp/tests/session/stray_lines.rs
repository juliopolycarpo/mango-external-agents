//! Lines an agent puts on stdout mid-turn that are not part of the turn it is streaming.

use super::*;

fn text_chunk(text: &str) -> serde_json::Value {
    serde_json::json!({
        "sessionUpdate": "agent_message_chunk",
        "content": { "type": "text", "text": text }
    })
}

/// Each event as a short label, with a `TextDelta` showing its text.
fn labels(events: &[EventKind]) -> Vec<String> {
    events
        .iter()
        .map(|kind| match kind {
            EventKind::TextDelta { text } => format!("TextDelta({text})"),
            other => format!("{other:?}").chars().take(80).collect(),
        })
        .collect()
}

/// Every line the client wrote to the agent that is JSON and carries exactly this `id`.
fn written_with_id(launcher: &FakeLauncher, id: &serde_json::Value) -> Vec<serde_json::Value> {
    launcher
        .written()
        .iter()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|frame| frame.get("id") == Some(id))
        .collect()
}

/// Waits until the client has written `count` frames carrying `id`, or fails naming what it wrote.
///
/// A bounded poll rather than one read: the SDK's transport actor flushes a reply after the handler
/// that produced it returned, so the line is not there yet when the turn ends.
async fn replies_reaching_the_agent(
    launcher: &FakeLauncher,
    id: serde_json::Value,
    count: usize,
) -> Vec<serde_json::Value> {
    let waited = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let replies = written_with_id(launcher, &id);
            if replies.len() >= count {
                return replies;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    waited.unwrap_or_else(|_| {
        panic!(
            "expected {count} frame(s) carrying id {id} to reach the agent | received: {:?}",
            launcher.written()
        )
    })
}

/// Runs one turn to its terminal and asserts it streamed `before`, `after` and completed.
async fn assert_turn_streams_around_the_stray_line(session: &dyn Session, turn_id: &str) {
    let mut turn = session
        .start_turn(TurnRequest::new(turn_id, "say two things"))
        .await
        .unwrap_or_else(|error| panic!("expected {turn_id} to be admitted | received: {error:?}"));
    let received = labels(&drain(&mut turn).await);
    assert_eq!(
        received,
        [
            "TurnStarted",
            "TextDelta(before)",
            "TextDelta(after)",
            "Completed"
        ],
        "expected {turn_id}: [TurnStarted, TextDelta(before), TextDelta(after), Completed] | received: {received:?}"
    );
}

/// What happens today when an agent prints something that is not JSON between two updates, as a
/// CLI that logs a warning to stdout does: the line costs the turn nothing. The update after it
/// still arrives, the turn completes, the session stays `Ready` and takes another turn, and the
/// SDK answers each such line with a JSON-RPC parse error (`-32700`, `id: null`).
///
/// A pin rather than a requirement: it records the behaviour so a change in the SDK's handling of
/// malformed input shows up here instead of as a session that dies on a stray log line.
#[tokio::test]
async fn a_non_json_line_mid_turn_is_answered_with_a_parse_error_and_costs_the_turn_nothing() {
    let (session, launcher) = open(
        FakeAcpAgent::new()
            .with_updates(vec![text_chunk("before"), text_chunk("after")])
            .writing_mid_turn(1, "warning: this line is not JSON-RPC"),
        permissive(),
    )
    .await;

    assert_turn_streams_around_the_stray_line(session.as_ref(), "turn-1").await;
    let status = session.snapshot().status;
    assert_eq!(
        status,
        SessionStatus::Ready,
        "expected session status after a non-JSON line: Ready | received: {status:?}"
    );
    // The fake writes the line on every turn, so a second turn proves the session survived the
    // first and that the answer is per line rather than once per connection.
    assert_turn_streams_around_the_stray_line(session.as_ref(), "turn-2").await;

    let replies = replies_reaching_the_agent(&launcher, serde_json::Value::Null, 2).await;
    let codes: Vec<&serde_json::Value> = replies
        .iter()
        .map(|reply| &reply["error"]["code"])
        .collect();
    assert!(
        replies.len() == 2 && codes.iter().all(|code| **code == serde_json::json!(-32700)),
        "expected two id-null replies with error code -32700, one per stray line | received: {replies:?}"
    );

    session
        .close(CloseReason::Requested)
        .await
        .expect("expected a clean close");
    assert_eq!(
        launcher.live_children(),
        0,
        "expected live children after close: 0 | received: {}",
        launcher.live_children()
    );
}

/// The client advertises neither file access nor terminals (`fs.readTextFile: false`,
/// `terminal: false`), so an agent that asks anyway is refused with JSON-RPC `-32601` on the id it
/// used, and the turn it asked during carries on to its end.
///
/// These are methods the SDK's schema knows, unlike the invented method the connection's own unit
/// test sends, so this is the case where a typed default handler could answer in the library's
/// place.
#[tokio::test]
async fn a_file_or_terminal_request_from_the_agent_is_refused_as_method_not_found() {
    let asks = [
        (
            9_101,
            "fs/read_text_file",
            serde_json::json!({ "sessionId": "sess_fake", "path": "/etc/hostname" }),
        ),
        (
            9_102,
            "terminal/create",
            serde_json::json!({ "sessionId": "sess_fake", "command": "true" }),
        ),
    ];
    let agent = asks.iter().fold(
        FakeAcpAgent::new().with_updates(vec![text_chunk("before"), text_chunk("after")]),
        |agent, (id, method, params)| {
            agent.writing_mid_turn(
                1,
                serde_json::json!({
                    "jsonrpc": "2.0", "id": id, "method": method, "params": params
                })
                .to_string(),
            )
        },
    );
    let (session, launcher) = open(agent, permissive()).await;

    assert_turn_streams_around_the_stray_line(session.as_ref(), "turn-1").await;

    for (id, method, _) in &asks {
        let replies = replies_reaching_the_agent(&launcher, serde_json::json!(id), 1).await;
        let code = &replies[0]["error"]["code"];
        assert!(
            replies.len() == 1
                && *code == serde_json::json!(-32601)
                && replies[0].get("result").is_none(),
            "expected one reply to {method} (id {id}) with error code -32601 and no result | received: {replies:?}"
        );
    }
    let status = session.snapshot().status;
    assert_eq!(
        status,
        SessionStatus::Ready,
        "expected session status after refusing the agent's requests: Ready | received: {status:?}"
    );
}
