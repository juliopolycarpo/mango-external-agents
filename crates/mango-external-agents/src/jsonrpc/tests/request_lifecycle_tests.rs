//! What one request may ask for apart from the connection's defaults (no deadline, a place
//! outside the pending budget, a name), and what becomes of a frame around a close.

use super::outbox_tests::{
    Gated, QUEUED_BYTES, QUEUED_FRAMES, assert_ended, eventually, failure, gated, limits,
    link_held, methods, one_termination, options, ordered_options, outbox_holds, sent, until,
    within,
};
use super::*;
use crate::jsonrpc::{CallFailureCause, ConnectionEnd, Reply, WireOptions};
use std::pin::Pin;
use std::sync::PoisonError;

const LONG: Duration = Duration::from_secs(60 * 60);

fn unbounded() -> RequestOptions {
    RequestOptions::new().without_deadline()
}

/// Whether `reply` has an outcome yet, without waiting for one.
async fn settled(reply: &mut Reply) -> bool {
    std::future::poll_fn(|cx| std::task::Poll::Ready(Pin::new(&mut *reply).poll(cx).is_ready()))
        .await
}

fn answer(gated: &Gated, id: &RequestId, result: Value) {
    gated
        .link
        .push_line(json!({"id": id.as_json(), "result": result}).to_string());
}

/// A request that asked for no deadline is still waiting an hour after the connection's own
/// request timeout, and is answered when the answer comes.
#[tokio::test(start_paused = true)]
async fn a_queued_request_without_a_deadline_waits_for_as_long_as_it_takes() {
    let gated = gated(options(), WireOptions::new());
    let submitted = gated
        .client
        .submit_request("session/prompt", json!({}), unbounded())
        .expect("expected the request queued");
    let id = submitted.id().clone();
    let (written, mut reply) = submitted.into_parts();
    eventually("the request written", written)
        .await
        .expect("expected the request written");

    tokio::time::sleep(LONG).await;
    let early = settled(&mut reply).await;
    assert!(
        !early,
        "expected the reply still waiting an hour past the 5s request timeout: pending | received an outcome"
    );
    answer(&gated, &id, json!("done"));
    let answered = eventually("the answer", reply)
        .await
        .expect("expected the answer");
    assert_eq!(
        answered,
        json!("done"),
        "expected the peer's answer: \"done\" | received {answered}"
    );
    gated.client.close().await.expect("expected a clean close");
}

