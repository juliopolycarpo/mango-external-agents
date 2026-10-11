//! What a client does on the wire only when asked to: reading batches, answering lines that
//! are not JSON, and telling the peer about a request it gave up.

use super::outbox_tests::{
    assert_ended, gated, limits, link_held, methods, one_termination, options, ordered_options,
    outbox_holds, sent, until, within,
};
use super::*;
use crate::jsonrpc::{WireOptions, is_peer_work};

const CANCEL: &str = "$/cancel_request";

/// A client on a link that takes every write, with a handler that records and answers `yes`.
fn connect(
    options: ClientOptions,
    wire: WireOptions,
) -> (ScriptedLink, Arc<RecordingHandler>, Arc<Client>) {
    let link = ScriptedLink::new();
    let handler = RecordingHandler::arc(Some(ServerRequestOutcome::Answer(json!("yes"))));
    let client = Arc::new(Client::connect_with(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options,
        wire,
    ));
    (link, handler, client)
}

fn notification(method: &str) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": {} })
}

fn seen(notifications: &[(String, Value)]) -> Vec<&str> {
    notifications
        .iter()
        .map(|(method, _)| method.as_str())
        .collect()
}

fn frames(link: &ScriptedLink) -> Vec<Value> {
    link.sent()
        .iter()
        .map(|frame| serde_json::from_str(frame).expect("expected a JSON frame"))
        .collect()
}

