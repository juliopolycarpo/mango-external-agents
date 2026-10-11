//! JSON that is not a request, answered with `-32600` only by a client that was asked to.

use super::outbox_tests::{gated, limits, link_held, options, outbox_holds, sent, until, within};
use super::*;
use crate::jsonrpc::WireOptions;

/// A client on a link that takes every write, with a handler that records and answers `yes`.
fn connect(wire: WireOptions) -> (ScriptedLink, Arc<RecordingHandler>, Arc<Client>) {
    let link = ScriptedLink::new();
    let handler = RecordingHandler::arc(Some(ServerRequestOutcome::Answer(json!("yes"))));
    let client = Arc::new(Client::connect_with(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options(),
        wire,
    ));
    (link, handler, client)
}

fn notification(method: &str) -> String {
    json!({ "jsonrpc": "2.0", "method": method, "params": {} }).to_string()
}

fn invalid_request() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": { "code": -32600, "message": "Invalid Request" },
    })
}

fn frames(link: &ScriptedLink) -> Vec<Value> {
    link.sent()
        .iter()
        .map(|frame| serde_json::from_str(frame).expect("expected a JSON frame"))
        .collect()
}

fn methods_seen(handled: &[(String, Value)]) -> Vec<&str> {
    handled.iter().map(|(method, _)| method.as_str()).collect()
}

/// The lines a client that answers invalid requests owes an answer: JSON that is neither an
/// object nor an array.
const NOT_REQUESTS: [&str; 5] = ["5", "\"text\"", "true", "false", "null"];