#[tokio::test(start_paused = true)]
async fn a_waiting_request_without_a_deadline_waits_for_as_long_as_it_takes() {
    let gated = gated(options(), WireOptions::new());
    let asking = {
        let client = Arc::clone(&gated.client);
        tokio::spawn(async move {
            client
                .request_with::<_, Value>("session/prompt", json!({}), unbounded())
                .await
        })
    };
    sent(&gated.link, 1).await;

    tokio::time::sleep(LONG).await;
    assert!(
        !asking.is_finished(),
        "expected the call still waiting an hour past the 5s request timeout: pending | received an outcome"
    );
    gated
        .link
        .push_line(r#"{"jsonrpc":"2.0","id":"1","result":"done"}"#);
    let answered = eventually("the answer", asking)
        .await
        .expect("expected the caller's task to finish")
        .expect("expected the answer");
    assert_eq!(
        answered,
        json!("done"),
        "expected the peer's answer: \"done\" | received {answered}"
    );
    gated.client.close().await.expect("expected a clean close");
}

/// Only the wait for the answer loses its bound. A peer that does not take the frame fails the
/// request at the connection's write bound and ends the connection, as for any request.
#[tokio::test(start_paused = true)]
async fn a_request_without_a_deadline_still_has_its_write_bounded() {
    let handler = RecordingHandler::arc(None);
    let client = stalled_client(Arc::clone(&handler), "session/prompt");
    let began = tokio::time::Instant::now();
    let outcome = client
        .request_with::<_, Value>("session/prompt", json!({}), unbounded())
        .await;
    assert!(
        matches!(&outcome, Err(Error::Timeout { operation, .. }) if operation == "JSON-RPC frame write"),
        "expected the write to time out: Timeout(JSON-RPC frame write) | received {outcome:?}"
    );
    let waited = began.elapsed();
    assert_eq!(
        waited,
        Duration::from_secs(5),
        "expected the write given up at the request timeout: 5s | received {waited:?}"
    );
    assert_write_timeout_ended_the_connection(&handler, &client).await;
    client.close().await.expect("expected a clean close");

    let handler = RecordingHandler::arc(None);
    let client = stalled_client(Arc::clone(&handler), "session/prompt");
    let began = tokio::time::Instant::now();
    let (written, reply) = client
        .submit_request("session/prompt", json!({}), unbounded())
        .expect("expected the request queued")
        .into_parts();
    let outcome = eventually("the stalled write", written).await;
    assert!(
        matches!(&outcome, Err(Error::Timeout { operation, .. }) if operation == "JSON-RPC frame write"),
        "expected the queued write to time out: Timeout(JSON-RPC frame write) | received {outcome:?}"
    );
    let waited = began.elapsed();
    assert_eq!(
        waited,
        Duration::from_secs(5),
        "expected the queued write given up at the request timeout: 5s | received {waited:?}"
    );
    let (cause, _) = failure(eventually("the reply", reply).await);
    assert_eq!(
        cause,
        CallFailureCause::Unwritten,
        "expected the reply failed with its write | received {cause:?}"
    );
    assert_write_timeout_ended_the_connection(&handler, &client).await;
    client.close().await.expect("expected a clean close");
}

/// With no deadline, the connection's end is what ends the wait: the peer going, or a close.
#[tokio::test(start_paused = true)]
async fn a_request_without_a_deadline_ends_with_the_connection() {
    for (ending, expected) in [
        ("exit", ConnectionEnd::Peer(PeerTermination::Exited)),
        ("close", ConnectionEnd::Closed),
    ] {
        let gated = gated(options(), WireOptions::new());
        let (written, reply) = gated
            .client
            .submit_request("session/prompt", json!({}), unbounded())
            .expect("expected the request queued")
            .into_parts();
        eventually("the request written", written)
            .await
            .expect("expected the request written");
        let waiting = {
            let client = Arc::clone(&gated.client);
            tokio::spawn(async move {
                client
                    .request_with::<_, Value>("session/load", json!({}), unbounded())
                    .await
            })
        };
        sent(&gated.link, 2).await;
        tokio::time::sleep(LONG).await;

        if ending == "exit" {
            gated.link.end();
        } else {
            gated.client.close().await.expect("expected a clean close");
        }
        let (cause, _) = failure(eventually("the reply", reply).await);
        assert_eq!(
            cause,
            CallFailureCause::Ended(expected.clone()),
            "expected the reply ended by the {ending} | received {cause:?}"
        );
        let outcome = eventually("the waiting caller", waiting)
            .await
            .expect("expected the caller's task to finish");
        assert!(
            matches!(&outcome, Err(Error::Vendor(_))),
            "expected the waiting caller failed by the {ending}: Err(Vendor) | received {outcome:?}"
        );
        assert_ended(&gated.client, Some(expected));
        gated.client.close().await.expect("expected a clean close");
    }
}

fn one_place() -> ClientOptions {
    let mut one = options();
    one.max_pending_requests = 1;
    one
}

fn outside() -> RequestOptions {
    RequestOptions::new().outside_pending_budget()
}

/// A request outside the pending budget is admitted with the budget full, and takes no place in
/// it: the requests that count are admitted and refused as if it were not there.
#[tokio::test]
async fn queued_requests_outside_the_pending_budget_are_neither_counted_nor_refused() {
    let gated = gated(one_place(), WireOptions::new());
    let mut kept = Vec::new();
    for (method, options) in [
        ("outside-1", outside()),
        ("counted", RequestOptions::new()),
        ("outside-2", outside()),
    ] {
        let (written, reply) = gated
            .client
            .submit_request(method, json!({}), options)
            .expect("expected the request queued")
            .into_parts();
        let outcome = within("the write", written).await;
        assert!(
            outcome.is_ok(),
            "expected `{method}` written with one counted place in all: Ok | received {outcome:?}"
        );
        kept.push(reply);
    }
    let (written, reply) = gated
        .client
        .submit_request("counted-too", json!({}), RequestOptions::new())
        .expect("expected the request queued")
        .into_parts();
    let refused = within("the second counted write", written).await;
    assert!(
        matches!(
            refused.as_ref().map_err(Error::cause),
            Err(Error::LimitExceeded {
                subject: "pending JSON-RPC requests",
                limit: 1,
                received: 2
            })
        ),
        "expected the second counted request refused, counting only the first: LimitExceeded {{ limit: 1, received: 2 }} | received {refused:?}"
    );
    drop(reply);
    let order = methods(&gated.link);
    assert_eq!(
        order,
        vec!["outside-1", "counted", "outside-2"],
        "expected every request but the second counted one on the wire | received {order:?}"
    );
    gated.client.close().await.expect("expected a clean close");
}

#[tokio::test]
async fn waiting_requests_outside_the_pending_budget_are_neither_counted_nor_refused() {
    let gated = gated(one_place(), WireOptions::new());
    let ask = |method: &'static str, options: RequestOptions| {
        let client = Arc::clone(&gated.client);
        tokio::spawn(async move {
            client
                .request_with::<_, Value>(method, json!({}), options)
                .await
        })
    };
    let _first = ask("outside-1", outside());
    sent(&gated.link, 1).await;
    let _counted = ask("counted", RequestOptions::new());
    sent(&gated.link, 2).await;
    let _second = ask("outside-2", outside());
    sent(&gated.link, 3).await;

    let refused = gated
        .client
        .request_with::<_, Value>("counted-too", json!({}), RequestOptions::new())
        .await;
    assert!(
        matches!(
            &refused,
            Err(Error::LimitExceeded {
                subject: "pending JSON-RPC requests",
                limit: 1,
                received: 2
            })
        ),
        "expected the second counted request refused, counting only the first: LimitExceeded {{ limit: 1, received: 2 }} | received {refused:?}"
    );
    let order = methods(&gated.link);
    assert_eq!(
        order,
        vec!["outside-1", "counted", "outside-2"],
        "expected every request but the second counted one on the wire | received {order:?}"
    );
    gated.client.close().await.expect("expected a clean close");
}

/// The name a request was given comes back with its failure, whatever failed it, and stays out
/// of the error's text.
#[tokio::test(start_paused = true)]
async fn a_failed_reply_names_the_request_it_was_for() {
    let gated = gated(options(), WireOptions::new());
    let ask = |label: &'static str, options: RequestOptions| {
        let submitted = gated
            .client
            .submit_request("session/prompt", json!({}), options.labelled(label))
            .expect("expected the request queued");
        (submitted.id().clone(), submitted.into_parts().1)
    };
    let (refused_id, refused) = ask("refused-by-the-peer", RequestOptions::new());
    let (_, late) = ask(
        "timed-out",
        RequestOptions::new().with_timeout(Duration::from_secs(1)),
    );
    let (_, cut_off) = ask("cut-off", unbounded());
    let (_, unnamed) = {
        let submitted = gated
            .client
            .submit_request("session/prompt", json!({}), unbounded())
            .expect("expected the request queued");
        (submitted.id().clone(), submitted.into_parts().1)
    };
    sent(&gated.link, 4).await;
    gated.link.push_line(
        json!({"id": refused_id.as_json(), "error": {"code": -32602, "message": "no"}}).to_string(),
    );

    let mut named = Vec::new();
    for (expected, reply) in [
        (Some("refused-by-the-peer"), refused),
        (Some("timed-out"), late),
    ] {
        let failed = eventually("the reply", reply)
            .await
            .expect_err("expected the reply to fail");
        named.push((expected, failed));
    }
    gated.link.end();
    for (expected, reply) in [(Some("cut-off"), cut_off), (None, unnamed)] {
        let failed = eventually("the reply", reply)
            .await
            .expect_err("expected the reply to fail");
        named.push((expected, failed));
    }
    for (expected, failed) in named {
        let label = failed.label();
        assert_eq!(
            label,
            expected,
            "expected the failure to carry its request's name: {expected:?} | received {label:?} for {:?}",
            failed.cause()
        );
        let shown = format!("{failed} {failed:?}");
        let error = format!("{:?}", failed.into_error());
        assert!(
            expected.is_none_or(|label| !shown.contains(label) && !error.contains(label)),
            "expected the name kept out of the error's text | received {shown} and {error}"
        );
    }
    gated.client.close().await.expect("expected a clean close");
}

