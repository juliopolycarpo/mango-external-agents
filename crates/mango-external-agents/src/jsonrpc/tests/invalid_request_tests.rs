//! JSON that is not a request, answered with `-32600` only by a client that was asked to.

use super::outbox_tests::{
    gated, limits, link_held, one_termination, options, outbox_holds, sent, until, within,
};
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

/// The invalid-request error, under `id`.
fn invalid_request(id: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
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

/// Writes a probe and says what is on the wire with it: every frame written, what is still
/// queued beside the bounds, and whether the client closed. A reply queued and not yet
/// written would go out ahead of the probe.
async fn up_to_a_probe(link: &ScriptedLink, client: &Client) -> (Vec<Value>, usize, bool) {
    within("the probe", client.notify("probe", json!({})))
        .await
        .expect("expected the probe written");
    (
        frames(link),
        client.state.outbox.aside(),
        client.is_closed(),
    )
}

fn probe() -> Value {
    json!({ "jsonrpc": "2.0", "method": "probe", "params": {} })
}

/// Every option that has this side write something the peer did not ask for.
fn every_reply() -> WireOptions {
    WireOptions::new()
        .with_batch_decoding()
        .with_parse_error_replies(8)
        .with_invalid_request_replies(8)
}

/// Lines that are JSON and neither an object nor an array.
const SCALARS: [&str; 5] = ["5", "\"text\"", "true", "false", "null"];

/// Objects that cannot be requests and are not responses, with the id each is answered under
/// by a client that answers invalid requests.
fn invalid_objects() -> Vec<(Value, Value)> {
    vec![
        (json!({}), Value::Null),
        (json!({ "jsonrpc": "2.0" }), Value::Null),
        (json!({ "method": 1 }), Value::Null),
        (json!({ "id": 4, "method": 5 }), json!(4)),
        (json!({ "id": "a", "method": null }), json!("a")),
        (json!({ "method": "x", "id": {} }), Value::Null),
        (json!({ "method": "x", "id": true }), Value::Null),
        (json!({ "id": null }), Value::Null),
        (json!({ "id": [1] }), Value::Null),
        (json!({ "id": {} }), Value::Null),
        (json!({ "method": "x", "id": [1] }), Value::Null),
    ]
}

/// A client that was not asked answers none of it, and routes every object as it always did.
/// What is not a frame is dropped. An object with a method that is a string is put to the
/// handler whatever its id is, and answered under that id as it came; an object with an id
/// and no method that is a string is read as a response, here to nobody.
#[tokio::test]
async fn a_client_that_was_not_asked_answers_nothing_and_routes_objects_as_before() {
    let (link, handler, client) = connect(WireOptions::new().with_batch_decoding());
    for line in SCALARS {
        link.push_line(line);
    }
    link.push_line("[]");
    link.push_line("[[]]");
    link.push_line(json!([7, "text"]).to_string());
    for (object, _) in invalid_objects() {
        link.push_line(object.to_string());
    }
    link.push_line(notification("after"));

    let handled = handler.wait_for_notifications(1).await;
    // The three objects with a method that is a string are questions to this client.
    sent(&link, 3).await;
    let observed = (methods_seen(&handled), up_to_a_probe(&link, &client).await);
    let expected = (
        vec!["after"],
        (
            vec![
                json!({ "jsonrpc": "2.0", "id": {}, "result": "yes" }),
                json!({ "jsonrpc": "2.0", "id": true, "result": "yes" }),
                json!({ "jsonrpc": "2.0", "id": [1], "result": "yes" }),
                probe(),
            ],
            0,
            false,
        ),
    );
    assert_eq!(
        observed, expected,
        "expected no invalid-request error, and the three questions answered under the ids they came with: {expected:?} | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// The same client reads an object with an id and a method that is not a string as the
/// response to the call that id names, with no result, as it always has.
#[tokio::test]
async fn a_client_that_was_not_asked_reads_an_id_with_a_bad_method_as_a_response() {
    let (link, _handler, client) = connect(WireOptions::new());
    let (written, reply) = client
        .submit_request("ask", json!({}), RequestOptions::new())
        .expect("expected the request queued")
        .into_parts();
    within("the request written", written)
        .await
        .expect("expected the request written");
    link.push_line(json!({ "id": "1", "method": 5 }).to_string());

    let answer = within("the reply", reply).await;
    let observed = (answer.ok(), link.sent().len());
    assert_eq!(
        observed,
        (Some(Value::Null), 1),
        "expected the call answered with no result and nothing written for it: (Some(Null), 1) | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// Asked to, a client answers a line that is JSON and neither an object nor an array with an
/// invalid-request error and a null id, once for each such line, and goes on reading.
#[tokio::test]
async fn a_line_that_is_json_and_not_a_request_is_answered_as_an_invalid_request() {
    let (link, handler, client) = connect(WireOptions::new().with_invalid_request_replies(8));
    for line in SCALARS {
        link.push_line(line);
    }
    link.push_line(notification("after"));

    let handled = handler.wait_for_notifications(1).await;
    sent(&link, SCALARS.len()).await;
    let observed = (frames(&link), methods_seen(&handled), client.is_closed());
    let expected = (
        vec![invalid_request(Value::Null); SCALARS.len()],
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
/// is not an object, a batch inside the batch included. The members that are frames are
/// handled as they are without the option.
#[tokio::test]
async fn an_empty_batch_and_each_member_that_is_not_an_object_are_answered() {
    let (link, handler, client) = connect(
        WireOptions::new()
            .with_batch_decoding()
            .with_invalid_request_replies(8),
    );
    link.push_line("[]");
    link.push_line("[[]]");
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
    sent(&link, 4).await;
    let observed = (frames(&link), methods_seen(&handled));
    let expected = (vec![invalid_request(Value::Null); 4], vec!["one", "two"]);
    assert_eq!(
        observed, expected,
        "expected one error for the empty batch, one for the batch of an empty batch, and one for each member that is not an object, the frames handled in order: {expected:?} | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// A client that does not read batches cannot put one to its handler. Asked to answer invalid
/// requests, it answers the whole line once, under a null id, empty or not.
#[tokio::test]
async fn a_batch_to_a_client_that_reads_none_is_answered_once() {
    let (link, handler, client) = connect(WireOptions::new().with_invalid_request_replies(8));
    link.push_line("[]");
    link.push_line(
        json!([{ "jsonrpc": "2.0", "method": "in a batch", "params": {} }, 5, 6]).to_string(),
    );
    link.push_line(notification("after"));

    let handled = handler.wait_for_notifications(1).await;
    sent(&link, 2).await;
    let observed = (methods_seen(&handled), up_to_a_probe(&link, &client).await);
    let expected = (
        vec!["after"],
        (
            vec![
                invalid_request(Value::Null),
                invalid_request(Value::Null),
                probe(),
            ],
            0,
            false,
        ),
    );
    assert_eq!(
        observed, expected,
        "expected one error for each line that was a batch, and none of its members handled: {expected:?} | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// An object that is not a response and cannot be a request is answered: under its own id
/// when that is a string or a number, under null otherwise. None of them reaches the handler,
/// on a line of its own or in a batch.
#[tokio::test]
async fn an_object_that_cannot_be_a_request_is_answered_under_the_id_it_can_name() {
    for batched in [false, true] {
        let (link, handler, client) = connect(
            WireOptions::new()
                .with_batch_decoding()
                .with_invalid_request_replies(16),
        );
        let objects = invalid_objects();
        if batched {
            let members: Vec<Value> = objects.iter().map(|(object, _)| object.clone()).collect();
            link.push_line(Value::Array(members).to_string());
        } else {
            for (object, _) in &objects {
                link.push_line(object.to_string());
            }
        }
        link.push_line(notification("after"));

        let handled = handler.wait_for_notifications(1).await;
        sent(&link, objects.len()).await;
        let mut expected: Vec<Value> = objects
            .into_iter()
            .map(|(_, id)| invalid_request(id))
            .collect();
        expected.push(probe());
        let observed = (methods_seen(&handled), up_to_a_probe(&link, &client).await);
        let expected = (vec!["after"], (expected, 0, false));
        assert_eq!(
            observed, expected,
            "expected each object answered under the id it can name and none put to the handler (batched: {batched}): {expected:?} | received {observed:?}"
        );
        client.close().await.expect("expected a clean close");
    }
}

/// An object with the id of a call in flight and a method that is not a string is answered as
/// an invalid request, where a client that was not asked takes it for the call's response.
/// The call goes on waiting, and the response that follows reaches it.
#[tokio::test]
async fn an_invalid_request_with_the_id_of_a_call_in_flight_does_not_answer_the_call() {
    let (link, _handler, client) = connect(WireOptions::new().with_invalid_request_replies(8));
    let (written, reply) = client
        .submit_request("ask", json!({}), RequestOptions::new())
        .expect("expected the request queued")
        .into_parts();
    within("the request written", written)
        .await
        .expect("expected the request written");
    link.push_line(json!({ "id": "1", "method": 5 }).to_string());
    sent(&link, 2).await;
    let still_waiting = client.state.pending.lock().await.len();
    link.push_line(json!({ "jsonrpc": "2.0", "id": "1", "result": "real" }).to_string());

    let answer = within("the reply", reply).await;
    let observed = (still_waiting, answer.ok(), frames(&link).pop());
    let expected = (1, Some(json!("real")), Some(invalid_request(json!("1"))));
    assert_eq!(
        observed, expected,
        "expected the call still waiting after the invalid request, then answered by the response, and the error under the id it came with: {expected:?} | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// What is a response, or a request read leniently, is never answered as invalid, with every
/// option on that has this side answer unasked. That includes this side's own replies fed
/// back to it: each carries `error`, so two peers that answer invalid requests cannot answer
/// each other for ever.
#[tokio::test]
async fn what_is_a_response_or_a_lenient_request_is_never_answered_as_invalid() {
    let (link, handler, client) = connect(every_reply());
    let (written, reply) = client
        .submit_request("ask", json!({}), RequestOptions::new())
        .expect("expected the request queued")
        .into_parts();
    within("the request written", written)
        .await
        .expect("expected the request written");
    // A response to nobody, a response with both members, and this side's own two replies.
    link.push_line(json!({ "result": 1 }).to_string());
    link.push_line(json!({ "error": {} }).to_string());
    link.push_line(json!({ "id": 99, "result": 1 }).to_string());
    link.push_line(
        json!({ "id": 9, "result": 1, "error": { "code": 1, "message": "m" } }).to_string(),
    );
    link.push_line(invalid_request(Value::Null).to_string());
    link.push_line(invalid_request(json!(4)).to_string());
    link.push_line(
        json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "Parse error" } })
            .to_string(),
    );
    // Read leniently, as before: params that are not structured, and another version.
    link.push_line(json!({ "method": "lenient params", "params": 1 }).to_string());
    link.push_line(json!({ "jsonrpc": "1.0", "method": "other version" }).to_string());
    // An id alone is the response it always was: the call in flight gets no result.
    link.push_line(json!({ "id": "1" }).to_string());
    link.push_line(notification("after"));

    let answer = within("the reply", reply).await;
    let handled = handler.wait_for_notifications(3).await;
    let (written, queued, closed) = up_to_a_probe(&link, &client).await;
    let observed = (
        answer.ok(),
        methods_seen(&handled),
        written.len(),
        written.last().cloned(),
        queued,
        closed,
    );
    let expected = (
        Some(Value::Null),
        vec!["lenient params", "other version", "after"],
        2,
        Some(probe()),
        0,
        false,
    );
    assert_eq!(
        observed, expected,
        "expected the call answered by the bare id, both lenient requests handled, and nothing written but the request and the probe: {expected:?} | received {observed:?}"
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
    client.state.answer_invalid_request(Value::Null);
    let (written, _, _) = up_to_a_probe(&link, &client).await;
    let expected = vec![invalid_request(Value::Null), probe()];
    assert_eq!(
        written, expected,
        "expected the error ahead of the host's notification: {expected:?} | received {written:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// A bound of nothing answers nothing, and a batch with more members to answer than the bound
/// allows gets as many answers as the bound and no more.
///
/// This holds on a current-thread runtime whichever way the bound is counted: the replies are
/// not written while the batch is read, so the number still waiting stops the batch as well as
/// the count kept for it does. The test that shows the count kept for the batch is what stops
/// it is `the_bound_on_one_batch_does_not_start_again_as_its_replies_are_written`.
#[tokio::test]
async fn the_bound_holds_at_nothing_and_inside_one_batch() {
    for bound in [0, 2] {
        let (link, handler, client) = connect(
            WireOptions::new()
                .with_batch_decoding()
                .with_invalid_request_replies(bound),
        );
        link.push_line(json!([1, 2, 3]).to_string());
        link.push_line(notification("after"));
        handler.wait_for_notifications(1).await;

        let (written, queued, closed) = up_to_a_probe(&link, &client).await;
        let observed = (written.len() - 1, queued, closed);
        let expected = (bound, 0, false);
        assert_eq!(
            observed, expected,
            "expected a bound of {bound} to answer that many of the batch's three members and no more: {expected:?} | received {observed:?} ({written:?})"
        );
        client.close().await.expect("expected a clean close");
    }
}

/// The bound on one batch does not depend on how fast its answers are written. On four
/// threads, with a peer that reads at once, the writer takes each answer as it is queued, so
/// the count of what still waits never reaches the bound; the batch is answered for as many
/// members as the bound all the same, round after round, and not for the hundreds it holds.
///
/// A stress companion of the test that proves it: whether the writer takes an answer before
/// the next member is read is a matter of scheduling, so a run of this test does not by itself
/// show that the count kept for the batch is what holds the bound. That is shown by
/// `the_bound_on_one_batch_does_not_start_again_as_its_replies_are_written`, which lets every
/// waiting reply go at the moment it is queued.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_batch_is_answered_for_no_more_members_than_the_bound_however_fast_it_is_written() {
    const ROUNDS: usize = 40;
    const MEMBERS: usize = 400;
    let (link, handler, client) = connect(
        WireOptions::new()
            .with_batch_decoding()
            .with_invalid_request_replies(2),
    );
    let mut members = vec![json!(1); MEMBERS / 2];
    members.extend(vec![json!({ "method": 1 }); MEMBERS / 2]);
    let batch = Value::Array(members).to_string();
    for round in 1..=ROUNDS {
        link.push_line(batch.clone());
        link.push_line(notification("after"));
        handler.wait_for_notifications(round).await;
        until("the answers to be written", || {
            client.state.outbox.aside() == 0
        })
        .await;
    }

    let (written, queued, closed) = up_to_a_probe(&link, &client).await;
    let observed = (written.len() - 1, queued, closed);
    let expected = (2 * ROUNDS, 0, false);
    assert_eq!(
        observed, expected,
        "expected two answers for each of the {ROUNDS} batches of {MEMBERS} members: {expected:?} | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// What the bound is kept on does not grow with the connection. After a long run of inputs
/// each answered and written before the next, the list of replies holds no more than the last
/// one, for parse errors and for invalid requests alike, however large the bound.
#[tokio::test]
async fn the_replies_kept_for_the_bound_do_not_grow_with_what_was_answered() {
    let (link, _handler, client) = connect(
        WireOptions::new()
            .with_parse_error_replies(10_000)
            .with_invalid_request_replies(10_000),
    );
    let mut most = (0, 0);
    for round in 1..=100 {
        link.push_line("5");
        link.push_line("{oops");
        sent(&link, 2 * round).await;
        let held = (
            client.state.invalid_request_replies.held(),
            client.state.parse_error_replies.held(),
        );
        most = (most.0.max(held.0), most.1.max(held.1));
    }

    assert!(
        most.0 <= 1 && most.1 <= 1,
        "expected no more than the last reply of each kind kept after 100 of each were written: (at most 1, at most 1) | received {most:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// A batch refused whole, for passing the handoff budget, is not answered for the members it
/// holds that are not objects: nothing of such a batch is handled, and the connection ends.
#[tokio::test]
async fn a_batch_refused_whole_is_not_answered_for_its_invalid_members() {
    let link = ScriptedLink::new();
    let handler = RecordingHandler::arc(None);
    let client = Client::connect_with(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options().with_max_pending_notifications(2),
        WireOptions::new()
            .with_batch_decoding()
            .with_invalid_request_replies(8),
    );
    let mut members = vec![json!(5), json!({ "method": 1 })];
    members.extend((0..3).map(|n| json!({ "jsonrpc": "2.0", "method": format!("n{n}") })));
    link.push_line(Value::Array(members).to_string());

    let termination = one_termination(&handler).await;
    // The list the bound is kept on is read as well as the wire: a reply queued ahead of the
    // refusal would sit in it whether or not the writer had got to it before the end.
    let observed = (
        &termination,
        link.sent().len(),
        client.state.outbox.aside(),
        client.state.invalid_request_replies.held(),
    );
    assert!(
        matches!(
            &observed,
            (
                PeerTermination::NotificationBackpressure {
                    limit: 2,
                    received: 3,
                    ..
                },
                0,
                0,
                0
            )
        ),
        "expected the batch refused for its three notifications, with nothing written, queued or kept for its other members: (NotificationBackpressure {{ limit: 2, received: 3 }}, 0, 0, 0) | received {observed:?}"
    );
    let _ = client.close().await;
}

/// What turning the answers on costs, as for parse errors: an answer is a write, and a peer
/// that sends invalid requests while it does not read leaves that write waiting. When its
/// deadline runs out the connection ends.
#[tokio::test(start_paused = true)]
async fn an_invalid_request_reply_the_peer_never_reads_ends_the_connection_at_the_write_deadline() {
    let script = ScriptedLink::new();
    let (sender, receiver) = script.clone().into_link().split();
    let handler = RecordingHandler::arc(None);
    let client = Client::connect_with(
        crate::link::Link::new(Box::new(StalledReplies { inner: sender }), receiver),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options().with_request_timeout(Duration::from_millis(50)),
        WireOptions::new().with_invalid_request_replies(4),
    );
    let began = tokio::time::Instant::now();
    script.push_line("5");

    let termination = one_termination(&handler).await;
    let observed = (began.elapsed(), &termination);
    assert!(
        matches!(observed, (waited, PeerTermination::LinkFailed(_))
            if waited >= Duration::from_millis(50) && waited < Duration::from_millis(100)),
        "expected the connection ended by the stalled write at its deadline, not before: (50ms or just after, LinkFailed) | received {observed:?}"
    );
    // The link is unusable by now; what the close makes of that is not this test's subject.
    let _ = client.close().await;
}

/// One batch is answered for as many of its members as the bound and no more, whatever has
/// been written meanwhile. With every reply let go the moment the first is queued, as a writer
/// on another thread could have it, the count of what still waits starts again from nothing,
/// and the batch's own count is what stops at two.
#[tokio::test]
async fn the_bound_on_one_batch_does_not_start_again_as_its_replies_are_written() {
    let (link, handler, client) = connect(
        WireOptions::new()
            .with_batch_decoding()
            .with_invalid_request_replies(2),
    );
    {
        let state = Arc::clone(&client.state);
        *client
            .state
            .after_batch_member_answered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(Box::new(move || state.invalid_request_replies.forget()));
    }
    link.push_line(json!([1, 2, 3, 4]).to_string());
    link.push_line(notification("after"));
    handler.wait_for_notifications(1).await;

    let (written, queued, closed) = up_to_a_probe(&link, &client).await;
    let observed = (written.len() - 1, queued, closed);
    assert_eq!(
        observed,
        (2, 0, false),
        "expected two of the batch's four members answered, the bound counted for the batch: (2, 0, false) | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// The rest of the table, with the option on. A request whose id is null is a request, put to
/// the handler and answered under null. A response whose result is null is a response, and so
/// is an object with an id and no method, whatever else it holds. A number is sent back as it
/// came, negative or fractional. A string that takes more than 128 bytes written, its two
/// quotes included, is not sent back: the reply is under null and stays small.
#[tokio::test]
async fn ids_are_read_and_sent_back_for_what_they_are() {
    let (link, handler, client) = connect(WireOptions::new().with_invalid_request_replies(8));
    let (written, reply) = client
        .submit_request("ask", json!({}), RequestOptions::new())
        .expect("expected the request queued")
        .into_parts();
    within("the request written", written)
        .await
        .expect("expected the request written");
    link.push_line(json!({ "jsonrpc": "2.0", "id": 1, "result": null }).to_string());
    let answer = within("the reply", reply).await;
    link.push_line(json!({ "jsonrpc": "2.0", "id": 2, "extra": true }).to_string());
    link.push_line(json!({ "method": "question", "id": null }).to_string());
    link.push_line(json!({ "id": -1.5, "method": 7 }).to_string());
    link.push_line(json!({ "id": "s".repeat(126), "method": 7 }).to_string());
    link.push_line(json!({ "id": "s".repeat(127), "method": 7 }).to_string());
    link.push_line(notification("after"));

    let handled = handler.wait_for_notifications(1).await;
    sent(&link, 5).await;
    let (written, queued, closed) = up_to_a_probe(&link, &client).await;
    // The handler answers on a task of its own, so its answer has no fixed place among the
    // errors, which the reader queues in the order it read what they answer.
    let (answers, errors): (Vec<Value>, Vec<Value>) = written[1..]
        .iter()
        .filter(|frame| frame.get("method").is_none())
        .cloned()
        .partition(|frame| frame.get("result").is_some());
    let observed = (
        answer.ok(),
        methods_seen(&handled),
        answers,
        errors,
        queued,
        closed,
    );
    let expected = (
        Some(Value::Null),
        vec!["after"],
        vec![json!({ "jsonrpc": "2.0", "id": null, "result": "yes" })],
        vec![
            invalid_request(json!(-1.5)),
            invalid_request(json!("s".repeat(126))),
            invalid_request(Value::Null),
        ],
        0,
        false,
    );
    assert_eq!(
        observed, expected,
        "expected the response read as one, the question with a null id answered by the handler, and each id sent back or not for what it is: {expected:?} | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// A bound of nothing is the same as not asking: nothing is answered, and every object is
/// routed as it is without the option. The question with an id that cannot be one still
/// reaches the handler, and the id with a method that is not a string still answers its call.
#[tokio::test]
async fn a_bound_of_nothing_routes_every_object_as_before() {
    let (link, handler, client) = connect(WireOptions::new().with_invalid_request_replies(0));
    let (written, reply) = client
        .submit_request("ask", json!({}), RequestOptions::new())
        .expect("expected the request queued")
        .into_parts();
    within("the request written", written)
        .await
        .expect("expected the request written");
    link.push_line(json!({ "id": "1", "method": 5 }).to_string());
    link.push_line(json!({ "method": "x", "id": {} }).to_string());
    link.push_line("5");
    link.push_line("{}");
    link.push_line(notification("after"));

    let answer = within("the reply", reply).await;
    let handled = handler.wait_for_notifications(1).await;
    sent(&link, 2).await;
    let (written, queued, closed) = up_to_a_probe(&link, &client).await;
    let observed = (
        answer.ok(),
        methods_seen(&handled),
        written[1..].to_vec(),
        queued,
        closed,
    );
    let expected = (
        Some(Value::Null),
        vec!["after"],
        vec![
            json!({ "jsonrpc": "2.0", "id": {}, "result": "yes" }),
            probe(),
        ],
        0,
        false,
    );
    assert_eq!(
        observed, expected,
        "expected the call answered, the question put to the handler, and no invalid-request error: {expected:?} | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// A member of a batch that is answered as an invalid request is not something for the
/// handler, though it has a method: it takes none of the handoff budget. A batch of two
/// notifications and such a member fits a budget of two, and the line's bytes, which the two
/// notifications hold between them, are all given back when both are handled.
#[tokio::test]
async fn a_batch_member_answered_as_invalid_takes_none_of_the_handlers_budget() {
    let link = ScriptedLink::new();
    let handler = RecordingHandler::arc(None);
    let client = Client::connect_with(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options().with_max_pending_notifications(2),
        WireOptions::new()
            .with_batch_decoding()
            .with_invalid_request_replies(8),
    );
    let bytes = client.state.peer_bytes.available_permits();
    link.push_line(
        json!([
            { "jsonrpc": "2.0", "method": "one" },
            { "method": "x", "id": {} },
            { "jsonrpc": "2.0", "method": "two" },
        ])
        .to_string(),
    );

    let handled = handler.wait_for_notifications(2).await;
    sent(&link, 1).await;
    until("the batch's bytes to be given back", || {
        client.state.peer_bytes.available_permits() == bytes
    })
    .await;
    let observed = (methods_seen(&handled), frames(&link), client.is_closed());
    let expected = (
        vec!["one", "two"],
        vec![invalid_request(Value::Null)],
        false,
    );
    assert_eq!(
        observed, expected,
        "expected both notifications handled within a budget of two, the third member answered, and the connection open: {expected:?} | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// The id is sent back only when it is short as this side writes it. A string of quotes and
/// control characters is measured with its escapes: sixty-three such characters and the two
/// quotes are 128 bytes written, the most that is sent back, and sixty-four are past it. The
/// reply to the first is the longest there is: its 75 bytes and the id's 128. A number is
/// written again from what was read: a long numeral that is a number this side can hold
/// comes back as it prints that number, short, and not digit for digit.
#[tokio::test]
async fn an_id_is_sent_back_only_when_it_is_short_as_written() {
    let (link, _handler, client) = connect(WireOptions::new().with_invalid_request_replies(8));
    let escaped = |count: usize| "\"".repeat(count / 2) + &"\n".repeat(count - count / 2);
    let long_numeral = format!("0.{}", "1".repeat(400));
    link.push_line(json!({ "id": escaped(63), "method": 7 }).to_string());
    link.push_line(json!({ "id": escaped(64), "method": 7 }).to_string());
    link.push_line(format!(r#"{{"id":{long_numeral},"method":7}}"#));

    sent(&link, 3).await;
    let written = link.sent();
    let ids: Vec<Value> = frames(&link)
        .iter()
        .map(|frame| frame["id"].clone())
        .collect();
    let longest = written.iter().map(String::len).max().unwrap_or(0);
    let numeral_written = ids[2].to_string();
    let observed = (
        ids[..2].to_vec(),
        ids[2].is_number() && numeral_written.len() < 30,
        longest,
    );
    let expected = (vec![json!(escaped(63)), Value::Null], true, 75 + 128);
    assert_eq!(
        observed, expected,
        "expected the short string sent back, the long one answered under null, the numeral sent back as a short number, and the longest reply its 75 bytes and the 128 an id may take: {expected:?} | received {observed:?} (numeral {numeral_written})"
    );
    client.close().await.expect("expected a clean close");
}

/// The rule itself, on what a reader built with exact numbers would hand it: an id is sent
/// back when it takes at most 128 bytes as written, a string or a number alike, and anything
/// else an id can be is answered under null.
#[test]
fn an_id_is_measured_as_it_would_be_written() {
    use crate::jsonrpc::echoed_id;
    let at_the_cap = json!("s".repeat(126));
    let past_it = json!("s".repeat(127));
    let escapes_past_it = json!("\u{1}".repeat(22));
    let observed = (
        echoed_id(Some(&at_the_cap)),
        echoed_id(Some(&past_it)),
        echoed_id(Some(&escapes_past_it)),
        echoed_id(Some(&json!(-0.5))),
        echoed_id(Some(&json!(null))),
        echoed_id(Some(&json!([1]))),
        echoed_id(None),
    );
    let expected = (
        at_the_cap.clone(),
        Value::Null,
        Value::Null,
        json!(-0.5),
        Value::Null,
        Value::Null,
        Value::Null,
    );
    assert_eq!(
        observed, expected,
        "expected ids of at most 128 written bytes sent back and the rest answered under null: {expected:?} | received {observed:?}"
    );
}

/// An object with a method that is a string is a request or a notification, as it always was,
/// even when it carries `result` as well: the method decides, and it is put to the handler.
#[tokio::test]
async fn a_method_that_is_a_string_decides_even_beside_a_result() {
    let (link, handler, client) = connect(WireOptions::new().with_invalid_request_replies(8));
    link.push_line(json!({ "id": 1, "method": "asked", "result": 1 }).to_string());
    link.push_line(json!({ "method": "told", "error": {} }).to_string());

    let handled = handler.wait_for_notifications(1).await;
    sent(&link, 1).await;
    let observed = (methods_seen(&handled), up_to_a_probe(&link, &client).await);
    let expected = (
        vec!["told"],
        (
            vec![
                json!({ "jsonrpc": "2.0", "id": 1, "result": "yes" }),
                probe(),
            ],
            0,
            false,
        ),
    );
    assert_eq!(
        observed, expected,
        "expected the question answered by the handler and the notification handled, with no invalid-request error: {expected:?} | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// An object with an id and nothing else that says what it is, such as `{"id":"1","extra":true}`,
/// is the response to the call that id names, with no result, whether or not the client was
/// asked to answer invalid requests: it is not an invalid request, and is not diverted.
/// A pin of what a client always did.
#[tokio::test]
async fn an_object_with_an_id_and_nothing_else_is_a_response_whether_or_not_replies_are_asked_for()
{
    for wire in [
        WireOptions::new(),
        WireOptions::new().with_invalid_request_replies(8),
    ] {
        let asked = wire.invalid_request_replies();
        let (link, _handler, client) = connect(wire);
        let (written, reply) = client
            .submit_request("ask", json!({}), RequestOptions::new())
            .expect("expected the request queued")
            .into_parts();
        within("the request written", written)
            .await
            .expect("expected the request written");
        link.push_line(json!({ "id": "1", "extra": true }).to_string());

        let answer = within("the reply", reply).await;
        let observed = (answer.ok(), link.sent().len(), client.state.outbox.aside());
        assert_eq!(
            observed,
            (Some(Value::Null), 1, 0),
            "expected the call answered with no result and nothing written for it (invalid-request replies asked for: {asked:?}): (Some(Null), 1, 0) | received {observed:?}"
        );
        client.close().await.expect("expected a clean close");
    }
}

/// An object with a method that is a string is put to the handler even beside a `result`,
/// when the client was not asked to answer invalid requests as well as when it was. A pin of
/// what a client always did; the other half of it is
/// `a_method_that_is_a_string_decides_even_beside_a_result`.
#[tokio::test]
async fn a_method_that_is_a_string_decides_beside_a_result_without_the_option_too() {
    let (link, handler, client) = connect(WireOptions::new());
    link.push_line(json!({ "id": 1, "method": "asked", "result": 1 }).to_string());
    link.push_line(json!({ "method": "told", "error": {} }).to_string());

    let handled = handler.wait_for_notifications(1).await;
    sent(&link, 1).await;
    let observed = (methods_seen(&handled), up_to_a_probe(&link, &client).await);
    let expected = (
        vec!["told"],
        (
            vec![
                json!({ "jsonrpc": "2.0", "id": 1, "result": "yes" }),
                probe(),
            ],
            0,
            false,
        ),
    );
    assert_eq!(
        observed, expected,
        "expected the question answered by the handler and the notification handled, with no invalid-request error: {expected:?} | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// A line that is not JSON is answered by the parse-error reply, and by that alone. A client
/// asked to answer invalid requests but not parse errors drops it, and goes on reading.
#[tokio::test]
async fn a_line_that_is_not_json_is_dropped_when_only_invalid_requests_are_answered() {
    let (link, handler, client) = connect(WireOptions::new().with_invalid_request_replies(8));
    link.push_line("{oops");
    link.push_line(notification("after"));

    let handled = handler.wait_for_notifications(1).await;
    let observed = (methods_seen(&handled), up_to_a_probe(&link, &client).await);
    let expected = (vec!["after"], (vec![probe()], 0, false));
    assert_eq!(
        observed, expected,
        "expected the line dropped, with no reply of either kind and the next frame handled: {expected:?} | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// An empty array is dropped by a client that reads no batches and was not asked to answer
/// invalid requests, and so is an array holding one. A pin of what a client always did; the
/// non-empty array is pinned in `wire_options_tests`.
#[tokio::test]
async fn an_empty_array_is_dropped_by_a_client_that_reads_no_batches_and_answers_nothing() {
    let (link, handler, client) = connect(WireOptions::new());
    link.push_line("[]");
    link.push_line("[[]]");
    link.push_line(notification("after"));

    let handled = handler.wait_for_notifications(1).await;
    let observed = (methods_seen(&handled), up_to_a_probe(&link, &client).await);
    let expected = (vec!["after"], (vec![probe()], 0, false));
    assert_eq!(
        observed, expected,
        "expected both arrays dropped, nothing written and the next frame handled: {expected:?} | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}
