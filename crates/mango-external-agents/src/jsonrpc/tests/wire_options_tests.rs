//! What a client does on the wire only when asked to: reading batches, answering lines that
//! are not JSON, and telling the peer about a request it gave up.

use super::outbox_tests::{
    assert_ended, gated, limits, link_held, methods, one_termination, options, ordered_options,
    outbox_holds, sent, until, within,
};
use super::write_failure_tests::RefuseFirstSend;
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
/// an array inside the array included, and so is an empty batch.
#[tokio::test]
async fn a_batch_reaches_the_handler_member_by_member_in_order() {
    let (link, handler, client) = connect(options(), WireOptions::new().with_batch_decoding());
    link.push_line("[]");
    link.push_line(
        json!([
            notification("one"),
            { "jsonrpc": "2.0", "id": 7, "method": "ask", "params": {} },
            5,
            [notification("nested")],
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
        // A frame of this side's own, written behind every answer the lines set off.
        let after = client
            .submit_notification("after", json!({}))
            .expect("expected the notification queued");
        within("the notification written", after)
            .await
            .expect("expected the notification written");
        let mut written = frames(&link);
        let last = written.pop().expect("expected this side's own frame");
        assert_eq!(
            last["method"], "after",
            "expected this side's own frame written last: \"after\" | received {last}"
        );
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
        "expected a line that is not JSON answered again once the earlier answers were written: [\"hold\", \"?\", \"?\", \"?\"] | received {written:?}"
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
        "expected both answers and the host's frame written in the order they were queued: [\"hold\", \"?\", \"?\", \"mine\"] | received {written:?}"
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

/// Where in the reader's handling of a response a test has the caller give the call up.
#[derive(Clone, Copy, Debug, PartialEq)]
enum GivenUp {
    /// The call is off the record and the reader has not yet claimed it.
    BeforeTheReaderClaims,
    /// The reader has claimed it and not yet handed the response on.
    AfterTheReaderClaimed,
}

/// Writes one request, has its reply dropped at `when` inside the reader's handling of the
/// response, and returns what was written and whether the reader went on to hand the response
/// on after the point where it takes the claim.
async fn give_up_as_the_response_is_read(
    when: GivenUp,
    request: RequestOptions,
) -> (Vec<String>, bool, usize) {
    let (link, _handler, client) =
        connect(options(), WireOptions::new().with_cancel_on_abandon(CANCEL));
    let (written, reply) = client
        .submit_request("ask", json!({}), request)
        .expect("expected the request queued")
        .into_parts();
    within("the request written", written)
        .await
        .expect("expected the request written");
    let reply = Arc::new(std::sync::Mutex::new(Some(reply)));
    let handed_on = Arc::new(AtomicBool::new(false));
    let give_up = {
        let reply = Arc::clone(&reply);
        move || {
            let reply = reply
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            drop(reply);
        }
    };
    let state = &client.state;
    match when {
        GivenUp::BeforeTheReaderClaims => {
            *state
                .after_response_matched
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(give_up));
            let handed_on = Arc::clone(&handed_on);
            *state
                .after_response_claimed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(Box::new(move || handed_on.store(true, Ordering::Release)));
        }
        GivenUp::AfterTheReaderClaimed => {
            let handed_on = Arc::clone(&handed_on);
            *state
                .after_response_claimed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(move || {
                handed_on.store(true, Ordering::Release);
                give_up();
            }));
        }
    }
    link.push_line(r#"{"jsonrpc":"2.0","id":"1","result":"done"}"#);
    until("the reply to be given up", || {
        reply
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none()
    })
    .await;
    within("the next write", client.notify("after", json!({})))
        .await
        .expect("expected the notification written");
    until("the response to be out of the handoff queue", || {
        client.state.queued_answers.load(Ordering::Acquire) == 0
    })
    .await;
    let places = client.state.ordered_places.available_permits();
    let observed = (methods(&link), handed_on.load(Ordering::Acquire), places);
    client.close().await.expect("expected a clean close");
    observed
}

/// A caller that gives a call up as its response is being read, before the reader has claimed
/// the call, has the last word: the cancel goes out, once, and the reader drops the response
/// as a late one instead of handing it on. Whichever way the response was to be delivered.
#[tokio::test]
async fn a_call_given_up_before_the_reader_claims_it_is_cancelled_and_its_response_dropped() {
    for request in [RequestOptions::new(), ordered_options()] {
        let observed =
            give_up_as_the_response_is_read(GivenUp::BeforeTheReaderClaims, request).await;
        let expected = (
            vec![
                String::from("ask"),
                String::from(CANCEL),
                String::from("after"),
            ],
            false,
            64,
        );
        assert_eq!(
            observed, expected,
            "expected one cancel, the response not handed on, every ordered place back: {expected:?} | received {observed:?}"
        );
    }
}

/// A response the reader has claimed disarms the cancel, whether or not its caller has looked
/// at it: giving the reply up from then on tells the peer nothing.
#[tokio::test]
async fn a_call_given_up_after_the_reader_claimed_it_sends_no_cancel() {
    for request in [RequestOptions::new(), ordered_options()] {
        let observed =
            give_up_as_the_response_is_read(GivenUp::AfterTheReaderClaimed, request).await;
        let expected = (vec![String::from("ask"), String::from("after")], true, 64);
        assert_eq!(
            observed, expected,
            "expected no cancel for a call that was answered, every ordered place back: {expected:?} | received {observed:?}"
        );
    }
}

/// A response that was delivered disarms the cancel too: a caller that has its answer and lets
/// the call go tells the peer nothing.
#[tokio::test]
async fn a_call_that_was_answered_sends_no_cancel() {
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
        let answer = within("the answer", reply).await;
        within("the next write", client.notify("after", json!({})))
            .await
            .expect("expected the notification written");

        let observed = (answer.ok(), methods(&link));
        let expected = (
            Some(json!("done")),
            vec![String::from("ask"), String::from("after")],
        );
        assert_eq!(
            observed, expected,
            "expected the answer and no cancel behind it: {expected:?} | received {observed:?}"
        );
        client.close().await.expect("expected a clean close");
    }
}

/// A request the link refused whole never reached the peer: its frame's write began and the
/// link took none of it. The caller hears that, lets the call go, and nothing is cancelled.
#[tokio::test]
async fn a_request_the_link_refused_whole_sends_no_cancel() {
    let link = ScriptedLink::new();
    let (sender, receiver) = link.clone().into_link().split();
    let handler = RecordingHandler::arc(None);
    let client = Client::connect_with(
        crate::link::Link::new(
            Box::new(RefuseFirstSend {
                inner: sender,
                first: true,
            }),
            receiver,
        ),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options(),
        WireOptions::new().with_cancel_on_abandon(CANCEL),
    );
    let (written, reply) = client
        .submit_request("ask", json!({}), RequestOptions::new())
        .expect("expected the request queued")
        .into_parts();
    let refused = within("the write refused", written).await;
    assert!(
        matches!(&refused, Err(error) if matches!(error.cause(), Error::LimitExceeded { .. })),
        "expected the link's refusal of the frame: Err(LimitExceeded) | received {refused:?}"
    );
    let answer = within("the reply", reply).await;
    assert!(
        answer.is_err(),
        "expected the call failed with its frame unwritten: Err | received {answer:?}"
    );
    within("the next write", client.notify("after", json!({})))
        .await
        .expect("expected the notification written");

    let observed = (methods(&link), client.state.outbox.aside());
    assert_eq!(
        observed,
        (vec![String::from("after")], 0),
        "expected no cancel for a request the peer never got: ([\"after\"], 0) | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// The same for a caller that lets go before it has heard: once the link has refused the
/// frame the call is settled, even while the writer still waits for the record of calls to
/// say so.
#[tokio::test]
async fn a_request_refused_whole_and_given_up_before_its_caller_heard_sends_no_cancel() {
    let link = ScriptedLink::new();
    let (sender, receiver) = link.clone().into_link().split();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let handler = RecordingHandler::arc(None);
    let client = Client::connect_with(
        crate::link::Link::new(
            Box::new(GatedSender {
                inner: Box::new(RefuseFirstSend {
                    inner: sender,
                    first: true,
                }),
                method: "ask",
                gate: Arc::clone(&gate),
            }),
            receiver,
        ),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options(),
        WireOptions::new().with_cancel_on_abandon(CANCEL),
    );
    let (written, reply) = client
        .submit_request("ask", json!({}), RequestOptions::new())
        .expect("expected the request queued")
        .into_parts();
    until("the write to begin", || written.started()).await;
    let in_the_way = client.state.pending.lock().await;
    gate.add_permits(1);
    until("the refused frame to be let go", || {
        outbox_holds(&client) == (0, 0)
    })
    .await;
    drop(reply);
    let queued = client.state.outbox.aside();
    assert_eq!(
        queued, 0,
        "expected no cancel queued for a frame the link had refused, its caller not yet told: 0 | received {queued}"
    );
    drop(in_the_way);
    let refused = within("the write refused", written).await;
    within("the next write", client.notify("after", json!({})))
        .await
        .expect("expected the notification written");

    let observed = (refused.is_err(), methods(&link));
    assert_eq!(
        observed,
        (true, vec![String::from("after")]),
        "expected the refusal reported and nothing but the next write on the wire: (true, [\"after\"]) | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
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

/// The cancel is decided and queued as the reply is dropped, whatever the record of calls is
/// doing at that moment. With the record held by someone else, the cancel is in the queue
/// before the drop returns, and a write this caller makes next goes out behind it.
///
/// The record of calls is no longer on the path of the decision. Holding it here is what shows
/// that: a drop that went back to waiting for it would leave nothing queued when it returned.
#[tokio::test]
async fn a_cancel_does_not_wait_for_the_record_of_calls() {
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
    let queued = client.state.outbox.aside();
    assert_eq!(
        queued, 1,
        "expected the cancel queued before the drop returned, the record still held: 1 | received {queued}"
    );
    within("the next write", client.notify("second", json!({})))
        .await
        .expect("expected the notification written");
    drop(in_the_way);

    let written = methods(&link);
    assert_eq!(
        written,
        vec!["ask", CANCEL, "second"],
        "expected the cancel ahead of the caller's next write: [\"ask\", \"$/cancel_request\", \"second\"] | received {written:?}"
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

    let observed = (methods(&link), client.state.outbox.aside());
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

    let observed = (
        outbox_holds(&gated.client).0,
        gated.client.state.outbox.aside(),
        gated.client.is_closed(),
    );
    assert_eq!(
        observed,
        (1, 1, false),
        "expected the held frame counted, the cancel beside it, the connection open: (1, 1, false) | received {observed:?}"
    );
    assert_ended(&gated.client, None);
    gated.gate.add_permits(1);
    sent(&gated.link, 3).await;
    let written = methods(&gated.link);
    assert_eq!(
        written,
        vec!["ask", "hold", CANCEL],
        "expected the cancel written once the link took writes again: [\"ask\", \"hold\", \"$/cancel_request\"] | received {written:?}"
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

/// A cancel is in the queue from the moment the request is given up, though it takes none of
/// the queue's bounds. A caller that writes for itself straight afterwards waits its turn
/// behind it: the retry does not reach the peer before the cancel of what it replaces.
#[tokio::test(start_paused = true)]
async fn a_write_made_straight_after_a_request_is_given_up_follows_its_cancel() {
    let (link, _handler, client) = connect(
        options().with_request_timeout(Duration::from_millis(50)),
        WireOptions::new().with_cancel_on_abandon(CANCEL),
    );
    let outcome = client.request::<_, Value>("ask", json!({})).await;
    assert!(
        matches!(outcome, Err(Error::Timeout { .. })),
        "expected the request to time out: Err(Timeout) | received {outcome:?}"
    );
    // Nothing has been awaited since the cancel was queued: the writer has not run.
    client
        .notify("second", json!({}))
        .await
        .expect("expected the notification written");

    let written = methods(&link);
    assert_eq!(
        written,
        vec!["ask", CANCEL, "second"],
        "expected the cancel ahead of the next write: [\"ask\", \"$/cancel_request\", \"second\"] | received {written:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// A handler that stops inside each notification until it is let through, and says which it
/// has begun and which it has finished.
struct GatedHandler {
    begun: std::sync::Mutex<Vec<String>>,
    finished: std::sync::Mutex<Vec<String>>,
    gate: tokio::sync::Semaphore,
    /// What had been handled to the end each time a question arrived.
    finished_when_asked: std::sync::Mutex<Vec<Vec<String>>>,
    terminations: std::sync::Mutex<Vec<PeerTermination>>,
}

impl GatedHandler {
    fn connect(
        options: ClientOptions,
        wire: WireOptions,
    ) -> (ScriptedLink, Arc<Self>, Arc<Client>) {
        let link = ScriptedLink::new();
        let handler = Arc::new(Self {
            begun: std::sync::Mutex::new(Vec::new()),
            finished: std::sync::Mutex::new(Vec::new()),
            gate: tokio::sync::Semaphore::new(0),
            finished_when_asked: std::sync::Mutex::new(Vec::new()),
            terminations: std::sync::Mutex::new(Vec::new()),
        });
        let client = Arc::new(Client::connect_with(
            link.clone().into_link(),
            Arc::clone(&handler) as Arc<dyn PeerHandler>,
            options,
            wire,
        ));
        (link, handler, client)
    }

    fn list(of: &std::sync::Mutex<Vec<String>>) -> Vec<String> {
        of.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn terminations(&self) -> Vec<PeerTermination> {
        self.terminations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait::async_trait]
impl PeerHandler for GatedHandler {
    async fn on_notification(&self, method: String, _params: Value) {
        self.begun
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(method.clone());
        if let Ok(permit) = self.gate.acquire().await {
            permit.forget();
        }
        self.finished
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(method);
    }

    async fn on_request(
        &self,
        _method: String,
        _params: Value,
        _id: RequestId,
    ) -> ServerRequestOutcome {
        let finished = Self::list(&self.finished);
        self.finished_when_asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(finished);
        ServerRequestOutcome::Answer(json!("yes"))
    }

    async fn on_terminated(&self, termination: PeerTermination) {
        self.terminations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(termination);
    }
}

/// An ordered response inside a batch reaches its caller once the handler has returned from
/// the notifications before it in the batch, and does not wait for the ones after it.
#[tokio::test]
async fn an_ordered_response_in_a_batch_waits_for_the_members_before_it_only() {
    let (link, handler, client) =
        GatedHandler::connect(options(), WireOptions::new().with_batch_decoding());
    let (written, reply) = client
        .submit_request("ask", json!({}), ordered_options())
        .expect("expected the request queued")
        .into_parts();
    within("the request written", written)
        .await
        .expect("expected the request written");
    link.push_line(
        json!([
            notification("before"),
            { "jsonrpc": "2.0", "id": "1", "result": "done" },
            notification("after"),
        ])
        .to_string(),
    );
    until("the handler to be inside the first member", || {
        GatedHandler::list(&handler.begun) == ["before"]
    })
    .await;
    let mut reply = Box::pin(reply);
    let early =
        std::future::poll_fn(|cx| std::task::Poll::Ready(reply.as_mut().poll(cx).is_ready())).await;
    assert!(
        !early,
        "expected the answer held back while the member before it is handled: pending | received an outcome"
    );

    handler.gate.add_permits(1);
    let answer = within("the reply", reply).await;
    let observed = (
        answer.as_ref().ok().cloned(),
        GatedHandler::list(&handler.finished),
    );
    assert_eq!(
        observed,
        (Some(json!("done")), vec![String::from("before")]),
        "expected the answer once the member before it was handled, the one after still in progress: (Some(\"done\"), [\"before\"]) | received {observed:?}"
    );
    handler.gate.add_permits(1);
    client.close().await.expect("expected a clean close");
}

/// A batch is counted on top of what the queue already holds. With one notification queued
/// behind a handler that is busy, a batch of two does not fit a budget of two, and the
/// termination reports the three.
#[tokio::test]
async fn a_batch_that_overflows_a_queue_already_in_use_reports_what_was_queued_too() {
    let (link, handler, client) = GatedHandler::connect(
        options().with_max_pending_notifications(2),
        WireOptions::new().with_batch_decoding(),
    );
    link.push_line(notification("busy").to_string());
    until("the handler to be inside a call", || {
        GatedHandler::list(&handler.begun) == ["busy"]
    })
    .await;
    link.push_line(notification("queued").to_string());
    link.push_line(json!([notification("one"), notification("two")]).to_string());

    until("the overflow to be told", || {
        handler.terminations().len() == 1
    })
    .await;
    let observed = (handler.terminations(), GatedHandler::list(&handler.begun));
    assert!(
        matches!(
            &observed,
            (terminations, begun)
                if matches!(
                    terminations[..],
                    [PeerTermination::NotificationBackpressure { limit: 2, received: 3 }]
                ) && begun == &["busy"]
        ),
        "expected the batch refused whole with the queued one counted: ([NotificationBackpressure {{ limit: 2, received: 3 }}], [\"busy\"]) | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// A batch admitted whole is not counted again member by member. The count of what is queued
/// can read one low for a moment, while the handler's task has taken an answer out of the
/// queue and not yet let it go; a batch admitted on that reading must not be refused half-way
/// when the reading corrects itself. That moment is put in place by hand here: the count of
/// queued answers is raised as the taken-out answer raises it, and lowered again by a hook
/// that only a second reading of the queue would run.
#[tokio::test]
async fn a_batch_admitted_whole_is_not_refused_half_way_by_a_second_count() {
    let (link, handler, client) = GatedHandler::connect(
        options().with_max_pending_notifications(2),
        WireOptions::new().with_batch_decoding(),
    );
    link.push_line(notification("busy").to_string());
    until("the handler to be inside a call", || {
        GatedHandler::list(&handler.begun) == ["busy"]
    })
    .await;
    link.push_line(notification("queued").to_string());
    until("the second notification to be queued", || {
        client.state.peer_bytes.available_permits()
            <= client.state.options.max_pending_bytes - 2 * notification("busy").to_string().len()
    })
    .await;

    // An answer taken out of the queue and not yet let go: still counted, no longer queued.
    client.state.queued_answers.fetch_add(1, Ordering::AcqRel);
    let state = Arc::clone(&client.state);
    *client
        .state
        .after_queue_length_read
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(move || {
        let again = Arc::clone(&state);
        // The first reading is the batch's own. A second one finds the answer let go.
        *state
            .after_queue_length_read
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(move || {
            again.queued_answers.fetch_sub(1, Ordering::AcqRel);
        }));
    }));
    link.push_line(json!([notification("one"), notification("two")]).to_string());
    handler.gate.add_permits(4);
    until("every notification to be handled", || {
        GatedHandler::list(&handler.finished).len() == 4 || !handler.terminations().is_empty()
    })
    .await;

    let observed = (
        GatedHandler::list(&handler.finished),
        handler.terminations().len(),
    );
    assert_eq!(
        observed,
        (
            ["busy", "queued", "one", "two"].map(String::from).to_vec(),
            0
        ),
        "expected the whole batch handled and the connection kept: ([\"busy\", \"queued\", \"one\", \"two\"], 0) | received {observed:?}"
    );
    // Put back what the hook did not get to take away.
    if client
        .state
        .after_queue_length_read
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .is_some()
    {
        client.state.queued_answers.fetch_sub(1, Ordering::AcqRel);
    }
    client.close().await.expect("expected a clean close");
}

/// The numbers the ACP harness pins for a flood: a batch of 1,100 notifications against a
/// budget of 1,024, read with nothing queued, is reported as 1,024 and 1,100.
#[tokio::test]
async fn a_flood_in_one_batch_reports_the_budget_and_the_size_of_the_batch() {
    let (link, handler, client) = connect(
        options().with_max_pending_notifications(1_024),
        WireOptions::new().with_batch_decoding(),
    );
    let flood: Vec<Value> = (0..1_100).map(|_| notification("session/update")).collect();
    link.push_line(Value::Array(flood).to_string());

    let termination = one_termination(&handler).await;
    let handled = handler.notifications.lock().await.len();
    assert!(
        matches!(
            termination,
            PeerTermination::NotificationBackpressure {
                limit: 1_024,
                received: 1_100
            }
        ) && handled == 0,
        "expected the flood refused whole: (NotificationBackpressure {{ limit: 1024, received: 1100 }}, 0 handled) | received ({termination:?}, {handled})"
    );
    client.close().await.expect("expected a clean close");
}

/// A frame queued beside the bounds is still ahead of what is written after it. With a parse
/// error waiting behind a write in progress, a notification the host writes for itself and an
/// answer to a question from the peer both go out behind it.
#[tokio::test]
async fn writes_made_after_a_parse_error_was_queued_follow_it() {
    let gated = gated(options(), WireOptions::new().with_parse_error_replies(4));
    let _held = link_held(&gated).await;
    gated.link.push_line("{oops");
    until("the parse error to be queued", || {
        gated.client.state.outbox.aside() == 1
    })
    .await;
    let mine = tokio::spawn({
        let client = Arc::clone(&gated.client);
        async move { client.notify("mine", json!({})).await }
    });
    gated
        .link
        .push_line(r#"{"jsonrpc":"2.0","id":9,"method":"ask","params":{}}"#);
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    let early = gated.link.sent().len();
    assert_eq!(
        early, 0,
        "expected nothing written while the link is held: 0 | received {early}"
    );

    gated.gate.add_permits(1);
    within("the host's notification", mine)
        .await
        .expect("expected the notifying task to finish")
        .expect("expected the notification written");
    sent(&gated.link, 4).await;
    let written = frames(&gated.link);
    let first_two: Vec<String> = written
        .iter()
        .take(2)
        .map(|frame| {
            frame["method"]
                .as_str()
                .map_or_else(|| frame["error"]["code"].to_string(), str::to_owned)
        })
        .collect();
    let rest_follow = written[2..]
        .iter()
        .all(|frame| frame["method"] == "mine" || frame["id"] == 9);
    assert!(
        first_two == ["hold", "-32700"] && rest_follow,
        "expected the parse error ahead of the host's notification and of the answer to the peer: [hold, -32700, then mine and the answer to 9] | received {written:?}"
    );
    gated.client.close().await.expect("expected a clean close");
}

/// The same on a link nobody holds, where a caller that writes for itself would otherwise go
/// straight to it: with only the parse error waiting, queued and not yet picked up, the host's
/// notification takes its turn behind it.
#[tokio::test]
async fn a_write_made_on_an_idle_link_follows_a_parse_error_still_queued() {
    let (link, _handler, client) =
        connect(options(), WireOptions::new().with_parse_error_replies(4));
    // Queued from here, so nothing has run between this and the write that follows.
    client.state.answer_unparseable();
    let waiting = (client.state.outbox.aside(), outbox_holds(&client));
    assert_eq!(
        waiting,
        (1, (0, 0)),
        "expected the parse error alone in the queue, beside its bounds: (1, (0, 0)) | received {waiting:?}"
    );
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
        vec!["-32700", "mine"],
        "expected the parse error ahead of the host's notification: [\"-32700\", \"mine\"] | received {written:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// Requests given up and retried from four threads, with the record of calls kept busy the
/// whole time. Every request is on the wire before it is given up, so every one has exactly
/// one cancel naming it, after it and ahead of the retry its caller made next.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_keeps_its_place_ahead_of_the_retry_with_the_record_of_calls_contended() {
    const CALLERS: usize = 4;
    const ROUNDS: usize = 50;
    let (link, _handler, client) =
        connect(options(), WireOptions::new().with_cancel_on_abandon(CANCEL));
    let busy = tokio::spawn({
        let client = Arc::clone(&client);
        async move {
            loop {
                let held = client.state.pending.lock().await;
                tokio::task::yield_now().await;
                drop(held);
            }
        }
    });
    let callers: Vec<_> = (0..CALLERS)
        .map(|caller| {
            let client = Arc::clone(&client);
            tokio::spawn(async move {
                let mut asked = Vec::new();
                for round in 0..ROUNDS {
                    let tag = json!({ "caller": caller, "round": round });
                    let submitted = client
                        .submit_request("ask", tag.clone(), RequestOptions::new())
                        .expect("expected the request queued");
                    asked.push((submitted.id().as_json().clone(), tag.clone()));
                    let (written, reply) = submitted.into_parts();
                    within("the request written", written)
                        .await
                        .expect("expected the request written");
                    // Nobody answers. Given up with the request on the wire.
                    drop(reply);
                    client
                        .notify("retry", tag)
                        .await
                        .expect("expected the retry written");
                }
                asked
            })
        })
        .collect();
    let mut asked = Vec::new();
    for caller in callers {
        asked.extend(
            within("a caller", caller)
                .await
                .expect("expected the caller to finish"),
        );
    }
    busy.abort();
    let _ = busy.await;
    sent(&link, CALLERS * ROUNDS * 3).await;

    let written = frames(&link);
    let places = |wanted: &dyn Fn(&Value) -> bool| -> Vec<usize> {
        written
            .iter()
            .enumerate()
            .filter(|(_, frame)| wanted(frame))
            .map(|(at, _)| at)
            .collect()
    };
    for (id, tag) in &asked {
        let asked_at = places(&|frame| frame["method"] == "ask" && frame["id"] == *id);
        let cancel_at =
            places(&|frame| frame["method"] == CANCEL && frame["params"]["requestId"] == *id);
        let retry_at = places(&|frame| frame["method"] == "retry" && frame["params"] == *tag);
        let in_order = matches!(
            (&asked_at[..], &cancel_at[..], &retry_at[..]),
            ([asked_at], [cancel_at], [retry_at]) if asked_at < cancel_at && cancel_at < retry_at
        );
        assert!(
            in_order,
            "expected request {id} once, then its one cancel, then its retry: [ask] < [cancel] < [retry] | received ask at {asked_at:?}, cancel at {cancel_at:?}, retry at {retry_at:?}"
        );
    }
    let observed = (asked.len(), written.len());
    let expected = (CALLERS * ROUNDS, CALLERS * ROUNDS * 3);
    assert_eq!(
        observed, expected,
        "expected every request written, cancelled and retried, and nothing else on the wire: {expected:?} | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// Refuses every frame naming `ask` whole, the way a transport refuses a frame it will not
/// carry: no byte accepted, the link as good as it was.
struct RefuseAsks {
    inner: Box<dyn crate::link::LinkSender>,
}

#[async_trait::async_trait]
impl crate::link::LinkSender for RefuseAsks {
    async fn send(&mut self, message: String) -> crate::error::Result<()> {
        if message.contains("\"ask\"") {
            return Err(Error::LimitExceeded {
                subject: "local outgoing frame bytes",
                limit: 1,
                received: message.len(),
            }
            .with_dispatch(crate::Dispatch::NotSubmitted));
        }
        self.inner.send(message).await
    }

    async fn close(&mut self) -> crate::error::Result<()> {
        self.inner.close().await
    }
}

/// The one request the peer never got that is announced all the same: given up while its
/// frame is being written, with the link then refusing the frame whole. The call is let go
/// between the write beginning and the link's answer, which is before anyone can know.
#[tokio::test]
async fn a_request_given_up_mid_write_is_cancelled_even_if_the_link_then_refuses_it() {
    let link = ScriptedLink::new();
    let (sender, receiver) = link.clone().into_link().split();
    let handler = RecordingHandler::arc(None);
    let client = Client::connect_with(
        crate::link::Link::new(Box::new(RefuseAsks { inner: sender }), receiver),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options(),
        WireOptions::new().with_cancel_on_abandon(CANCEL),
    );
    let reply = Arc::new(std::sync::Mutex::new(None));
    *client
        .state
        .after_write_began
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new({
        let reply = Arc::clone(&reply);
        move || {
            let reply = reply
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            drop::<Option<crate::jsonrpc::Reply>>(reply);
        }
    }));
    let (written, waiting) = client
        .submit_request("ask", json!({}), RequestOptions::new())
        .expect("expected the request queued")
        .into_parts();
    *reply
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(waiting);
    let refused = within("the write refused", written).await;
    within("the next write", client.notify("after", json!({})))
        .await
        .expect("expected the notification written");

    let observed = (refused.is_err(), methods(&link));
    assert_eq!(
        observed,
        (true, vec![String::from(CANCEL), String::from("after")]),
        "expected the refusal reported and the cancel sent for a call let go mid-write: (true, [\"$/cancel_request\", \"after\"]) | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// Requests the link refuses whole, each let go once its write is over, with the record of
/// calls kept busy from another thread so that the writer is still waiting to tell the caller.
/// None of them reached the peer, and none is cancelled.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_refused_whole_are_never_cancelled_with_the_record_of_calls_contended() {
    const ROUNDS: usize = 200;
    let link = ScriptedLink::new();
    let (sender, receiver) = link.clone().into_link().split();
    let handler = RecordingHandler::arc(None);
    let client = Arc::new(Client::connect_with(
        crate::link::Link::new(Box::new(RefuseAsks { inner: sender }), receiver),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options(),
        WireOptions::new().with_cancel_on_abandon(CANCEL),
    ));
    let busy = tokio::spawn({
        let client = Arc::clone(&client);
        async move {
            loop {
                let held = client.state.pending.lock().await;
                tokio::task::yield_now().await;
                drop(held);
            }
        }
    });
    for _ in 0..ROUNDS {
        let (written, reply) = client
            .submit_request("ask", json!({}), RequestOptions::new())
            .expect("expected the request queued")
            .into_parts();
        // The frame is let go as its write ends, before the writer waits for the record.
        until("the refused frame to be let go", || {
            written.started() && outbox_holds(&client) == (0, 0)
        })
        .await;
        let dropped = tokio::task::spawn_blocking(move || drop(reply));
        within("the reply dropped", dropped)
            .await
            .expect("expected the dropping thread to finish");
        let refused = within("the write refused", written).await;
        assert!(
            refused.is_err(),
            "expected every request refused by the link: Err | received {refused:?}"
        );
    }
    busy.abort();
    let _ = busy.await;
    within("the last write", client.notify("last", json!({})))
        .await
        .expect("expected the notification written");

    let observed = (
        methods(&link),
        client.state.outbox.aside(),
        client.is_closed(),
    );
    assert_eq!(
        observed,
        (vec![String::from("last")], 0, false),
        "expected no cancel for any request the peer never got, and the connection open: ([\"last\"], 0, false) | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// Which side a round of the race below gives the head start.
#[derive(Clone, Copy)]
enum HeadStart {
    /// The response is read and taken before the caller lets go.
    Reader,
    /// The caller has let go before the response is sent.
    Caller,
    /// Both at once, from two threads.
    Nobody,
}

/// A response racing the caller that gives its call up, from two threads, request after
/// request, with every third round given to one side and every third to the other so that
/// both ends are certain to occur. Each call ends exactly one way: the reader took it and no
/// cancel was sent, or the caller let it go first and one cancel was sent. Never both, never
/// neither, never an answer in the caller's hands beside a cancel, and nothing left behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_response_racing_a_call_given_up_ends_the_call_one_way_only() {
    const ROUNDS: usize = 300;
    let (link, handler, client) =
        connect(options(), WireOptions::new().with_cancel_on_abandon(CANCEL));
    let taken_by_reader = |key: &str| {
        client
            .state
            .claimed_by_reader
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|taken| taken == key)
    };
    let mut calls = Vec::new();
    for round in 0..ROUNDS {
        let request = if round % 2 == 0 {
            RequestOptions::new()
        } else {
            ordered_options()
        };
        let head_start = match round % 3 {
            0 => HeadStart::Reader,
            1 => HeadStart::Caller,
            _ => HeadStart::Nobody,
        };
        let submitted = client
            .submit_request("ask", json!({}), request)
            .expect("expected the request queued");
        let id = submitted.id().as_json().clone();
        let key = submitted.id().key();
        let (written, reply) = submitted.into_parts();
        within("the request written", written)
            .await
            .expect("expected the request written");
        let (give_up, given_up) = tokio::sync::oneshot::channel::<()>();
        let caller = tokio::spawn(async move {
            tokio::select! {
                answer = reply => answer.ok(),
                _ = given_up => None,
            }
        });
        let response = json!({ "jsonrpc": "2.0", "id": id, "result": "done" }).to_string();
        let answered = match head_start {
            HeadStart::Reader => {
                link.push_line(response);
                until("the reader to take the call", || taken_by_reader(&key)).await;
                let _ = give_up.send(());
                within("the caller", caller).await
            }
            HeadStart::Caller => {
                let _ = give_up.send(());
                let answered = within("the caller", caller).await;
                link.push_line(response);
                answered
            }
            HeadStart::Nobody => {
                link.push_line(response);
                let _ = give_up.send(());
                within("the caller", caller).await
            }
        }
        .expect("expected the caller to finish")
        .is_some();
        calls.push((id, key, answered));
    }
    // A marker behind the last response: once it is handled, every response has been read.
    link.push_line(notification("read to here").to_string());
    handler.wait_for_notifications(1).await;
    within("the last write", client.notify("last", json!({})))
        .await
        .expect("expected the notification written");
    // Given back by whoever let go last, on its own thread: waited for, then asserted.
    until("everything held for the calls to be given back", || {
        client.state.queued_answers.load(Ordering::Acquire) == 0
            && client.state.outbox.aside() == 0
            && client.state.ordered_places.available_permits() == 64
            && client
                .state
                .pending
                .try_lock()
                .is_ok_and(|pending| pending.is_empty())
    })
    .await;

    let written = frames(&link);
    let (mut by_reader, mut by_caller) = (0, 0);
    for (id, key, answered) in &calls {
        let asks = written
            .iter()
            .filter(|frame| frame["method"] == "ask" && frame["id"] == *id)
            .count();
        let cancels = written
            .iter()
            .filter(|frame| frame["method"] == CANCEL && frame["params"]["requestId"] == *id)
            .count();
        let taken = taken_by_reader(key);
        by_reader += usize::from(taken);
        by_caller += cancels;
        let one_way = asks == 1 && usize::from(taken) + cancels == 1 && (taken || !*answered);
        assert!(
            one_way,
            "expected request {id} written once and then taken by the reader or cancelled, exactly one of the two, and answered only if taken: (1 ask, taken xor 1 cancel) | received ({asks} asks, taken {taken}, {cancels} cancels, answered {answered})"
        );
    }
    let others = written.len() - ROUNDS - by_caller - 1;
    let observed = (others, by_reader >= ROUNDS / 3, by_caller >= ROUNDS / 3);
    assert_eq!(
        observed,
        (0, true, true),
        "expected nothing else on the wire and both ends to have occurred at least {} times each: (0, true, true) | received {observed:?} ({by_reader} taken by the reader, {by_caller} cancelled)",
        ROUNDS / 3
    );
    client.close().await.expect("expected a clean close");
}

/// A line the transport decoded from bytes that were not UTF-8 arrives with replacement
/// characters in it. Inside a string of a frame it is a frame like any other; in a line that
/// is not JSON it is answered like any other.
#[tokio::test]
async fn a_line_with_replacement_characters_is_read_for_what_it_is() {
    let (link, handler, client) =
        connect(options(), WireOptions::new().with_parse_error_replies(4));
    link.push_line(
        json!({ "jsonrpc": "2.0", "method": "said", "params": { "text": "caf\u{fffd}" } })
            .to_string(),
    );
    link.push_line("\u{fffd}\u{fffd} not json");
    link.push_line(notification("after").to_string());

    let handled = handler.wait_for_notifications(2).await;
    sent(&link, 1).await;
    let observed = (seen(&handled), link.sent().len());
    assert_eq!(
        observed,
        (vec!["said", "after"], 1),
        "expected the frame handled and the other line answered once: ([\"said\", \"after\"], 1) | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// What turning the answers on costs: an answer is a write, and a peer that sends lines that
/// are not JSON while it does not read what it is sent leaves that write waiting. When its
/// deadline runs out the connection ends, as after any write that could not finish, where a
/// client that answers nothing would have gone on reading.
#[tokio::test(start_paused = true)]
async fn a_parse_error_the_peer_never_reads_ends_the_connection_at_the_write_deadline() {
    let script = ScriptedLink::new();
    let (sender, receiver) = script.clone().into_link().split();
    let handler = RecordingHandler::arc(None);
    let client = Client::connect_with(
        crate::link::Link::new(Box::new(StalledReplies { inner: sender }), receiver),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options().with_request_timeout(Duration::from_millis(50)),
        WireOptions::new().with_parse_error_replies(4),
    );
    let began = tokio::time::Instant::now();
    script.push_line("{oops");

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

/// A peer's question inside a batch waits for the notifications before it, like one on a line
/// of its own: the handler is asked only once it has returned from the first member, however
/// long that takes, and then the question is answered and the member after it is handled.
#[tokio::test]
async fn a_question_in_a_batch_waits_for_the_members_before_it() {
    let (link, handler, client) =
        GatedHandler::connect(options(), WireOptions::new().with_batch_decoding());
    link.push_line(
        json!([
            notification("one"),
            { "jsonrpc": "2.0", "id": 7, "method": "ask", "params": {} },
            notification("two"),
        ])
        .to_string(),
    );
    until("the handler to be inside the first member", || {
        GatedHandler::list(&handler.begun) == ["one"]
    })
    .await;
    handler.gate.add_permits(2);
    sent(&link, 1).await;
    until("the last member to be handled", || {
        GatedHandler::list(&handler.finished).len() == 2
    })
    .await;
    let asked_after: Vec<bool> = handler
        .finished_when_asked
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .map(|finished| finished.first().is_some_and(|first| first == "one"))
        .collect();
    let observed = (
        GatedHandler::list(&handler.finished),
        asked_after,
        frames(&link),
    );
    let expected = (
        vec![String::from("one"), String::from("two")],
        vec![true],
        vec![json!({ "jsonrpc": "2.0", "id": 7, "result": "yes" })],
    );
    assert_eq!(
        observed, expected,
        "expected both members handled in order, the question asked once and after the first, and answered once: {expected:?} | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// The bytes a child wrote, as the stdio transport reads them: through the line reader, with
/// its caps and its repair of bytes that are not UTF-8.
struct ChildOutput {
    chunks: std::collections::VecDeque<Vec<u8>>,
}

#[async_trait::async_trait]
impl crate::process::ByteSource for ChildOutput {
    async fn next_chunk(&mut self) -> crate::error::Result<Option<Vec<u8>>> {
        match self.chunks.pop_front() {
            Some(chunk) => Ok(Some(chunk)),
            // The child is still there: it has only stopped writing.
            None => std::future::pending().await,
        }
    }
}

/// A link's reading half over the line reader, as the stdio transport's is.
struct LinesFromBytes {
    lines: crate::process::LineStream,
}

#[async_trait::async_trait]
impl crate::link::LinkReceiver for LinesFromBytes {
    async fn recv(&mut self) -> crate::error::Result<Option<String>> {
        self.lines.next_line().await
    }
}

/// A link's reading half that fails the way the websocket transport does for a text frame that
/// is not UTF-8.
struct BadTextFrame;

#[async_trait::async_trait]
impl crate::link::LinkReceiver for BadTextFrame {
    async fn recv(&mut self) -> crate::error::Result<Option<String>> {
        Err(Error::Link {
            peer: String::from("ws://peer"),
            message: String::from("a text frame that is not UTF-8"),
        })
    }
}

/// A client that answers parse errors, reading from `receiver`.
fn answering_from(
    receiver: Box<dyn crate::link::LinkReceiver>,
) -> (ScriptedLink, Arc<RecordingHandler>, Client) {
    let link = ScriptedLink::new();
    let (sender, _unused) = link.clone().into_link().split();
    let handler = RecordingHandler::arc(None);
    let client = Client::connect_with(
        crate::link::Link::new(sender, receiver),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options(),
        WireOptions::new().with_parse_error_replies(4),
    );
    (link, handler, client)
}

fn child_output(bytes: &[u8], max_line_bytes: usize) -> Box<dyn crate::link::LinkReceiver> {
    let limits = crate::process::LineLimits {
        max_line_bytes,
        max_buffered_bytes: 4096,
    };
    let source = ChildOutput {
        chunks: std::collections::VecDeque::from([bytes.to_vec()]),
    };
    Box::new(LinesFromBytes {
        lines: crate::process::LineStream::new(Box::new(source), limits),
    })
}

/// Bytes on a child's output that are not UTF-8 are repaired by the line reader and arrive as
/// a line. When what is left is not JSON, a client asked to answer parse errors answers it,
/// and the connection goes on.
#[tokio::test]
async fn a_stdio_line_that_is_not_utf8_is_answered_as_a_parse_error() {
    let mut bytes = vec![0xff, 0xfe, b'\n'];
    bytes.extend_from_slice(notification("after").to_string().as_bytes());
    bytes.push(b'\n');
    let (link, handler, client) = answering_from(child_output(&bytes, 1024));

    let handled = handler.wait_for_notifications(1).await;
    sent(&link, 1).await;
    let observed = (
        frames(&link)[0]["error"]["code"].clone(),
        seen(&handled),
        client.is_closed(),
    );
    assert_eq!(
        observed,
        (json!(-32700), vec!["after"], false),
        "expected the repaired line answered and the next one handled: (-32700, [\"after\"], false) | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// A line longer than the transport's cap never becomes a line: the read fails, which ends the
/// connection. It is not JSON either, but it is not answered: there is no line to answer.
#[tokio::test]
async fn a_stdio_line_past_the_cap_ends_the_connection_and_is_not_answered() {
    let mut bytes = vec![b'x'; 65];
    bytes.push(b'\n');
    let (link, handler, client) = answering_from(child_output(&bytes, 64));

    let termination = one_termination(&handler).await;
    let _ = client.close().await;
    let observed = (&termination, link.sent().len(), client.state.outbox.aside());
    assert!(
        matches!(&observed, (PeerTermination::LinkFailed(cause), 0, 0)
            if cause.contains("bytes of one vendor output line")),
        "expected the read's refusal to end the connection with nothing written or waiting: (LinkFailed naming the line cap, 0, 0) | received {observed:?}"
    );
}

/// A read that fails is not a line either. This is the shape the websocket transport gives a
/// text frame that is not UTF-8, returned here by a fake: the connection ends and nothing is
/// answered. That the transport reports such a frame this way is the transport's to pin.
#[tokio::test]
async fn a_read_that_fails_on_a_bad_text_frame_ends_the_connection_and_is_not_answered() {
    let (link, handler, client) = answering_from(Box::new(BadTextFrame));

    let termination = one_termination(&handler).await;
    let _ = client.close().await;
    let observed = (&termination, link.sent().len(), client.state.outbox.aside());
    assert!(
        matches!(&observed, (PeerTermination::LinkFailed(cause), 0, 0)
            if cause.contains("a text frame that is not UTF-8")),
        "expected the failed read to end the connection with nothing written or waiting: (LinkFailed naming the frame, 0, 0) | received {observed:?}"
    );
}