/// From the moment a close begins the queue takes nothing more. A caller that read the client
/// as open an instant earlier is refused as it queues: nothing of its frame is held or written.
async fn a_frame_queued_after_a_close_began_is_refused_and_never_written() {
    let gated = gated(options(), WireOptions::new());
    let held = link_held(&gated).await;
    let holding = outbox_holds(&gated.client);
    // The close has to drain this map and waits for it here, with the client already closed.
    let map = gated.client.state.pending.lock().await;
    let closing = {
        let client = Arc::clone(&gated.client);
        tokio::spawn(async move { client.close().await })
    };
    until("the close to have begun", || gated.client.is_closed()).await;

    // What `submit_*` runs once it has read the client as open.
    let notification = gated.client.queue_notification("late", json!({}));
    assert!(
        matches!(&notification, Err(Error::Closed { subject: "link" })),
        "expected the late notification refused: Err(Closed(link)) | received {notification:?}"
    );
    let request = gated
        .client
        .queue_request("late", json!({}), ordered_options());
    assert!(
        matches!(&request, Err(Error::Closed { subject: "link" })),
        "expected the late request refused: Err(Closed(link)) | received {request:?}"
    );
    let after = outbox_holds(&gated.client);
    assert_eq!(
        after, holding,
        "expected nothing of the refused frames held: {holding:?} | received {after:?}"
    );
    let places = gated.client.state.ordered_places.available_permits();
    let all = gated.client.state.options.max_pending_requests;
    assert_eq!(
        places, all,
        "expected the refused request's ordered place given back: {all} | received {places}"
    );

    drop(map);
    gated.gate.add_permits(1);
    within("the held frame", held)
        .await
        .expect("expected the held frame written");
    within("the close", closing)
        .await
        .expect("expected the close task to finish")
        .expect("expected a clean close");
    let order = methods(&gated.link);
    assert_eq!(
        order,
        vec!["hold"],
        "expected only what was queued before the close on the wire: [hold] | received {order:?}"
    );
}

/// A request that got into the queue ahead of a close is a frame queued before it: not written,
/// its write reported as never begun, with the close as the typed reason for both halves.
async fn a_request_queued_ahead_of_a_close_is_refused_unwritten_with_the_close_as_its_reason() {
    let gated = gated(options(), WireOptions::new());
    let held = link_held(&gated).await;
    let (mut written, reply) = gated
        .client
        .submit_request("session/list", json!({}), RequestOptions::new())
        .expect("expected the request queued")
        .into_parts();
    let closing = {
        let client = Arc::clone(&gated.client);
        tokio::spawn(async move { client.close().await })
    };
    until("the close to have begun", || gated.client.is_closed()).await;
    gated.gate.add_permits(1);
    within("the held frame", held)
        .await
        .expect("expected the held frame written");

    let outcome = within("the request's write", &mut written).await;
    assert!(
        matches!(&outcome, Err(Error::Closed { subject: "link" })),
        "expected the write refused by the close: Err(Closed(link)) | received {outcome:?}"
    );
    assert!(
        !written.started(),
        "expected a write the close refused never begun: false | received true"
    );
    let (cause, _) = failure(within("the reply", reply).await);
    assert_eq!(
        cause,
        CallFailureCause::Ended(ConnectionEnd::Closed),
        "expected the reply failed by the close | received {cause:?}"
    );
    within("the close", closing)
        .await
        .expect("expected the close task to finish")
        .expect("expected a clean close");
    let order = methods(&gated.link);
    assert_eq!(
        order,
        vec!["hold"],
        "expected the request absent from the wire: [hold] | received {order:?}"
    );
}