/// A line that is a JSON array is not a frame to a client that was not asked to read batches:
/// it is dropped, as it always was, and the connection goes on.
#[tokio::test]
async fn a_batch_is_dropped_by_a_client_that_was_not_asked_to_read_batches() {
    let (link, handler, client) = connect(options(), WireOptions::new());
    link.push_line(json!([notification("one"), notification("two")]).to_string());
    link.push_line(notification("after").to_string());

    let handled = handler.wait_for_notifications(1).await;
    let observed = (seen(&handled), client.is_closed());
    assert_eq!(
        observed,
        (vec!["after"], false),
        "expected the batch dropped and the next frame handled: ([\"after\"], false) | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// A batch reaches the handler member by member, in the order the array gives them. A question
/// in it is answered with a frame of its own; a member that is not an object is passed over,
/// and so is an empty batch.
#[tokio::test]
async fn a_batch_reaches_the_handler_member_by_member_in_order() {
    let (link, handler, client) = connect(options(), WireOptions::new().with_batch_decoding());
    link.push_line("[]");
    link.push_line(
        json!([
            notification("one"),
            { "jsonrpc": "2.0", "id": 7, "method": "ask", "params": {} },
            5,
            notification("two"),
        ])
        .to_string(),
    );

    let handled = handler.wait_for_notifications(2).await;
    sent(&link, 1).await;
    let observed = (seen(&handled), frames(&link));
    let expected = (
        vec!["one", "two"],
        vec![json!({ "jsonrpc": "2.0", "id": 7, "result": "yes" })],
    );
    assert_eq!(
        observed, expected,
        "expected both notifications in order and the question answered on its own: {expected:?} | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// A response in a batch reaches the call that waits for it, ordered or not, and the
/// notification beside it reaches the handler.
#[tokio::test]
async fn a_response_in_a_batch_reaches_its_caller() {
    for request in [RequestOptions::new(), ordered_options()] {
        let (link, handler, client) = connect(options(), WireOptions::new().with_batch_decoding());
        let (written, reply) = client
            .submit_request("ask", json!({}), request)
            .expect("expected the request queued")
            .into_parts();
        within("the request written", written)
            .await
            .expect("expected the request written");
        link.push_line(
            json!([
                notification("before"),
                { "jsonrpc": "2.0", "id": "1", "result": "done" },
            ])
            .to_string(),
        );

        let answer = within("the reply", reply).await;
        let handled = handler.wait_for_notifications(1).await;
        let observed = (answer.as_ref().ok(), seen(&handled));
        assert_eq!(
            observed,
            (Some(&json!("done")), vec!["before"]),
            "expected the answer delivered and the notification handled: (Some(\"done\"), [\"before\"]) | received ({answer:?}, {handled:?})"
        );
        client.close().await.expect("expected a clean close");
    }
}

/// Each notification in a batch counts against the handoff queue on its own, and the batch is
/// counted whole before any of it is queued: one that fits is handled, one that does not ends
/// the connection with nothing of it handled, and the termination reports all of it.
#[tokio::test]
async fn a_batch_past_the_notification_budget_ends_the_connection_counted_whole() {
    let (link, handler, client) = connect(
        options().with_max_pending_notifications(2),
        WireOptions::new().with_batch_decoding(),
    );
    link.push_line(json!([notification("one"), notification("two")]).to_string());
    let handled = handler.wait_for_notifications(2).await;
    assert_eq!(
        seen(&handled),
        vec!["one", "two"],
        "expected a batch that fits handled whole: [\"one\", \"two\"] | received {handled:?}"
    );

    link.push_line(
        json!([
            notification("three"),
            notification("four"),
            notification("five"),
            notification("six")
        ])
        .to_string(),
    );
    let termination = one_termination(&handler).await;
    let handled = handler.notifications.lock().await.len();
    assert!(
        matches!(
            termination,
            PeerTermination::NotificationBackpressure {
                limit: 2,
                received: 4
            }
        ) && handled == 2,
        "expected the batch refused whole and counted whole: (NotificationBackpressure {{ limit: 2, received: 4 }}, 2 handled) | received ({termination:?}, {handled})"
    );
    client.close().await.expect("expected a clean close");
}

/// A batch's bytes count against the byte budget once, however many members it has, and all
/// of them are back when its members have been handled. A batch longer than the budget ends
/// the connection with its own length reported.
#[tokio::test]
async fn a_batch_takes_its_bytes_once_and_gives_them_back() {
    let batch = json!([
        notification("one"),
        notification("two"),
        notification("three")
    ])
    .to_string();
    let mut exact = options();
    exact.max_pending_bytes = batch.len();
    let (link, handler, client) = connect(exact, WireOptions::new().with_batch_decoding());
    link.push_line(batch.clone());
    let handled = handler.wait_for_notifications(3).await;
    until("the batch's bytes to be given back", || {
        client.state.peer_bytes.available_permits() == batch.len()
    })
    .await;
    assert_eq!(
        seen(&handled),
        vec!["one", "two", "three"],
        "expected a batch exactly as long as the budget admitted, in order: [\"one\", \"two\", \"three\"] | received {handled:?}"
    );

    let longer = json!([
        notification("one"),
        notification("two"),
        notification("three!")
    ])
    .to_string();
    link.push_line(longer.clone());
    let termination = one_termination(&handler).await;
    assert!(
        matches!(
            termination,
            PeerTermination::NotificationByteBackpressure { limit, received }
                if limit == batch.len() && received == longer.len()
        ),
        "expected the longer batch refused for its bytes: NotificationByteBackpressure {{ limit: {}, received: {} }} | received {termination:?}",
        batch.len(),
        longer.len()
    );
    client.close().await.expect("expected a clean close");
}

/// A line that is not JSON gets no answer from a client that was not asked to give one.
#[tokio::test]
async fn a_line_that_is_not_json_is_dropped_by_a_client_that_was_not_asked_to_answer() {
    let (link, handler, client) = connect(options(), WireOptions::new());
    link.push_line("{oops");
    link.push_line(notification("after").to_string());
    handler.wait_for_notifications(1).await;
    // A frame of this side's own, written behind anything the line could have set off.
    let after = client
        .submit_notification("after", json!({}))
        .expect("expected the notification queued");
    within("the notification written", after)
        .await
        .expect("expected the notification written");

    let written = methods(&link);
    assert_eq!(
        written,
        vec!["after"],
        "expected nothing written for a line that is not JSON: [\"after\"] | received {written:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// Asked to, a client answers a line that is not JSON with a parse error for no request. A
/// blank line is not answered, and neither is a line that is JSON and not a frame.
#[tokio::test]
async fn a_line_that_is_not_json_is_answered_with_a_parse_error() {
    for versioned in [true, false] {
        let mut speaking = options();
        speaking.include_version_header = versioned;
        let (link, handler, client) =
            connect(speaking, WireOptions::new().with_parse_error_replies(4));
        link.push_line("");
        link.push_line("   ");
        link.push_line("{oops");
        link.push_line("42");
        link.push_line(notification("after").to_string());

        handler.wait_for_notifications(1).await;
        sent(&link, 1).await;
        let written = frames(&link);
        let mut expected = json!({
            "id": null,
            "error": { "code": -32700, "message": "Parse error" },
        });
        if versioned {
            expected["jsonrpc"] = json!("2.0");
        }
        assert_eq!(
            written,
            vec![expected.clone()],
            "expected one parse error, for the line that was not JSON: [{expected}] | received {written:?}"
        );
        client.close().await.expect("expected a clean close");
    }
}

/// The answers are bounded by how many of them wait to be written. With the link not taking
/// writes, lines past the bound are dropped; once the waiting ones are written, a new one is
/// answered again.
#[tokio::test]
async fn parse_error_replies_are_bounded_by_how_many_wait_to_be_written() {
    let gated = gated(options(), WireOptions::new().with_parse_error_replies(2));
    let _held = link_held(&gated).await;
    for _ in 0..5 {
        gated.link.push_line("{oops");
    }
    gated.link.push_line(notification("after").to_string());
    gated.handler.wait_for_notifications(1).await;
    let waiting = gated
        .client
        .state
        .parse_error_replies
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len();
    assert_eq!(
        waiting, 2,
        "expected two parse errors waiting to be written, the other three dropped: 2 | received {waiting}"
    );

    gated.gate.add_permits(1);
    sent(&gated.link, 3).await;
    gated.link.push_line("{oops");
    sent(&gated.link, 4).await;
    let written = methods(&gated.link);
    assert_eq!(
        written,
        vec!["hold", "?", "?", "?"],
        "expected a line that is not JSON answered again once the earlier answers were written | received {written:?}"
    );
    gated.client.close().await.expect("expected a clean close");
}

/// The answers are held beside the bounds of the outbound queue. With the queue one frame
/// short of full, the peer's noise takes none of the room that is left: the host's next frame
/// is queued, where it would have ended the connection had an answer taken its place.
#[tokio::test]
async fn parse_errors_take_none_of_the_room_the_queue_leaves_the_host() {
    let gated = gated(
        options(),
        limits(usize::MAX, usize::MAX, 2).with_parse_error_replies(4),
    );
    let _held = link_held(&gated).await;
    gated.link.push_line("{oops");
    gated.link.push_line("{oops");
    gated.link.push_line(notification("after").to_string());
    gated.handler.wait_for_notifications(1).await;

    let mine = gated.client.submit_notification("mine", json!({}));
    let observed = (
        mine.is_ok(),
        outbox_holds(&gated.client).0,
        gated.client.is_closed(),
    );
    assert_eq!(
        observed,
        (true, 2, false),
        "expected the host's frame queued in the room that was left: (true, 2, false) | received {observed:?}"
    );
    assert_ended(&gated.client, None);
    gated.gate.add_permits(1);
    sent(&gated.link, 4).await;
    let written = methods(&gated.link);
    assert_eq!(
        written,
        vec!["hold", "?", "?", "mine"],
        "expected both answers and the host's frame written in the order they were queued | received {written:?}"
    );
    gated.client.close().await.expect("expected a clean close");
}

/// A request given up is not announced by a client that was not asked to announce it.
#[tokio::test]
async fn a_request_given_up_is_not_announced_by_a_client_that_was_not_asked_to() {
    let (link, _handler, client) = connect(options(), WireOptions::new());
    let (written, reply) = client
        .submit_request("ask", json!({}), RequestOptions::new())
        .expect("expected the request queued")
        .into_parts();
    within("the request written", written)
        .await
        .expect("expected the request written");
    drop(reply);
    let after = client
        .submit_notification("after", json!({}))
        .expect("expected the notification queued");
    within("the notification written", after)
        .await
        .expect("expected the notification written");

    let written = methods(&link);
    assert_eq!(
        written,
        vec!["ask", "after"],
        "expected nothing written for the request given up | received {written:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// Asked to, a client tells the peer when a request it has written is given up: a notification
/// naming the request by the id it was sent with, and no id of its own. Whether the reply is
/// dropped or the whole submission, and a response that comes afterwards finds nobody waiting.
#[tokio::test]
async fn a_request_given_up_after_it_was_written_sends_the_cancel() {
    for whole in [false, true] {
        let (link, handler, client) =
            connect(options(), WireOptions::new().with_cancel_on_abandon(CANCEL));
        let submitted = client
            .submit_request("ask", json!({}), RequestOptions::new())
            .expect("expected the request queued");
        if whole {
            sent(&link, 1).await;
            until("the write to be over", || outbox_holds(&client) == (0, 0)).await;
            drop(submitted);
        } else {
            let (written, reply) = submitted.into_parts();
            within("the request written", written)
                .await
                .expect("expected the request written");
            drop(reply);
        }

        sent(&link, 2).await;
        let cancel = frames(&link).pop().expect("expected a second frame");
        let expected = json!({
            "jsonrpc": "2.0",
            "method": CANCEL,
            "params": { "requestId": "1" },
        });
        assert_eq!(
            cancel, expected,
            "expected the cancel for the request given up (whole: {whole}): {expected} | received {cancel}"
        );
        // The peer answers all the same. Nobody waits, and nothing comes of it.
        link.push_line(r#"{"jsonrpc":"2.0","id":"1","result":"late"}"#);
        link.push_line(notification("after").to_string());
        handler.wait_for_notifications(1).await;
        let observed = (link.sent().len(), client.is_closed());
        assert_eq!(
            observed,
            (2, false),
            "expected the late response dropped and the connection open: (2, false) | received {observed:?}"
        );
        client.close().await.expect("expected a clean close");
    }
}

/// A request given up while it is still queued is taken out of the queue. The peer never saw
/// it, so there is nothing to cancel and nothing is sent.
#[tokio::test]
async fn a_request_given_up_before_its_write_began_sends_no_cancel() {
    let gated = gated(options(), WireOptions::new().with_cancel_on_abandon(CANCEL));
    let _held = link_held(&gated).await;
    let queued = gated
        .client
        .submit_request("ask", json!({}), RequestOptions::new())
        .expect("expected the request queued");
    drop(queued);
    gated.gate.add_permits(1);
    let after = gated
        .client
        .submit_notification("after", json!({}))
        .expect("expected the notification queued");
    within("the notification written", after)
        .await
        .expect("expected the notification written");

    let written = methods(&gated.link);
    assert_eq!(
        written,
        vec!["hold", "after"],
        "expected neither the request nor a cancel for it on the wire | received {written:?}"
    );
    gated.client.close().await.expect("expected a clean close");
}

/// A response that has been read disarms the cancel, whether or not its caller has looked at
/// it: the reader took the call off the record, so giving the reply up finds nothing to cancel.
#[tokio::test]
async fn a_response_already_read_disarms_the_cancel() {
    for request in [RequestOptions::new(), ordered_options()] {
        let (link, _handler, client) =
            connect(options(), WireOptions::new().with_cancel_on_abandon(CANCEL));
        let (written, reply) = client
            .submit_request("ask", json!({}), request)
            .expect("expected the request queued")
            .into_parts();
        within("the request written", written)
            .await
            .expect("expected the request written");
        link.push_line(r#"{"jsonrpc":"2.0","id":"1","result":"done"}"#);
        until("the response to be read", || {
            client
                .state
                .pending
                .try_lock()
                .is_ok_and(|pending| pending.is_empty())
        })
        .await;
        drop(reply);
        let after = client
            .submit_notification("after", json!({}))
            .expect("expected the notification queued");
        within("the notification written", after)
            .await
            .expect("expected the notification written");

        let written = methods(&link);
        assert_eq!(
            written,
            vec!["ask", "after"],
            "expected no cancel for a request that was answered | received {written:?}"
        );
        client.close().await.expect("expected a clean close");
    }
}

/// A call whose deadline runs out is given up like any other: the caller gets its timeout and
/// the peer is told. This is the path of a caller that writes for itself.
#[tokio::test(start_paused = true)]
async fn a_request_that_times_out_sends_the_cancel() {
    let (link, _handler, client) = connect(
        options().with_request_timeout(Duration::from_millis(50)),
        WireOptions::new().with_cancel_on_abandon(CANCEL),
    );
    let outcome = client.request::<_, Value>("ask", json!({})).await;
    assert!(
        matches!(outcome, Err(Error::Timeout { .. })),
        "expected the request to time out: Err(Timeout) | received {outcome:?}"
    );

    sent(&link, 2).await;
    let written = methods(&link);
    assert_eq!(
        written,
        vec!["ask", CANCEL],
        "expected the cancel behind the request that timed out | received {written:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// A reply dropped while something else holds the record of calls decides the cancel later, on
/// the connection's runtime, and still sends it.
#[tokio::test]
async fn a_cancel_decided_after_waiting_for_the_record_of_calls_is_still_sent() {
    let (link, _handler, client) =
        connect(options(), WireOptions::new().with_cancel_on_abandon(CANCEL));
    let (written, reply) = client
        .submit_request("ask", json!({}), RequestOptions::new())
        .expect("expected the request queued")
        .into_parts();
    within("the request written", written)
        .await
        .expect("expected the request written");
    let in_the_way = client.state.pending.lock().await;
    drop(reply);
    let early = link.sent().len();
    assert_eq!(
        early, 1,
        "expected no cancel while the record of calls is held: 1 frame | received {early}"
    );
    drop(in_the_way);

    sent(&link, 2).await;
    let written = methods(&link);
    assert_eq!(
        written,
        vec!["ask", CANCEL],
        "expected the cancel once the record of calls was free | received {written:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// Once the connection has ended there is nobody to tell: a reply dropped after a close sends
/// nothing. The drop decides it on the spot, so there is nothing to wait for.
#[tokio::test]
async fn a_reply_dropped_after_the_connection_ended_sends_no_cancel() {
    let (link, _handler, client) =
        connect(options(), WireOptions::new().with_cancel_on_abandon(CANCEL));
    let (written, reply) = client
        .submit_request("ask", json!({}), RequestOptions::new())
        .expect("expected the request queued")
        .into_parts();
    within("the request written", written)
        .await
        .expect("expected the request written");
    client.close().await.expect("expected a clean close");
    drop(reply);

    let observed = (methods(&link), outbox_holds(&client).0);
    assert_eq!(
        observed,
        (vec![String::from("ask")], 0),
        "expected nothing written or queued after the close: ([\"ask\"], 0) | received {observed:?}"
    );
}

/// Cancels are held beside the bounds of the outbound queue. With the queue full, a caller
/// letting go of a request does not end the connection, and the cancel is written all the same.
#[tokio::test]
async fn a_cancel_is_queued_beside_a_full_queue_and_the_connection_stays() {
    let gated = gated(
        options(),
        limits(usize::MAX, usize::MAX, 1).with_cancel_on_abandon(CANCEL),
    );
    let (written, reply) = gated
        .client
        .submit_request("ask", json!({}), RequestOptions::new())
        .expect("expected the request queued")
        .into_parts();
    within("the request written", written)
        .await
        .expect("expected the request written");
    let _held = link_held(&gated).await;
    drop(reply);

    let observed = (outbox_holds(&gated.client).0, gated.client.is_closed());
    assert_eq!(
        observed,
        (1, false),
        "expected only the held frame counted and the connection open: (1, false) | received {observed:?}"
    );
    assert_ended(&gated.client, None);
    gated.gate.add_permits(1);
    sent(&gated.link, 3).await;
    let written = methods(&gated.link);
    assert_eq!(
        written,
        vec!["ask", "hold", CANCEL],
        "expected the cancel written once the link took writes again | received {written:?}"
    );
    gated.client.close().await.expect("expected a clean close");
}

/// A reply can be dropped by a thread that is not on any runtime. The cancel is queued from
/// there all the same, and nothing panics.
#[test]
fn a_reply_dropped_outside_any_runtime_still_sends_the_cancel() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_time()
        .build()
        .expect("expected a runtime to build");
    let (link, client, reply) = runtime.block_on(async {
        let (link, _handler, client) =
            connect(options(), WireOptions::new().with_cancel_on_abandon(CANCEL));
        let (written, reply) = client
            .submit_request("ask", json!({}), RequestOptions::new())
            .expect("expected the request queued")
            .into_parts();
        within("the request written", written)
            .await
            .expect("expected the request written");
        (link, client, reply)
    });
    drop(reply);
    let written = runtime.block_on(async {
        sent(&link, 2).await;
        let written = methods(&link);
        client.close().await.expect("expected a clean close");
        written
    });
    assert_eq!(
        written,
        vec!["ask", CANCEL],
        "expected the cancel queued from outside the runtime | received {written:?}"
    );
}

/// A caller that writes for itself is on record before it has the link. Given up while it
/// still waits its turn, it has written nothing, so there is nothing to cancel.
#[tokio::test]
async fn a_request_given_up_while_it_waits_for_the_link_sends_no_cancel() {
    let gated = gated(options(), WireOptions::new().with_cancel_on_abandon(CANCEL));
    let _held = link_held(&gated).await;
    let waiting = tokio::spawn({
        let client = Arc::clone(&gated.client);
        async move { client.request::<_, Value>("ask", json!({})).await }
    });
    until("the request to be on record", || {
        gated
            .client
            .state
            .pending
            .try_lock()
            .is_ok_and(|pending| pending.len() == 1)
    })
    .await;
    waiting.abort();
    let _ = waiting.await;
    until("the request to be off the record", || {
        gated
            .client
            .state
            .pending
            .try_lock()
            .is_ok_and(|pending| pending.is_empty())
    })
    .await;
    gated.gate.add_permits(1);
    let after = gated
        .client
        .submit_notification("after", json!({}))
        .expect("expected the notification queued");
    within("the notification written", after)
        .await
        .expect("expected the notification written");

    let written = methods(&gated.link);
    assert_eq!(
        written,
        vec!["hold", "after"],
        "expected neither the request nor a cancel for it on the wire | received {written:?}"
    );
    gated.client.close().await.expect("expected a clean close");
}

/// A batch of responses alone takes nothing of the byte budget, as a response on a line of
/// its own takes nothing. One for a call nobody waits for is dropped.
#[tokio::test]
async fn a_batch_of_responses_alone_takes_none_of_the_byte_budget() {
    let mut narrow = options();
    narrow.max_pending_bytes = 8;
    let (link, _handler, client) = connect(narrow, WireOptions::new().with_batch_decoding());
    let (written, reply) = client
        .submit_request("ask", json!({}), RequestOptions::new())
        .expect("expected the request queued")
        .into_parts();
    within("the request written", written)
        .await
        .expect("expected the request written");
    link.push_line(
        json!([
            { "jsonrpc": "2.0", "id": "nobody", "result": "stray" },
            { "jsonrpc": "2.0", "id": "1", "result": "done" },
        ])
        .to_string(),
    );

    let answer = within("the reply", reply).await;
    let observed = (answer.as_ref().ok(), client.is_closed());
    assert_eq!(
        observed,
        (Some(&json!("done")), false),
        "expected the answer delivered and the connection open: (Some(\"done\"), false) | received ({answer:?}, {})",
        client.is_closed()
    );
    client.close().await.expect("expected a clean close");
}

/// A batch with a response and notifications is charged its whole line, shared among the
/// notifications in shares that need not be equal, and gives every byte back.
#[tokio::test]
async fn a_mixed_batch_gives_back_every_byte_it_took() {
    let batch = json!([
        { "jsonrpc": "2.0", "id": "nobody", "result": "a response of some length." },
        notification("one"),
        notification("two"),
        notification("three"),
    ])
    .to_string();
    assert!(
        !batch.len().is_multiple_of(3),
        "expected a line whose bytes do not share out evenly among three: remainder 1 or 2 | received {}",
        batch.len() % 3
    );
    let mut exact = options();
    exact.max_pending_bytes = batch.len();
    let (link, handler, client) = connect(exact, WireOptions::new().with_batch_decoding());
    link.push_line(batch.clone());
    let handled = handler.wait_for_notifications(3).await;
    until("the batch's bytes to be given back", || {
        client.state.peer_bytes.available_permits() == batch.len()
    })
    .await;
    assert_eq!(
        seen(&handled),
        vec!["one", "two", "three"],
        "expected the notifications handled in order: [\"one\", \"two\", \"three\"] | received {handled:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// What makes a batch member something for the handler: an object with a method that is a
/// string. Anything else is a response, or nothing.
#[test]
fn a_batch_member_is_work_for_the_handler_only_with_a_string_method() {
    for (member, expected) in [
        (json!({ "method": "session/update" }), true),
        (json!({ "id": 1, "method": "ask" }), true),
        (json!({ "id": 1, "result": {} }), false),
        (json!({ "method": 5 }), false),
        (json!("method"), false),
        (json!(["method"]), false),
    ] {
        let work = is_peer_work(&member);
        assert_eq!(
            work, expected,
            "expected {member} to be work for the handler: {expected} | received {work}"
        );
    }
}