/// A client that was not asked answers none of it: JSON that is not a request is dropped, as
/// it always was, in a batch or on a line of its own, and the connection goes on.
#[tokio::test]
async fn json_that_is_not_a_request_is_dropped_by_a_client_that_was_not_asked_to_answer() {
    let (link, handler, client) = connect(WireOptions::new().with_batch_decoding());
    for line in NOT_REQUESTS {
        link.push_line(line);
    }
    link.push_line("[]");
    link.push_line(json!([7, "text"]).to_string());
    link.push_line(notification("after"));

    let handled = handler.wait_for_notifications(1).await;
    let observed = (
        methods_seen(&handled),
        link.sent().len(),
        client.is_closed(),
    );
    assert_eq!(
        observed,
        (vec!["after"], 0, false),
        "expected nothing written and the next frame handled: ([\"after\"], 0, false) | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// Asked to, a client answers a line that is JSON and neither an object nor an array with an
/// invalid-request error and a null id, once for each such line, and goes on reading.
#[tokio::test]
async fn a_line_that_is_json_and_not_a_request_is_answered_as_an_invalid_request() {
    let (link, handler, client) = connect(WireOptions::new().with_invalid_request_replies(8));
    for line in NOT_REQUESTS {
        link.push_line(line);
    }
    link.push_line(notification("after"));

    let handled = handler.wait_for_notifications(1).await;
    sent(&link, NOT_REQUESTS.len()).await;
    let observed = (frames(&link), methods_seen(&handled), client.is_closed());
    let expected = (
        vec![invalid_request(); NOT_REQUESTS.len()],
        vec!["after"],
        false,
    );
    assert_eq!(
        observed, expected,
        "expected one invalid-request error for each such line and the next frame handled: {expected:?} | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// A dialect without the version header gets the answer without it as well.
#[tokio::test]
async fn an_invalid_request_is_answered_in_the_dialect_of_the_connection() {
    let link = ScriptedLink::new();
    let handler = RecordingHandler::arc(None);
    let mut dialect = options();
    dialect.include_version_header = false;
    let client = Client::connect_with(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        dialect,
        WireOptions::new().with_invalid_request_replies(8),
    );
    link.push_line("5");

    sent(&link, 1).await;
    let written = frames(&link);
    let expected = vec![json!({
        "id": null,
        "error": { "code": -32600, "message": "Invalid Request" },
    })];
    assert_eq!(
        written, expected,
        "expected the error without a version member: {expected:?} | received {written:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// With batches read, an empty batch is answered once, and so is each member of a batch that
/// is not an object. The members that are frames are handled as they are without the option.
#[tokio::test]
async fn an_empty_batch_and_each_member_that_is_not_an_object_are_answered() {
    let (link, handler, client) = connect(
        WireOptions::new()
            .with_batch_decoding()
            .with_invalid_request_replies(8),
    );
    link.push_line("[]");
    link.push_line(
        json!([
            { "jsonrpc": "2.0", "method": "one", "params": {} },
            7,
            [{ "jsonrpc": "2.0", "method": "nested", "params": {} }],
            { "jsonrpc": "2.0", "method": "two", "params": {} },
        ])
        .to_string(),
    );

    let handled = handler.wait_for_notifications(2).await;
    sent(&link, 3).await;
    let observed = (frames(&link), methods_seen(&handled));
    let expected = (vec![invalid_request(); 3], vec!["one", "two"]);
    assert_eq!(
        observed, expected,
        "expected one error for the empty batch and one for each member that is not an object, the frames handled in order: {expected:?} | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// An array to a client that does not read batches is a request it was not asked to read, not
/// an invalid one, and an object is routed as it always was whatever it holds. Neither is
/// answered, and neither is a line that is not JSON: that is a parse error, which this client
/// was not asked to answer.
#[tokio::test]
async fn what_is_not_an_invalid_request_is_not_answered_as_one() {
    let (link, handler, client) = connect(WireOptions::new().with_invalid_request_replies(8));
    link.push_line("[]");
    link.push_line(json!([notification("in a batch")]).to_string());
    link.push_line("{}");
    link.push_line(json!({ "jsonrpc": "2.0", "id": 4, "method": 5 }).to_string());
    link.push_line("{oops");
    link.push_line(notification("after"));

    let handled = handler.wait_for_notifications(1).await;
    let observed = (
        methods_seen(&handled),
        link.sent().len(),
        client.is_closed(),
    );
    assert_eq!(
        observed,
        (vec!["after"], 0, false),
        "expected none of it answered and the next frame handled: ([\"after\"], 0, false) | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// At most the configured number of these answers wait to be written at once. With the link
/// busy, input past the bound is dropped as it is without the option.
#[tokio::test]
async fn invalid_request_replies_are_bounded_by_how_many_wait_to_be_written() {
    let gated = gated(
        options(),
        WireOptions::new().with_invalid_request_replies(2),
    );
    let _held = link_held(&gated).await;
    for _ in 0..5 {
        gated.link.push_line("5");
    }
    gated.link.push_line(notification("after"));
    gated.handler.wait_for_notifications(1).await;

    let waiting = gated.client.state.outbox.aside();
    assert_eq!(
        waiting, 2,
        "expected two errors waiting to be written, the other three dropped: 2 | received {waiting}"
    );
    gated.gate.add_permits(1);
    sent(&gated.link, 3).await;
    until("the answers to be written", || {
        gated.client.state.outbox.aside() == 0
    })
    .await;
    // Room again once they are written.
    gated.link.push_line("5");
    sent(&gated.link, 4).await;
    let written = gated.link.sent().len();
    assert_eq!(
        written, 4,
        "expected the held frame, the two errors and one more once there was room: 4 | received {written}"
    );
    gated.client.close().await.expect("expected a clean close");
}

/// The bound is this option's own. With as many parse errors waiting as that option allows,
/// an invalid request is still answered, and the other way round.
#[tokio::test]
async fn the_bound_on_invalid_request_replies_is_apart_from_the_one_on_parse_errors() {
    let gated = gated(
        options(),
        WireOptions::new()
            .with_parse_error_replies(1)
            .with_invalid_request_replies(1),
    );
    let _held = link_held(&gated).await;
    gated.link.push_line("{oops");
    gated.link.push_line("{oops");
    gated.link.push_line("5");
    gated.link.push_line("5");
    gated.link.push_line(notification("after"));
    gated.handler.wait_for_notifications(1).await;

    gated.gate.add_permits(1);
    sent(&gated.link, 3).await;
    let codes: Vec<Value> = frames(&gated.link)
        .iter()
        .skip(1)
        .map(|frame| frame["error"]["code"].clone())
        .collect();
    assert_eq!(
        codes,
        vec![json!(-32700), json!(-32600)],
        "expected one answer of each kind, in the order the lines came: [-32700, -32600] | received {codes:?}"
    );
    gated.client.close().await.expect("expected a clean close");
}

/// The answers are held beside the bounds of the outbound queue. With the queue's one place
/// taken, an invalid request is answered all the same, the connection stays, and the host's
/// own room is what it was.
#[tokio::test]
async fn invalid_request_replies_take_none_of_the_room_the_queue_leaves_the_host() {
    let gated = gated(
        options(),
        limits(usize::MAX, usize::MAX, 2).with_invalid_request_replies(4),
    );
    let _held = link_held(&gated).await;
    gated.link.push_line("5");
    gated.link.push_line(notification("after"));
    gated.handler.wait_for_notifications(1).await;

    let mine = gated.client.submit_notification("mine", json!({}));
    let observed = (
        mine.is_ok(),
        outbox_holds(&gated.client).0,
        gated.client.state.outbox.aside(),
        gated.client.is_closed(),
    );
    assert_eq!(
        observed,
        (true, 2, 1, false),
        "expected the host's frame queued in the room that was left, the error beside it: (true, 2, 1, false) | received {observed:?}"
    );
    gated.gate.add_permits(1);
    within("the host's frame", mine.expect("expected the frame queued"))
        .await
        .expect("expected the frame written");
    gated.client.close().await.expect("expected a clean close");
}

/// The answer is in the queue ahead of what is written after it: on a link nobody holds, a
/// notification the host writes for itself takes its turn behind an answer still queued.
#[tokio::test]
async fn a_write_made_on_an_idle_link_follows_an_invalid_request_reply_still_queued() {
    let (link, _handler, client) = connect(WireOptions::new().with_invalid_request_replies(4));
    // Queued from here, so nothing has run between this and the write that follows.
    client.state.answer_invalid_request();
    within("the host's notification", client.notify("mine", json!({})))
        .await
        .expect("expected the notification written");

    let written: Vec<String> = frames(&link)
        .iter()
        .map(|frame| {
            frame["method"]
                .as_str()
                .map_or_else(|| frame["error"]["code"].to_string(), str::to_owned)
        })
        .collect();
    assert_eq!(
        written,
        vec!["-32600", "mine"],
        "expected the error ahead of the host's notification: [\"-32600\", \"mine\"] | received {written:?}"
    );
    client.close().await.expect("expected a clean close");
}