/// A close waits once for the answers this side owes the peer and once for the queue and the
/// link, each for `shutdown_timeout` at most. On a link that takes nothing it gives up after
/// both, and not later.
#[tokio::test(start_paused = true)]
async fn a_close_on_a_link_that_takes_nothing_gives_up_after_two_shutdown_timeouts() {
    let grace = Duration::from_millis(200);
    let gated = gated(options(), WireOptions::new());
    let _held = link_held(&gated).await;
    // The peer asks a question. The answer waits its turn behind the frame that is stuck.
    gated
        .link
        .push_line(r#"{"jsonrpc":"2.0","id":7,"method":"fs/read_text_file"}"#);
    until("the answer's turn queued", || {
        gated.client.state.outbox.turns() == 1
    })
    .await;

    let began = tokio::time::Instant::now();
    let closed = gated.client.close().await;
    let waited = began.elapsed();
    assert!(
        matches!(&closed, Err(Error::Timeout { operation, after }) if operation == "JSON-RPC link shutdown" && *after == grace),
        "expected the close to give up: Timeout(JSON-RPC link shutdown, 200ms) | received {closed:?}"
    );
    assert_eq!(
        waited,
        grace * 2,
        "expected the close over after both waits: 400ms | received {waited:?}"
    );
    let order = methods(&gated.link);
    assert!(
        order.is_empty(),
        "expected nothing written on a link that takes nothing: [] | received {order:?}"
    );
}

/// The peer passed an inbound budget an instant after another cause was put on record. The
/// handler is told the one on record, as the client and the failed calls are.
#[tokio::test]
async fn the_handler_is_told_the_reason_on_record_when_an_inbound_budget_is_passed_after_it() {
    let link = ScriptedLink::new();
    let handler = RecordingHandler::arc(None);
    let client = Client::connect(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        ClientOptions {
            max_pending_bytes: 16,
            ..ClientOptions::new("ACP agent")
        },
    );
    let recorded = PeerTermination::OutboundBackpressure {
        subject: QUEUED_BYTES,
        limit: 8,
        received: 9,
    };
    client.state.ended_with(
        ConnectionEnd::Peer(recorded.clone()),
        client.state.closed_error(),
    );
    link.push_line(r#"{"jsonrpc":"2.0","method":"session/update","params":{"text":"far more than sixteen bytes"}}"#);

    let told = one_termination(&handler).await;
    assert_eq!(
        told, recorded,
        "expected the handler told the reason on record: {recorded:?} | received {told:?}"
    );
    assert_ended(&client, Some(ConnectionEnd::Peer(recorded)));
    client.close().await.expect("expected a clean close");
}

/// Two callers pass the outbound budget at once: one puts its numbers on record, the other's
/// reach the pump. The handler is told the ones on record.
#[tokio::test]
async fn the_handler_is_told_the_outbound_overflow_on_record_when_another_reached_the_pump() {
    let gated = gated(options(), limits(usize::MAX, usize::MAX, 1));
    let _held = link_held(&gated).await;
    let recorded = PeerTermination::OutboundBackpressure {
        subject: QUEUED_BYTES,
        limit: 8,
        received: 9,
    };
    gated.client.state.ended_with(
        ConnectionEnd::Peer(recorded.clone()),
        gated.client.state.closed_error(),
    );
    let refused = gated.client.submit_notification("one-too-many", json!({}));
    assert!(
        matches!(
            refused.as_ref().map_err(Error::cause),
            Err(Error::LimitExceeded {
                subject: QUEUED_FRAMES,
                limit: 1,
                received: 2
            })
        ),
        "expected the caller told its own overflow: LimitExceeded {{ {QUEUED_FRAMES}, limit: 1, received: 2 }} | received {refused:?}"
    );

    let told = one_termination(&gated.handler).await;
    assert_eq!(
        told, recorded,
        "expected the handler told the reason on record: {recorded:?} | received {told:?}"
    );
    assert_ended(&gated.client, Some(ConnectionEnd::Peer(recorded)));
    gated.gate.add_permits(1);
    gated.client.close().await.expect("expected a clean close");
}

/// What an ACP prompt asks for, all at once: no deadline, no place in the pending budget, and
/// its answer after the notifications that came before it.
fn prompt() -> RequestOptions {
    RequestOptions::new()
        .without_deadline()
        .outside_pending_budget()
        .after_earlier_notifications()
        .labelled("prompt")
}

/// The three options together: the request runs beside a full pending budget, outlasts the
/// request timeout, and is answered only once the notification read before its answer has been
/// handled. A second one is ended, by name, when the peer exits.
#[tokio::test(start_paused = true)]
async fn a_prompt_shaped_request_runs_beside_a_full_budget_and_is_answered_in_order() {
    let gated = gated(one_place(), WireOptions::new());
    let (counted_written, _counted) = gated
        .client
        .submit_request("session/list", json!({}), RequestOptions::new())
        .expect("expected the counted request queued")
        .into_parts();
    eventually("the counted request", counted_written)
        .await
        .expect("expected the counted request written");

    let submitted = gated
        .client
        .submit_request("session/prompt", json!({}), prompt())
        .expect("expected the prompt queued with the pending budget full");
    let id = submitted.id().clone();
    let (written, mut reply) = submitted.into_parts();
    let outcome = eventually("the prompt's write", written).await;
    assert!(
        outcome.is_ok(),
        "expected the prompt written beside a full pending budget: Ok | received {outcome:?}"
    );
    tokio::time::sleep(LONG).await;
    let early = settled(&mut reply).await;
    assert!(
        !early,
        "expected the prompt still running an hour on: pending | received an outcome"
    );

    gated
        .link
        .push_line(r#"{"jsonrpc":"2.0","method":"session/update","params":{"n":1}}"#);
    answer(&gated, &id, json!({"stopReason": "end_turn"}));
    let answered = eventually("the prompt's answer", reply)
        .await
        .expect("expected the prompt answered");
    let handled = gated.handler.notifications.lock().await.len();
    assert_eq!(
        (handled, &answered),
        (1, &json!({"stopReason": "end_turn"})),
        "expected the update handled before the answer returned: (1, end_turn) | received ({handled}, {answered})"
    );

    let (written, reply) = gated
        .client
        .submit_request("session/prompt", json!({}), prompt())
        .expect("expected the second prompt queued")
        .into_parts();
    eventually("the second prompt's write", written)
        .await
        .expect("expected the second prompt written");
    gated.link.end();
    let failed = eventually("the second prompt's reply", reply)
        .await
        .expect_err("expected the second prompt ended by the peer's exit");
    let named = (failed.label(), failed.cause().clone());
    assert_eq!(
        named,
        (
            Some("prompt"),
            CallFailureCause::Ended(ConnectionEnd::Peer(PeerTermination::Exited))
        ),
        "expected the prompt ended by the exit, by name | received {named:?}"
    );
    gated.client.close().await.expect("expected a clean close");
}

/// An answer that waits its turn needs a place to wait in, whatever budget its request stands
/// outside of. Those places are their own reserve, and one past it is refused before it is
/// queued.
#[tokio::test]
async fn an_ordered_request_outside_the_pending_budget_still_needs_an_ordered_place() {
    let gated = gated(one_place(), WireOptions::new());
    let (_written, _reply) = gated
        .client
        .submit_request("session/prompt", json!({}), prompt())
        .expect("expected the first prompt queued")
        .into_parts();
    let second = gated
        .client
        .submit_request("session/prompt", json!({}), prompt());
    assert!(
        matches!(
            &second,
            Err(Error::LimitExceeded {
                subject: "ordered JSON-RPC responses awaiting delivery",
                limit: 1,
                received: 2
            })
        ),
        "expected the second ordered request refused by the ordered reserve: LimitExceeded {{ limit: 1, received: 2 }} | received {second:?}"
    );
    sent(&gated.link, 1).await;
    let order = methods(&gated.link);
    assert_eq!(
        order,
        vec!["session/prompt"],
        "expected only the first prompt on the wire | received {order:?}"
    );
    gated.client.close().await.expect("expected a clean close");
}

/// Callers on several threads queue frames while another thread closes. Whatever the
/// interleaving, every frame has exactly one of three fates: refused as it was queued, written
/// whole before the link closed, or (a request the close overtook) reported unwritten with its
/// write never begun. Every write resolves, and nothing reaches the link after its close.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn frames_queued_while_another_thread_closes_are_each_refused_or_written_whole() {
    // The close has to land among the frames for a run to show anything. Almost every run
    // does; a few more are allowed before giving up on seeing one.
    let mut seen = Vec::new();
    for _ in 0..50 {
        let (written, refused) = frames_queued_around_a_close().await;
        seen.push((written, refused));
        if written > 0 && refused > 0 {
            return;
        }
    }
    panic!(
        "expected a run with frames on both sides of the close: (written > 0, refused > 0) | received {seen:?}"
    );
}

/// One run: how many frames were written and how many refused.
async fn frames_queued_around_a_close() -> (usize, usize) {
    const CALLERS: usize = 6;
    const ROUNDS: usize = 40;
    let link = ScriptedLink::new();
    let handler = RecordingHandler::arc(None);
    let mut roomy = options();
    roomy.max_pending_requests = usize::MAX;
    // Machine time, on a machine that may be busy: the close is not what is being timed.
    roomy.shutdown_timeout = Duration::from_secs(30);
    let client = Arc::new(Client::connect(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        roomy,
    ));
    let go = Arc::new(tokio::sync::Barrier::new(CALLERS + 1));

    let mut callers = Vec::new();
    for caller in 0..CALLERS {
        let client = Arc::clone(&client);
        let go = Arc::clone(&go);
        callers.push(tokio::spawn(async move {
            go.wait().await;
            // (tag, is a request, outcome of the write, whether the write began)
            let mut fates = Vec::new();
            let mut replies = Vec::new();
            for round in 0..ROUNDS {
                let tag = (caller * ROUNDS + round) as u64;
                let params = json!({"tag": tag});
                // Half the callers skip the look at the closed flag, as one that read it open a
                // moment before the close would: the queue itself has to turn them away.
                let trusting = caller % 2 == 1;
                if round % 2 == 0 {
                    let queued = if trusting {
                        client.queue_notification("note", params)
                    } else {
                        client.submit_notification("note", params)
                    };
                    match queued {
                        Err(refused) => fates.push((tag, false, Err(refused), false)),
                        Ok(mut written) => {
                            let outcome = within("a notification's write", &mut written).await;
                            fates.push((tag, false, outcome, written.started()));
                        }
                    }
                } else {
                    let queued = if trusting {
                        client.queue_request("ask", params, RequestOptions::new())
                    } else {
                        client.submit_request("ask", params, RequestOptions::new())
                    };
                    match queued {
                        Err(refused) => fates.push((tag, true, Err(refused), false)),
                        Ok(submitted) => {
                            let (mut written, reply) = submitted.into_parts();
                            let outcome = within("a request's write", &mut written).await;
                            fates.push((tag, true, outcome, written.started()));
                            replies.push((tag, reply));
                        }
                    }
                }
                tokio::task::yield_now().await;
            }
            // The peer answers nothing, so every reply is ended by the close, once: as a call
            // the close failed, or as a frame it kept from being written.
            for (tag, reply) in replies {
                let (cause, _) = failure(within("a request's reply", reply).await);
                assert!(
                    matches!(
                        cause,
                        CallFailureCause::Ended(ConnectionEnd::Closed) | CallFailureCause::Unwritten
                    ),
                    "expected the reply ended by the close: Ended(Closed) or Unwritten | received {cause:?} for {tag}"
                );
            }
            fates
        }));
    }
    go.wait().await;
    tokio::task::yield_now().await;
    client.close().await.expect("expected a clean close");

    let mut refused_frames = 0;
    let mut written_tags = std::collections::HashSet::new();
    for caller in callers {
        let fates = within("a caller", caller)
            .await
            .expect("expected the caller's task to finish");
        for (tag, is_request, outcome, began) in fates {
            match outcome {
                Ok(()) => {
                    assert!(
                        began,
                        "expected a written frame's write to have begun: true | received false for {tag}"
                    );
                    written_tags.insert(tag);
                }
                Err(refused) => {
                    refused_frames += 1;
                    assert!(
                        matches!(refused.cause(), Error::Closed { subject: "link" }),
                        "expected a frame that was not written refused by the close: Err(Closed(link)) | received {refused:?} for {tag} (request: {is_request})"
                    );
                    assert!(
                        !began,
                        "expected a refused frame's write never begun: false | received true for {tag}"
                    );
                }
            }
        }
    }
    // The fake link counts every send it was handed after its close, and records none of them.
    let late = link.refused_sends();
    assert_eq!(
        late, 0,
        "expected nothing handed to the link after its close: 0 sends | received {late}"
    );
    let frames = link.sent();
    let on_wire: std::collections::HashSet<u64> = frames
        .iter()
        .map(|frame| {
            let frame: Value = serde_json::from_str(frame).expect("expected a whole JSON frame");
            frame["params"]["tag"].as_u64().expect("expected a tag")
        })
        .collect();
    assert_eq!(
        (on_wire.len(), &on_wire),
        (frames.len(), &written_tags),
        "expected the wire to hold exactly the frames reported written, each once"
    );
    (written_tags.len(), refused_frames)
}

/// A frame that comes after the peer has gone, or after it passed an inbound budget, is turned
/// away by the queue like one that comes after a close: nothing of it is queued or written.
async fn a_frame_queued_after_the_connection_ended_under_this_side_is_refused() {
    for ending in ["exit", "inbound overflow"] {
        let link = ScriptedLink::new();
        let handler = RecordingHandler::arc(None);
        let client = Client::connect(
            link.clone().into_link(),
            Arc::clone(&handler) as Arc<dyn PeerHandler>,
            ClientOptions {
                max_pending_bytes: 16,
                ..ClientOptions::new("ACP agent")
            },
        );
        if ending == "exit" {
            link.end();
        } else {
            link.push_line(
                r#"{"jsonrpc":"2.0","method":"session/update","params":{"text":"far more than sixteen bytes"}}"#,
            );
        }
        one_termination(&handler).await;

        // What `submit_*` runs once it has read the client as open.
        let notification = client.queue_notification("late", json!({}));
        assert!(
            matches!(&notification, Err(Error::Closed { subject: "link" })),
            "expected a notification after the {ending} refused: Err(Closed(link)) | received {notification:?}"
        );
        let request = client.queue_request("late", json!({}), RequestOptions::new());
        assert!(
            matches!(&request, Err(Error::Closed { subject: "link" })),
            "expected a request after the {ending} refused: Err(Closed(link)) | received {request:?}"
        );
        let holding = outbox_holds(&client);
        assert_eq!(
            holding,
            (0, 0),
            "expected nothing of the refused frames held after the {ending}: (0, 0) | received {holding:?}"
        );
        let on_wire = link.sent();
        assert!(
            on_wire.is_empty(),
            "expected nothing written after the {ending}: [] | received {on_wire:?}"
        );
        client.close().await.expect("expected a clean close");
    }
}

/// A frame that comes too late is refused for that, before the queue's bounds are looked at: a
/// full queue does not turn a late frame into a peer that stopped reading.
async fn a_late_frame_is_refused_as_late_even_when_the_queue_is_full() {
    let gated = gated(options(), limits(usize::MAX, usize::MAX, 1));
    let held = link_held(&gated).await;
    let map = gated.client.state.pending.lock().await;
    let closing = {
        let client = Arc::clone(&gated.client);
        tokio::spawn(async move { client.close().await })
    };
    until("the close to have begun", || gated.client.is_closed()).await;

    let late = gated.client.queue_notification("late", json!({}));
    assert!(
        matches!(&late, Err(Error::Closed { subject: "link" })),
        "expected the late frame refused as late: Err(Closed(link)) | received {late:?}"
    );
    assert_ended(&gated.client, Some(ConnectionEnd::Closed));

    drop(map);
    gated.gate.add_permits(1);
    within("the held frame", held)
        .await
        .expect("expected the frame queued before the close still written");
    within("the close", closing)
        .await
        .expect("expected the close task to finish")
        .expect("expected a clean close");
    let terminations = gated.handler.terminations.lock().await.clone();
    assert!(
        terminations.is_empty(),
        "expected no termination from a late frame: [] | received {terminations:?}"
    );
}

/// A caller that waits for its own request, asked for no deadline and for its answer in order,
/// is ended by the peer's exit once the notifications read before the exit have been handled.
#[tokio::test(start_paused = true)]
async fn a_waiting_prompt_shaped_request_is_ended_by_the_peers_exit_after_the_drain() {
    let gated = gated(one_place(), WireOptions::new());
    let asking = {
        let client = Arc::clone(&gated.client);
        tokio::spawn(async move {
            client
                .request_with::<_, Value>("session/prompt", json!({}), prompt())
                .await
        })
    };
    sent(&gated.link, 1).await;
    tokio::time::sleep(LONG).await;
    gated
        .link
        .push_line(r#"{"jsonrpc":"2.0","method":"session/update","params":{"n":1}}"#);
    gated.link.end();

    let outcome = eventually("the waiting caller", asking)
        .await
        .expect("expected the caller's task to finish");
    let handled = gated.handler.notifications.lock().await.len();
    assert!(
        matches!(&outcome, Err(Error::Vendor(vendor)) if vendor.message == "the ACP agent exited")
            && handled == 1,
        "expected the call ended by the exit after the update was handled: (Err(the ACP agent exited), 1) | received ({outcome:?}, {handled})"
    );
    gated.client.close().await.expect("expected a clean close");
}

/// A waiting caller that runs out its deadline leaves no entry behind for its answer.
#[tokio::test(start_paused = true)]
async fn a_waiting_request_that_times_out_leaves_no_pending_entry() {
    let gated = gated(options(), WireOptions::new());
    let brief = RequestOptions::new().with_timeout(Duration::from_secs(2));
    let began = tokio::time::Instant::now();
    let outcome = gated
        .client
        .request_with::<_, Value>("session/list", json!({}), brief)
        .await;
    let waited = began.elapsed();
    assert!(
        matches!(&outcome, Err(Error::Timeout { operation, after }) if operation == "a JSON-RPC request" && *after == Duration::from_secs(2))
            && waited == Duration::from_secs(2),
        "expected Timeout(a JSON-RPC request, 2s) after 2s | received {outcome:?} after {waited:?}"
    );
    until("the timed-out call's entry removed", || {
        gated
            .client
            .state
            .pending
            .try_lock()
            .is_ok_and(|pending| pending.is_empty())
    })
    .await;
    gated.client.close().await.expect("expected a clean close");
}

/// A receiving half whose read fails once it is told to, as a carrier that broke would.
struct FailingReceiver {
    fail: Arc<Notify>,
}

#[async_trait::async_trait]
impl crate::link::LinkReceiver for FailingReceiver {
    async fn recv(&mut self) -> crate::error::Result<Option<String>> {
        self.fail.notified().await;
        Err(Error::Link {
            peer: String::from("fake peer"),
            message: String::from("the read failed"),
        })
    }
}

/// A client whose sends are recorded by `link` and whose next read fails when `fail` is told.
fn client_with_a_failing_read(link: &ScriptedLink) -> (Arc<RecordingHandler>, Arc<Notify>, Client) {
    let (sender, _receiver) = link.clone().into_link().split();
    let fail = Arc::new(Notify::new());
    let handler = RecordingHandler::arc(None);
    let client = Client::connect(
        crate::link::Link::new(
            sender,
            Box::new(FailingReceiver {
                fail: Arc::clone(&fail),
            }),
        ),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options(),
    );
    (handler, fail, client)
}

/// A read that fails ends the connection with the sending half still good. A frame queued after
/// that is refused like one queued after any other end, not written into a connection already
/// reported ended.
async fn a_frame_queued_after_a_read_failed_is_refused_and_never_written() {
    let link = ScriptedLink::new();
    let (handler, fail, client) = client_with_a_failing_read(&link);
    fail.notify_one();
    let termination = one_termination(&handler).await;
    assert!(
        matches!(&termination, PeerTermination::LinkFailed(cause) if cause.contains("the read failed")),
        "expected the link failed by the read: LinkFailed(.. the read failed ..) | received {termination:?}"
    );

    // What `submit_*` runs once it has read the client as open.
    let notification = client.queue_notification("late", json!({}));
    assert!(
        matches!(&notification, Err(Error::Closed { subject: "link" })),
        "expected a notification after the failed read refused: Err(Closed(link)) | received {notification:?}"
    );
    let request = client.queue_request("late", json!({}), RequestOptions::new());
    assert!(
        matches!(&request, Err(Error::Closed { subject: "link" })),
        "expected a request after the failed read refused: Err(Closed(link)) | received {request:?}"
    );
    let on_wire = link.sent();
    assert!(
        on_wire.is_empty(),
        "expected nothing written after the failed read: [] | received {on_wire:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// The queue is sealed before the closed flag goes up. A frame that arrives between the two, on
/// a queue that is full, is refused as late. Measured against the bounds instead, it would be
/// taken for a peer that stopped reading and end the connection under the close.
#[tokio::test]
async fn a_frame_queued_between_the_seal_and_the_closed_flag_is_refused_as_late() {
    let gated = gated(options(), limits(usize::MAX, usize::MAX, 1));
    let held = link_held(&gated).await;
    // What the caller in the window saw: (the closed flag, what queueing its frame returned).
    let seen = Arc::new(std::sync::Mutex::new(None));
    {
        let client = Arc::clone(&gated.client);
        let seen = Arc::clone(&seen);
        *gated
            .client
            .state
            .after_seal
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(move || {
            let outcome = client.queue_notification("late", json!({}));
            *seen.lock().unwrap_or_else(PoisonError::into_inner) =
                Some((client.is_closed(), outcome.map(drop)));
        }));
    }
    let closing = {
        let client = Arc::clone(&gated.client);
        tokio::spawn(async move { client.close().await })
    };
    until("the close to have begun", || gated.client.is_closed()).await;

    let seen = seen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
        .expect("expected the hook to have run");
    assert!(
        matches!(&seen, (false, Err(Error::Closed { subject: "link" }))),
        "expected the frame in the window refused as late, with the flag still down: (false, Err(Closed(link))) | received {seen:?}"
    );
    gated.gate.add_permits(1);
    within("the held frame", held)
        .await
        .expect("expected the frame queued before the close still written");
    within("the close", closing)
        .await
        .expect("expected the close task to finish")
        .expect("expected a clean close");
    assert_ended(&gated.client, Some(ConnectionEnd::Closed));
    let terminations = gated.handler.terminations.lock().await.clone();
    assert!(
        terminations.is_empty(),
        "expected no termination from a late frame: [] | received {terminations:?}"
    );
}

/// A request that asked for its answer in order and raced a close is refused for the close,
/// with the ordered reserve full: too late is said before a place is asked for.
async fn a_late_ordered_request_is_refused_for_the_close_with_the_ordered_reserve_full() {
    let gated = gated(one_place(), WireOptions::new());
    let (written, _reply) = gated
        .client
        .submit_request("session/prompt", json!({}), prompt())
        .expect("expected the prompt queued")
        .into_parts();
    within("the prompt's write", written)
        .await
        .expect("expected the prompt written");
    let places = gated.client.state.ordered_places.available_permits();
    assert_eq!(
        places, 0,
        "expected the ordered reserve full: 0 places | received {places}"
    );
    let map = gated.client.state.pending.lock().await;
    let closing = {
        let client = Arc::clone(&gated.client);
        tokio::spawn(async move { client.close().await })
    };
    until("the close to have begun", || gated.client.is_closed()).await;

    let late = gated
        .client
        .queue_request("session/prompt", json!({}), prompt());
    assert!(
        matches!(&late, Err(Error::Closed { subject: "link" })),
        "expected the late ordered request refused for the close: Err(Closed(link)) | received {late:?}"
    );
    drop(map);
    within("the close", closing)
        .await
        .expect("expected the close task to finish")
        .expect("expected a clean close");
}

/// With no deadline, every end of the connection has to end the wait. The peer's exit and a
/// close are covered above; these are the client being dropped, the link failing and this
/// side's own outbound budget being passed.
#[tokio::test(start_paused = true)]
async fn a_request_without_a_deadline_is_ended_by_a_dropped_client() {
    let gated = gated(options(), WireOptions::new());
    let (written, reply) = gated
        .client
        .submit_request("session/prompt", json!({}), unbounded())
        .expect("expected the request queued")
        .into_parts();
    eventually("the request written", written)
        .await
        .expect("expected the request written");
    tokio::time::sleep(LONG).await;
    let began = tokio::time::Instant::now();

    let Gated { client, .. } = gated;
    drop(Arc::into_inner(client).expect("expected the test to own the client"));
    let (cause, _) = failure(eventually("the reply", reply).await);
    let waited = began.elapsed();
    assert_eq!(
        (&cause, waited),
        (
            &CallFailureCause::Ended(ConnectionEnd::Closed),
            Duration::ZERO
        ),
        "expected the reply ended by the drop at once: (Ended(Closed), 0s) | received ({cause:?}, {waited:?})"
    );
}

#[tokio::test(start_paused = true)]
async fn a_request_without_a_deadline_is_ended_by_a_failed_link() {
    let link = ScriptedLink::new();
    let (handler, fail, client) = client_with_a_failing_read(&link);
    let (written, reply) = client
        .submit_request("session/prompt", json!({}), unbounded())
        .expect("expected the request queued")
        .into_parts();
    eventually("the request written", written)
        .await
        .expect("expected the request written");
    tokio::time::sleep(LONG).await;

    fail.notify_one();
    let termination = one_termination(&handler).await;
    let (cause, _) = failure(eventually("the reply", reply).await);
    assert_eq!(
        cause,
        CallFailureCause::Ended(ConnectionEnd::Peer(termination.clone())),
        "expected the reply ended by the failed link: Ended(Peer({termination:?})) | received {cause:?}"
    );
    client.close().await.expect("expected a clean close");
}

#[tokio::test(start_paused = true)]
async fn a_request_without_a_deadline_is_ended_by_an_outbound_overflow() {
    let gated = gated(options(), limits(usize::MAX, usize::MAX, 1));
    let (written, reply) = gated
        .client
        .submit_request("session/prompt", json!({}), unbounded())
        .expect("expected the request queued")
        .into_parts();
    eventually("the request written", written)
        .await
        .expect("expected the request written");
    tokio::time::sleep(LONG).await;

    let _held = link_held(&gated).await;
    let refused = gated.client.submit_notification("one-too-many", json!({}));
    assert!(
        refused.is_err(),
        "expected the frame past the budget refused: Err | received {refused:?}"
    );
    let termination = one_termination(&gated.handler).await;
    let (cause, _) = failure(eventually("the reply", reply).await);
    assert!(
        matches!(&termination, PeerTermination::OutboundBackpressure { .. })
            && cause == CallFailureCause::Ended(ConnectionEnd::Peer(termination.clone())),
        "expected the reply ended by the outbound overflow: Ended(Peer(OutboundBackpressure)) | received {cause:?} with {termination:?}"
    );
    gated.gate.add_permits(1);
    gated.client.close().await.expect("expected a clean close");
}

/// A prompt-shaped request given up by its caller leaves nothing behind: its entry among the
/// calls awaiting an answer goes, and so does its place in the ordered reserve.
async fn a_prompt_shaped_request_given_up_gives_back_its_entry_and_its_ordered_place() {
    let gated = gated(one_place(), WireOptions::new());
    let (written, reply) = gated
        .client
        .submit_request("session/prompt", json!({}), prompt())
        .expect("expected the prompt queued")
        .into_parts();
    within("the prompt's write", written)
        .await
        .expect("expected the prompt written");
    let holding = (
        gated.client.state.pending.lock().await.len(),
        gated.client.state.ordered_places.available_permits(),
    );
    assert_eq!(
        holding,
        (1, 0),
        "expected the prompt to hold one entry and the one ordered place: (1, 0) | received {holding:?}"
    );

    drop(reply);
    until("the abandoned prompt's entry and place given back", || {
        gated
            .client
            .state
            .pending
            .try_lock()
            .is_ok_and(|pending| pending.is_empty())
            && gated.client.state.ordered_places.available_permits() == 1
    })
    .await;
    let again = gated
        .client
        .submit_request("session/prompt", json!({}), prompt());
    assert!(
        again.is_ok(),
        "expected the next prompt admitted into the place given back: Ok | received {again:?}"
    );
    gated.client.close().await.expect("expected a clean close");
}

/// `request` and `notify` write for themselves and do not pass through the queue. On a
/// connection that has ended they are what they were: a request is refused as closed, and a
/// notification is dropped without a word. Neither reaches the link.
#[tokio::test]
async fn waiting_calls_made_after_the_connection_ended_are_refused_or_dropped_as_before() {
    for ending in ["close", "exit"] {
        let gated = gated(options(), WireOptions::new());
        if ending == "close" {
            gated.client.close().await.expect("expected a clean close");
        } else {
            gated.link.end();
            one_termination(&gated.handler).await;
        }

        let asked = gated
            .client
            .request::<_, Value>("session/list", json!({}))
            .await;
        assert!(
            matches!(&asked, Err(Error::Closed { subject: "link" })),
            "expected a request after the {ending} refused: Err(Closed(link)) | received {asked:?}"
        );
        let told = gated.client.notify("session/cancel", json!({})).await;
        assert!(
            told.is_ok(),
            "expected a notification after the {ending} dropped without an error: Ok(()) | received {told:?}"
        );
        let on_wire = gated.link.sent();
        let handed_late = gated.link.refused_sends();
        assert!(
            on_wire.is_empty() && handed_late == 0,
            "expected nothing handed to the link after the {ending}: ([], 0) | received ({on_wire:?}, {handed_late})"
        );
        gated.client.close().await.expect("expected a clean close");
    }
}

/// The tests above that place a frame around an end of the connection, on one thread and on
/// four.
mod on_either_runtime {
    #[tokio::test]
    async fn a_frame_queued_after_a_close_began_is_refused_and_never_written_on_one_thread() {
        super::a_frame_queued_after_a_close_began_is_refused_and_never_written().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_frame_queued_after_a_close_began_is_refused_and_never_written_on_four_threads() {
        super::a_frame_queued_after_a_close_began_is_refused_and_never_written().await;
    }

    #[tokio::test]
    async fn a_request_queued_ahead_of_a_close_is_refused_unwritten_with_the_close_as_its_reason_on_one_thread()
     {
        super::a_request_queued_ahead_of_a_close_is_refused_unwritten_with_the_close_as_its_reason(
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_request_queued_ahead_of_a_close_is_refused_unwritten_with_the_close_as_its_reason_on_four_threads()
     {
        super::a_request_queued_ahead_of_a_close_is_refused_unwritten_with_the_close_as_its_reason(
        )
        .await;
    }

    #[tokio::test]
    async fn a_late_frame_is_refused_as_late_even_when_the_queue_is_full_on_one_thread() {
        super::a_late_frame_is_refused_as_late_even_when_the_queue_is_full().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_late_frame_is_refused_as_late_even_when_the_queue_is_full_on_four_threads() {
        super::a_late_frame_is_refused_as_late_even_when_the_queue_is_full().await;
    }

    #[tokio::test]
    async fn a_frame_queued_after_a_read_failed_is_refused_and_never_written_on_one_thread() {
        super::a_frame_queued_after_a_read_failed_is_refused_and_never_written().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_frame_queued_after_a_read_failed_is_refused_and_never_written_on_four_threads() {
        super::a_frame_queued_after_a_read_failed_is_refused_and_never_written().await;
    }

    #[tokio::test]
    async fn a_late_ordered_request_is_refused_for_the_close_with_the_ordered_reserve_full_on_one_thread()
     {
        super::a_late_ordered_request_is_refused_for_the_close_with_the_ordered_reserve_full()
            .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_late_ordered_request_is_refused_for_the_close_with_the_ordered_reserve_full_on_four_threads()
     {
        super::a_late_ordered_request_is_refused_for_the_close_with_the_ordered_reserve_full()
            .await;
    }

    #[tokio::test]
    async fn a_prompt_shaped_request_given_up_gives_back_its_entry_and_its_ordered_place_on_one_thread()
     {
        super::a_prompt_shaped_request_given_up_gives_back_its_entry_and_its_ordered_place().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_prompt_shaped_request_given_up_gives_back_its_entry_and_its_ordered_place_on_four_threads()
     {
        super::a_prompt_shaped_request_given_up_gives_back_its_entry_and_its_ordered_place().await;
    }

    #[tokio::test]
    async fn a_frame_queued_after_the_connection_ended_under_this_side_is_refused_on_one_thread() {
        super::a_frame_queued_after_the_connection_ended_under_this_side_is_refused().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_frame_queued_after_the_connection_ended_under_this_side_is_refused_on_four_threads()
    {
        super::a_frame_queued_after_the_connection_ended_under_this_side_is_refused().await;
    }
}
