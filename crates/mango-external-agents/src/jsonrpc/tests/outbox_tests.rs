//! Frames queued without waiting: their order on the wire, their bounds, and what becomes of
//! them when their caller, the link or the client goes away.

use super::*;
use crate::Dispatch;
use crate::jsonrpc::outbox::{Refusal, admission};
use crate::jsonrpc::{
    CallFailure, CallFailureCause, ConnectionEnd, JsonRpcError, Reply, WireOptions, Written,
};
use std::sync::PoisonError;

const FRAME_SUBJECT: &str = "bytes of one outgoing JSON-RPC frame";
const QUEUED_FRAMES: &str = "outgoing JSON-RPC frames awaiting their write";
const QUEUED_BYTES: &str = "bytes of outgoing JSON-RPC frames awaiting their write";

fn limits(frame: usize, bytes: usize, frames: usize) -> WireOptions {
    WireOptions::new()
        .with_max_outbound_frame_bytes(frame)
        .with_max_outbound_queued_bytes(bytes)
        .with_max_outbound_queued_frames(frames)
}

#[test]
fn a_frame_is_admitted_up_to_each_bound_and_refused_one_past_it() {
    let bounds = limits(10, 25, 3);
    for (frames, held, bytes, expected) in [
        (0, 0, 10, Ok(())),
        (
            0,
            0,
            11,
            Err(Refusal::FrameTooLarge {
                limit: 10,
                received: 11,
            }),
        ),
        (2, 15, 10, Ok(())),
        (
            2,
            16,
            10,
            Err(Refusal::QueueFull {
                subject: QUEUED_BYTES,
                limit: 25,
                received: 26,
            }),
        ),
        (
            3,
            0,
            1,
            Err(Refusal::QueueFull {
                subject: QUEUED_FRAMES,
                limit: 3,
                received: 4,
            }),
        ),
        // Too large on its own is said first: it is true whatever the queue holds.
        (
            3,
            25,
            11,
            Err(Refusal::FrameTooLarge {
                limit: 10,
                received: 11,
            }),
        ),
    ] {
        let admitted = admission(&bounds, frames, held, bytes);
        assert_eq!(
            admitted, expected,
            "expected a {bytes}-byte frame joining {frames} frames of {held} bytes: {expected:?} | received {admitted:?}"
        );
    }
    // Larger than the whole queue may hold is too large on its own as well, with the queue's
    // bound as the limit: nothing about the peer made it not fit.
    let never_fits = admission(&limits(usize::MAX, 25, 3), 0, 0, 26);
    assert_eq!(
        never_fits,
        Err(Refusal::FrameTooLarge {
            limit: 25,
            received: 26
        }),
        "expected a frame an empty queue could not hold refused as too large | received {never_fits:?}"
    );
    let unbounded = admission(&WireOptions::new(), usize::MAX, usize::MAX, usize::MAX);
    assert_eq!(
        unbounded,
        Ok(()),
        "expected default options to bound nothing: Ok | received {unbounded:?}"
    );
}

/// A link whose sends of frames naming `hold` wait at a gate, with a client over it.
struct Gated {
    link: ScriptedLink,
    gate: Arc<tokio::sync::Semaphore>,
    handler: Arc<RecordingHandler>,
    client: Arc<Client>,
}

fn gated(options: ClientOptions, wire: WireOptions) -> Gated {
    let link = ScriptedLink::new();
    let (sender, receiver) = link.clone().into_link().split();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let handler = RecordingHandler::arc(None);
    let client = Arc::new(Client::connect_with(
        crate::link::Link::new(
            Box::new(GatedSender {
                inner: sender,
                method: "hold",
                gate: Arc::clone(&gate),
            }),
            receiver,
        ),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options,
        wire,
    ));
    Gated {
        link,
        gate,
        handler,
        client,
    }
}

fn options() -> ClientOptions {
    ClientOptions::new("ACP agent").with_request_timeout(Duration::from_secs(5))
}

/// The methods of the frames on the wire, in order.
fn methods(link: &ScriptedLink) -> Vec<String> {
    link.sent()
        .iter()
        .map(|frame| {
            let frame: Value = serde_json::from_str(frame).expect("expected a JSON frame");
            frame["method"].as_str().unwrap_or("?").to_owned()
        })
        .collect()
}

async fn sent(link: &ScriptedLink, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), link.wait_for_sent(count))
        .await
        .unwrap_or_else(|_| {
            panic!(
                "expected {count} frames written within 5s | received {:?}",
                link.sent()
            )
        });
}

async fn within<T>(what: &str, waiting: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .unwrap_or_else(|_| panic!("expected {what} within 5s | received nothing"))
}

/// `within` for a test on a paused clock, where the wait under test is itself measured in
/// minutes of that clock: bounded well past any deadline the tests set.
async fn eventually<T>(what: &str, waiting: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(24 * 60 * 60), waiting)
        .await
        .unwrap_or_else(|_| {
            panic!("expected {what} within a day of the paused clock | received nothing")
        })
}

/// Yields until `reached` holds, which a step of another task makes true.
///
/// Gives up after five seconds of the machine's own clock, not after a number of yields and not
/// by the runtime's clock: how many yields another thread needs depends on the load, and a
/// paused clock does not move while this task keeps yielding.
async fn until(what: &str, mut reached: impl FnMut() -> bool) {
    let began = std::time::Instant::now();
    while !reached() {
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "expected {what} within 5s | received not yet"
        );
        tokio::task::yield_now().await;
    }
}

/// Waits until the writer is inside the gated send, which is when the link is held.
async fn link_held(gated: &Gated) -> Written {
    let held = gated
        .client
        .submit_notification("hold", json!({}))
        .expect("expected the held frame queued");
    until("the writer to take the held frame", || held.started()).await;
    held
}

/// What a failed reply says: which side failed it, and the error `request` returns for it.
fn failure(outcome: std::result::Result<Value, CallFailure>) -> (CallFailureCause, Error) {
    let failure = outcome.expect_err("expected the reply to fail");
    (failure.cause().clone(), failure.into_error())
}

fn outbox_holds(client: &Client) -> (usize, usize) {
    client.state.outbox.held()
}

#[tokio::test]
async fn frames_reach_the_wire_in_the_order_their_callers_queued_them() {
    let gated = gated(options(), WireOptions::new());
    // Queued back to back from synchronous code, the way a caller under its own lock does:
    // nothing is awaited between them.
    let prompt = gated
        .client
        .submit_request("session/prompt", json!({}), RequestOptions::new())
        .expect("expected the request queued");
    let cancel = gated
        .client
        .submit_notification("session/cancel", json!({}))
        .expect("expected the notification queued");
    let list = gated
        .client
        .submit_request("session/list", json!({}), RequestOptions::new())
        .expect("expected the second request queued");

    sent(&gated.link, 3).await;
    let order = methods(&gated.link);
    assert_eq!(
        order,
        vec!["session/prompt", "session/cancel", "session/list"],
        "expected the wire in call order: [session/prompt, session/cancel, session/list] | received {order:?}"
    );
    within("the cancel written", cancel)
        .await
        .expect("expected the cancel written");
    assert!(prompt.write_started() && list.write_started());
    let held = outbox_holds(&gated.client);
    assert_eq!(
        held,
        (0, 0),
        "expected nothing held once all three were written: (0, 0) | received {held:?}"
    );
    gated.client.close().await.expect("expected a clean close");
}

/// A caller that waits for its own write takes its turn in the same queue: behind frames queued
/// before it, ahead of frames queued after it, whether or not the link is busy.
#[tokio::test]
async fn a_waiting_caller_takes_its_turn_among_queued_frames() {
    let gated = gated(options(), WireOptions::new());
    let held = link_held(&gated).await;
    let first = gated
        .client
        .submit_notification("first", json!({}))
        .expect("expected a frame queued behind the held one");
    let second = {
        let client = Arc::clone(&gated.client);
        tokio::spawn(async move { client.notify("second", json!({})).await })
    };
    // The waiting caller has queued its turn once the outbox counts it.
    until("the waiting caller's turn queued", || {
        second.is_finished() || gated.client.state.outbox.turns() == 1
    })
    .await;
    let third = gated
        .client
        .submit_notification("third", json!({}))
        .expect("expected a frame queued behind the waiting caller");

    gated.gate.add_permits(1);
    within("the held frame written", held)
        .await
        .expect("expected the held frame written");
    within("the first frame written", first)
        .await
        .expect("expected the first frame written");
    within("the waiting caller", second)
        .await
        .expect("expected the caller's task to finish")
        .expect("expected the waiting caller's frame written");
    within("the third frame written", third)
        .await
        .expect("expected the third frame written");
    let order = methods(&gated.link);
    assert_eq!(
        order,
        vec!["hold", "first", "second", "third"],
        "expected the wire in call order: [hold, first, second, third] | received {order:?}"
    );
    gated.client.close().await.expect("expected a clean close");
}

/// The link is free and the writer has not run yet: the frame queued first still goes first,
/// though the waiting caller could have taken the link there and then.
#[tokio::test]
async fn a_waiting_caller_does_not_overtake_a_frame_queued_on_an_idle_link() {
    let gated = gated(options(), WireOptions::new());
    let first = gated
        .client
        .submit_notification("first", json!({}))
        .expect("expected the frame queued");
    gated
        .client
        .notify("second", json!({}))
        .await
        .expect("expected the waiting caller's frame written");
    within("the first frame written", first)
        .await
        .expect("expected the first frame written");
    let order = methods(&gated.link);
    assert_eq!(
        order,
        vec!["first", "second"],
        "expected the queued frame ahead of the caller that came after it: [first, second] | received {order:?}"
    );
    gated.client.close().await.expect("expected a clean close");
}

#[tokio::test]
async fn a_submitted_request_reports_its_frame_its_write_and_its_answer() {
    let gated = gated(options(), WireOptions::new());
    let submitted = gated
        .client
        .submit_request("ping", json!({"n": 1}), RequestOptions::new())
        .expect("expected the request queued");
    let id = submitted.id().as_json().clone();
    let bytes = submitted.frame_bytes();
    let (written, reply) = submitted.into_parts();

    within("the request written", written)
        .await
        .expect("expected the request written");
    let frame = gated.link.sent().remove(0);
    assert_eq!(
        bytes,
        frame.len(),
        "expected the reported size to be the frame's: {} | received {bytes}",
        frame.len()
    );
    let parsed: Value = serde_json::from_str(&frame).expect("expected a JSON frame");
    assert_eq!(parsed["id"], id, "received {parsed}");
    gated
        .link
        .push_line(json!({"id": id, "result": "pong"}).to_string());
    let answer = within("the answer", reply)
        .await
        .expect("expected the answer");
    assert_eq!(answer, json!("pong"), "received {answer}");
    assert!(gated.client.state.pending.lock().await.is_empty());
    gated.client.close().await.expect("expected a clean close");
}

/// One frame over the per-frame bound is that call's problem alone: nothing is queued, nothing is
/// written, and the connection carries the next frame.
#[tokio::test]
async fn a_frame_over_the_frame_limit_is_refused_without_touching_the_connection() {
    let exact = r#"{"jsonrpc":"2.0","method":"fits","params":{"pad":"xxxx"}}"#;
    let gated = gated(options(), limits(exact.len(), usize::MAX, usize::MAX));

    let refused = gated
        .client
        .submit_notification("fits", json!({"pad": "xxxxx"}));
    let error = refused.expect_err("expected the frame one byte over refused");
    assert!(
        matches!(
            error.cause(),
            Error::LimitExceeded { subject: FRAME_SUBJECT, limit, received }
                if *limit == exact.len() && *received == exact.len() + 1
        ),
        "expected LimitExceeded {{ {FRAME_SUBJECT}, limit: {}, received: {} }} | received {error:?}",
        exact.len(),
        exact.len() + 1
    );
    assert_eq!(error.dispatch(), Dispatch::NotSubmitted);
    let request = gated.client.submit_request(
        "fits",
        json!({"pad": "a request frame carries an id as well"}),
        ordered_options(),
    );
    assert!(
        matches!(
            request.as_ref().map_err(Error::cause),
            Err(Error::LimitExceeded {
                subject: FRAME_SUBJECT,
                ..
            })
        ),
        "expected the oversized request refused the same way | received {request:?}"
    );
    let places = gated.client.state.ordered_places.available_permits();
    assert_eq!(
        places, 64,
        "expected the refused request's ordered place given back: 64 | received {places}"
    );

    let fits = gated
        .client
        .submit_notification("fits", json!({"pad": "xxxx"}))
        .expect("expected a frame of exactly the limit queued");
    within("the fitting frame written", fits)
        .await
        .expect("expected the fitting frame written");
    assert_eq!(gated.link.sent(), vec![exact.to_owned()]);
    assert!(!gated.client.is_closed());
    assert!(gated.handler.terminations.lock().await.is_empty());
    assert!(gated.client.state.pending.lock().await.is_empty());
    gated.client.close().await.expect("expected a clean close");
}

fn ordered_options() -> RequestOptions {
    RequestOptions::new().after_earlier_notifications()
}

async fn one_termination(handler: &RecordingHandler) -> PeerTermination {
    let terminations = within("one termination", terminations_after(handler, 1))
        .await
        .unwrap_or_else(|seen| panic!("expected terminations: 1 | received {seen}"));
    assert_eq!(
        terminations.len(),
        1,
        "expected exactly one termination | received {terminations:?}"
    );
    terminations[0].clone()
}

/// The queue is full because the peer stopped reading. That ends the connection, and the cause
/// is on record by the time the call that found it full returns.
#[tokio::test]
async fn a_frame_past_the_queued_frame_limit_ends_the_connection() {
    let gated = gated(options(), limits(usize::MAX, usize::MAX, 2));
    let held = link_held(&gated).await;
    let (queued_write, queued_reply) = gated
        .client
        .submit_request("queued", json!({}), RequestOptions::new())
        .expect("expected the second frame queued")
        .into_parts();

    let refused = gated.client.submit_notification("one-too-many", json!({}));
    // Read before anything is awaited: this is what "recorded before the error is returned" is.
    let recorded = gated
        .client
        .state
        .ended
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let error = refused.expect_err("expected the third frame refused");
    assert!(
        matches!(
            error.cause(),
            Error::LimitExceeded {
                subject: QUEUED_FRAMES,
                limit: 2,
                received: 3
            }
        ),
        "expected LimitExceeded {{ {QUEUED_FRAMES}, limit: 2, received: 3 }} | received {error:?}"
    );
    assert_eq!(error.dispatch(), Dispatch::NotSubmitted);
    assert!(
        recorded.is_some_and(
            |failure| failure.body.message == "the ACP agent outbound queue reached its limit"
        ),
        "expected the cause recorded before the refusal returned"
    );

    let termination = one_termination(&gated.handler).await;
    assert_eq!(
        termination,
        PeerTermination::OutboundBackpressure {
            subject: QUEUED_FRAMES,
            limit: 2,
            received: 3
        },
        "expected the handler told which budget and by how much | received {termination:?}"
    );
    assert_eq!(
        termination.to_string(),
        format!("the peer stopped reading: expected at most 2 {QUEUED_FRAMES}, received 3")
    );
    assert!(gated.client.is_closed());

    // The frame that was being written may finish; the one queued behind it never starts.
    gated.gate.add_permits(1);
    within("the held frame", held)
        .await
        .expect("expected the frame already being written to finish");
    let unwritten = within("the queued frame's write", queued_write).await;
    assert!(
        unwritten.is_err(),
        "expected the queued frame refused on the ended connection | received {unwritten:?}"
    );
    let (cause, error) = failure(within("the queued request's reply", queued_reply).await);
    assert_eq!(
        cause,
        CallFailureCause::Ended(ConnectionEnd::Peer(termination.clone())),
        "expected the queued request failed with what ended the connection | received {cause:?}"
    );
    assert_eq!(
        gated.client.ended(),
        Some(ConnectionEnd::Peer(termination)),
        "expected the client to give the same reason"
    );
    assert!(
        matches!(&error, Error::Vendor(vendor) if vendor.message == "the ACP agent outbound queue reached its limit"),
        "expected the error `request` returns for it | received {error:?}"
    );
    let order = methods(&gated.link);
    assert_eq!(
        order,
        vec!["hold"],
        "expected nothing written after the overflow: [hold] | received {order:?}"
    );
    gated.client.close().await.expect("expected a clean close");
}

#[tokio::test]
async fn a_frame_past_the_queued_byte_limit_ends_the_connection() {
    let hold = r#"{"jsonrpc":"2.0","method":"hold","params":{}}"#;
    let next = r#"{"jsonrpc":"2.0","method":"next","params":{}}"#;
    let gated = gated(
        options(),
        limits(usize::MAX, hold.len() + next.len(), usize::MAX),
    );
    let _held = link_held(&gated).await;
    // The frame being written still counts: with it, this one fills the budget exactly.
    let _next = gated
        .client
        .submit_notification("next", json!({}))
        .expect("expected a frame that fills the budget exactly queued");
    let holding = outbox_holds(&gated.client);
    assert_eq!(
        holding,
        (2, hold.len() + next.len()),
        "expected the frame being written counted with the queued one | received {holding:?}"
    );

    let error = gated
        .client
        .submit_notification("next", json!({}))
        .expect_err("expected one frame more refused");
    let limit = hold.len() + next.len();
    assert!(
        matches!(
            error.cause(),
            Error::LimitExceeded { subject: QUEUED_BYTES, limit: at_most, received }
                if *at_most == limit && *received == limit + next.len()
        ),
        "expected LimitExceeded {{ {QUEUED_BYTES}, limit: {limit}, received: {} }} | received {error:?}",
        limit + next.len()
    );
    let termination = one_termination(&gated.handler).await;
    assert!(
        matches!(
            termination,
            PeerTermination::OutboundBackpressure {
                subject: QUEUED_BYTES,
                ..
            }
        ),
        "received {termination:?}"
    );
    let later = gated.client.submit_notification("later", json!({}));
    assert!(
        later.is_err(),
        "expected nothing queued on a connection that has ended | received {later:?}"
    );
    gated.gate.add_permits(1);
    gated.client.close().await.expect("expected a clean close");
}

/// A reply nobody will read is not asked for: the request is taken out of the queue, never
/// reaches the wire, and gives its share of the budget back.
#[tokio::test]
async fn a_request_given_up_while_queued_is_never_written() {
    let gated = gated(options(), WireOptions::new());
    let held = link_held(&gated).await;
    let (written, reply) = gated
        .client
        .submit_request("abandoned", json!({}), ordered_options())
        .expect("expected the request queued")
        .into_parts();
    let after = gated
        .client
        .submit_notification("after", json!({}))
        .expect("expected a frame queued behind it");
    assert_eq!(outbox_holds(&gated.client).0, 3);

    drop(reply);
    assert!(
        !written.started(),
        "expected a withdrawn frame's write never begun: false | received true"
    );
    assert_eq!(
        outbox_holds(&gated.client).0,
        3,
        "expected the withdrawn frame counted for as long as the queue holds it: 3 | received {}",
        outbox_holds(&gated.client).0
    );

    gated.gate.add_permits(1);
    within("the held frame", held)
        .await
        .expect("expected the held frame written");
    within("the frame behind", after)
        .await
        .expect("expected the frame behind written");
    let unwritten = within("the withdrawn frame's write", written).await;
    assert!(
        matches!(unwritten, Err(Error::Link { .. })),
        "expected the withdrawn frame reported unwritten | received {unwritten:?}"
    );
    let order = methods(&gated.link);
    assert_eq!(
        order,
        vec!["hold", "after"],
        "expected the withdrawn request absent from the wire: [hold, after] | received {order:?}"
    );
    assert!(gated.client.state.pending.lock().await.is_empty());
    let places = gated.client.state.ordered_places.available_permits();
    assert_eq!(
        places, 64,
        "expected its ordered place given back: 64 | received {places}"
    );
    let held = outbox_holds(&gated.client);
    assert_eq!(
        held,
        (0, 0),
        "expected nothing held once the writer had passed it: (0, 0) | received {held:?}"
    );
    assert!(!gated.client.is_closed());
    gated.client.close().await.expect("expected a clean close");
}

/// The writer taking a frame and its caller giving the frame up are decided by one transition.
/// Here the caller lets go the instant after the writer won: the frame is written whole, the
/// link stays good, and nothing is left behind for the answer.
#[tokio::test]
async fn a_request_given_up_as_its_write_begins_is_written_whole() {
    let gated = gated(options(), WireOptions::new());
    let (written, reply) = gated
        .client
        .submit_request("racing", json!({}), RequestOptions::new())
        .expect("expected the request queued")
        .into_parts();
    let reply = Arc::new(std::sync::Mutex::new(Some(reply)));
    let giving_up = Arc::clone(&reply);
    *gated
        .client
        .state
        .after_write_began
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(move || {
        let reply: Option<Reply> = giving_up
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        drop(reply);
    }));

    within("the write", written)
        .await
        .expect("expected the frame the writer had taken written");
    assert!(
        reply
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none(),
        "expected the caller to have let go between the writer's take and its send"
    );
    let order = methods(&gated.link);
    assert_eq!(
        order,
        vec!["racing"],
        "expected the frame on the wire once, whole: [racing] | received {order:?}"
    );
    assert!(!gated.client.is_closed());
    assert!(gated.handler.terminations.lock().await.is_empty());
    assert!(gated.client.state.pending.lock().await.is_empty());
    assert_eq!(outbox_holds(&gated.client), (0, 0));
    gated
        .client
        .notify("after", json!({}))
        .await
        .expect("expected a usable link after the race");
    gated.client.close().await.expect("expected a clean close");
}

/// `max_pending_requests` is applied when a queued request's turn comes. One past it is not
/// written: its write says why and its reply fails.
#[tokio::test]
async fn a_queued_request_past_the_pending_budget_is_refused_when_its_turn_comes() {
    let mut one = options();
    one.max_pending_requests = 1;
    let gated = gated(one, WireOptions::new());
    let (first_written, _first_reply) = gated
        .client
        .submit_request("first", json!({}), RequestOptions::new())
        .expect("expected the first request queued")
        .into_parts();
    let (second_written, second_reply) = gated
        .client
        .submit_request("second", json!({}), RequestOptions::new())
        .expect("expected the second request queued")
        .into_parts();

    within("the first write", first_written)
        .await
        .expect("expected the first request written");
    let refused = within("the second write", second_written).await;
    assert!(
        matches!(
            refused.as_ref().map_err(Error::cause),
            Err(Error::LimitExceeded {
                subject: "pending JSON-RPC requests",
                limit: 1,
                received: 2
            })
        ),
        "expected LimitExceeded {{ pending JSON-RPC requests, limit: 1, received: 2 }} | received {refused:?}"
    );
    let (cause, error) = failure(within("the second reply", second_reply).await);
    assert_eq!(
        cause,
        CallFailureCause::Unwritten,
        "expected the reply failed as unwritten | received {cause:?}"
    );
    assert!(
        matches!(&error, Error::Vendor(vendor) if vendor.message == "the ACP agent request frame was not written"),
        "expected the error `request` returns for it | received {error:?}"
    );
    assert_eq!(gated.client.ended(), None);
    let order = methods(&gated.link);
    assert_eq!(order, vec!["first"], "received {order:?}");
    assert!(!gated.client.is_closed());
    gated.client.close().await.expect("expected a clean close");
}

/// A queued frame's write is bounded like any other. The peer that never drains it gets the
/// connection ended, and the caller learns the write began.
#[tokio::test(start_paused = true)]
async fn a_queued_frame_whose_write_stalls_times_out_and_ends_the_connection() {
    let handler = RecordingHandler::arc(None);
    let client = stalled_client(Arc::clone(&handler), "session/prompt");
    let (written, reply) = client
        .submit_request("session/prompt", json!({}), RequestOptions::new())
        .expect("expected the request queued")
        .into_parts();
    let began = tokio::time::Instant::now();

    let outcome = eventually("the stalled write's outcome", written).await;
    assert!(
        matches!(&outcome, Err(Error::Timeout { operation, .. }) if operation == "JSON-RPC frame write"),
        "expected the write to time out | received {outcome:?}"
    );
    let waited = began.elapsed();
    assert_eq!(
        waited,
        Duration::from_secs(5),
        "expected the write given up at the request timeout: 5s | received {waited:?}"
    );
    assert_write_timeout_ended_the_connection(&handler, &client).await;
    let failed = eventually("the reply", reply).await;
    assert!(failed.is_err(), "received {failed:?}");
    client.close().await.expect("expected a clean close");
}

#[tokio::test]
async fn a_queued_frame_the_link_refuses_ends_the_connection_and_reports_its_write_begun() {
    let link = ScriptedLink::new();
    link.fail_sends("EPIPE");
    let handler = RecordingHandler::arc(None);
    let client = client(link.clone(), Arc::clone(&handler));
    let submitted = client
        .submit_request("session/prompt", json!({}), RequestOptions::new())
        .expect("expected the request queued");
    let (written, reply) = submitted.into_parts();

    let mut written = written;
    let outcome = within("the write", &mut written).await;
    assert!(
        matches!(&outcome, Err(Error::Link { message, .. }) if message == "EPIPE"),
        "expected the link's own failure | received {outcome:?}"
    );
    assert!(
        written.started(),
        "expected a write the link refused to count as begun: true | received false"
    );
    let termination = one_termination(&handler).await;
    assert!(
        matches!(&termination, PeerTermination::LinkFailed(cause) if cause.contains("EPIPE")),
        "received {termination:?}"
    );
    // The reply failed with the write, before the connection had ended over it.
    let (cause, _) = failure(within("the reply", reply).await);
    assert_eq!(
        cause,
        CallFailureCause::Unwritten,
        "expected the reply failed with its write | received {cause:?}"
    );
    assert_eq!(client.ended(), Some(ConnectionEnd::Peer(termination)));
    client.close().await.expect("expected a clean close");
}

/// A frame queued before a close is written before the link goes; a request among them finds
/// the connection closed when its turn comes and is failed by this side instead.
#[tokio::test]
async fn frames_queued_before_a_close_are_dealt_with_before_the_link_closes() {
    let gated = gated(options(), WireOptions::new());
    let held = link_held(&gated).await;
    let cancel = gated
        .client
        .submit_notification("session/cancel", json!({}))
        .expect("expected the notification queued");
    let (list_written, list_reply) = gated
        .client
        .submit_request("session/list", json!({}), RequestOptions::new())
        .expect("expected the request queued")
        .into_parts();

    let closing = {
        let client = Arc::clone(&gated.client);
        tokio::spawn(async move { client.close().await })
    };
    until("the close to have begun", || gated.client.is_closed()).await;
    let late = gated.client.submit_notification("late", json!({}));
    assert!(
        matches!(late, Err(Error::Closed { subject: "link" })),
        "expected nothing queued after the close began | received {late:?}"
    );

    gated.gate.add_permits(1);
    within("the held frame", held)
        .await
        .expect("expected the held frame written");
    within("the queued notification", cancel)
        .await
        .expect("expected the notification queued before the close written");
    let refused = within("the queued request's write", list_written).await;
    assert!(
        matches!(refused, Err(Error::Closed { subject: "link" })),
        "expected the queued request refused by the close | received {refused:?}"
    );
    let (cause, error) = failure(within("the queued request's reply", list_reply).await);
    assert_eq!(
        cause,
        CallFailureCause::Ended(ConnectionEnd::Closed),
        "expected the reply failed by the close | received {cause:?}"
    );
    assert_eq!(gated.client.ended(), Some(ConnectionEnd::Closed));
    assert!(
        matches!(&error, Error::Vendor(vendor) if vendor.message == "the ACP agent connection was closed"),
        "expected the error `request` returns for it | received {error:?}"
    );
    within("the close", closing)
        .await
        .expect("expected the close task to finish")
        .expect("expected a clean close");
    let order = methods(&gated.link);
    assert_eq!(
        order,
        vec!["hold", "session/cancel"],
        "expected what was queued before the close on the wire: [hold, session/cancel] | received {order:?}"
    );
}

/// A reply can outlive the client that queued its request. Dropping the client fails it, written
/// or not, instead of leaving it to its deadline.
#[tokio::test(start_paused = true)]
async fn dropping_the_client_fails_replies_still_waiting() {
    let gated = gated(
        ClientOptions::new("ACP agent").with_request_timeout(Duration::from_secs(600)),
        WireOptions::new(),
    );
    let (asked_written, asked_reply) = gated
        .client
        .submit_request("asked", json!({}), RequestOptions::new())
        .expect("expected the request queued")
        .into_parts();
    within("the first request written", asked_written)
        .await
        .expect("expected the first request written");
    let _held = link_held(&gated).await;
    let (queued_written, queued_reply) = gated
        .client
        .submit_request("queued", json!({}), RequestOptions::new())
        .expect("expected the second request queued")
        .into_parts();
    let began = tokio::time::Instant::now();

    let Gated { client, .. } = gated;
    drop(Arc::into_inner(client).expect("expected the test to own the client"));

    let (cause, error) = failure(eventually("the written request's reply", asked_reply).await);
    assert_eq!(
        cause,
        CallFailureCause::Ended(ConnectionEnd::Closed),
        "expected the written request failed by the drop | received {cause:?}"
    );
    assert!(
        matches!(&error, Error::Vendor(vendor) if vendor.message == "the ACP agent connection was closed"),
        "expected the error `request` returns for it | received {error:?}"
    );
    let (cause, _) = failure(eventually("the queued request's reply", queued_reply).await);
    assert_eq!(
        cause,
        CallFailureCause::Ended(ConnectionEnd::Closed),
        "expected the queued request failed by the drop | received {cause:?}"
    );
    let unwritten = eventually("the queued request's write", queued_written).await;
    assert!(unwritten.is_err(), "received {unwritten:?}");
    let waited = began.elapsed();
    assert_eq!(
        waited,
        Duration::ZERO,
        "expected no wait for the 600s deadline: 0s | received {waited:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_submitted_request_times_out_from_when_it_was_queued() {
    let gated = gated(options(), WireOptions::new());
    let (written, reply) = gated
        .client
        .submit_request(
            "session/list",
            json!({}),
            RequestOptions::new().with_timeout(Duration::from_secs(2)),
        )
        .expect("expected the request queued")
        .into_parts();
    let began = tokio::time::Instant::now();
    eventually("the write", written)
        .await
        .expect("expected the request written");

    let (cause, error) = failure(eventually("the reply", reply).await);
    assert!(
        matches!(&cause, CallFailureCause::TimedOut { after } if *after == Duration::from_secs(2)),
        "expected the reply to time out: TimedOut after 2s | received {cause:?}"
    );
    assert!(
        matches!(&error, Error::Timeout { after, .. } if *after == Duration::from_secs(2)),
        "expected the error `request` returns for it: Timeout after 2s | received {error:?}"
    );
    assert_eq!(gated.client.ended(), None);
    assert_eq!(began.elapsed(), Duration::from_secs(2));
    // The reply is gone, and with it the call's entry. The drop may have had to defer that.
    until("the abandoned call cleaned up", || {
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

#[tokio::test]
async fn a_submitted_request_can_ask_for_its_answer_after_earlier_notifications() {
    let link = ScriptedLink::new();
    let handler = RecordingHandler::arc(None);
    let client = client(link.clone(), Arc::clone(&handler));
    let (written, reply) = client
        .submit_request("session/prompt", json!({}), ordered_options())
        .expect("expected the request queued")
        .into_parts();
    within("the request written", written)
        .await
        .expect("expected the request written");
    link.push_line(r#"{"jsonrpc":"2.0","method":"session/update","params":{"n":1}}"#);
    link.push_line(r#"{"jsonrpc":"2.0","id":"1","result":"done"}"#);

    let answer = within("the answer", reply)
        .await
        .expect("expected the answer");
    assert_eq!(answer, json!("done"), "received {answer}");
    let seen = handler.notifications.lock().await.len();
    assert_eq!(
        seen, 1,
        "expected the notification handled before the ordered answer returned: 1 | received {seen}"
    );
    client.close().await.expect("expected a clean close");
}

#[tokio::test]
async fn the_inbound_terminations_say_what_was_received() {
    let frame = String::from(r#"{"method":"delta","params":{"text":"payload"}}"#);
    let (queue, _worker_end) = tokio::sync::mpsc::channel(8);

    let small = Client::connect(
        ScriptedLink::new().into_link(),
        RecordingHandler::arc(None),
        ClientOptions {
            max_pending_bytes: frame.len(),
            ..ClientOptions::default()
        },
    );
    let longer = format!("{frame} ");
    let over_bytes = crate::jsonrpc::dispatch(&small.state, &queue, longer.clone()).await;
    assert_eq!(
        over_bytes,
        Err(PeerTermination::NotificationByteBackpressure {
            limit: frame.len(),
            received: longer.len()
        }),
        "expected the size of the frame that did not fit"
    );
    small.close().await.expect("close");

    let narrow = Client::connect(
        ScriptedLink::new().into_link(),
        RecordingHandler::arc(None),
        ClientOptions::default().with_max_pending_notifications(1),
    );
    crate::jsonrpc::dispatch(&narrow.state, &queue, frame.clone())
        .await
        .expect("expected the first frame queued");
    let over_count = crate::jsonrpc::dispatch(&narrow.state, &queue, frame).await;
    assert_eq!(
        over_count,
        Err(PeerTermination::NotificationBackpressure {
            limit: 1,
            received: 2
        }),
        "expected the count with the message that did not fit"
    );
    narrow.close().await.expect("close");
}

/// Many callers at once, on several threads, some queueing and some waiting for their own
/// writes. Whatever the interleaving, each caller's frames are on the wire in the order it sent
/// them, every frame is there exactly once, and nothing is left held.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_callers_each_keep_their_own_order_on_the_wire() {
    const CALLERS: usize = 8;
    const ROUNDS: usize = 50;
    let link = ScriptedLink::new();
    let handler = RecordingHandler::arc(None);
    let client = Arc::new(Client::connect(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options(),
    ));

    let mut callers = Vec::new();
    for caller in 0..CALLERS {
        let client = Arc::clone(&client);
        callers.push(tokio::spawn(async move {
            let mut queued = Vec::new();
            for round in 0..ROUNDS {
                let tag = |step: usize| json!({"caller": caller, "seq": round * 3 + step});
                queued.push(
                    client
                        .submit_notification("queued", tag(0))
                        .expect("expected the frame queued"),
                );
                client
                    .notify("waited", tag(1))
                    .await
                    .expect("expected the waiting caller's frame written");
                queued.push(
                    client
                        .submit_notification("queued", tag(2))
                        .expect("expected the frame queued"),
                );
                if round % 7 == 0 {
                    tokio::task::yield_now().await;
                }
            }
            for written in queued {
                written.await.expect("expected every queued frame written");
            }
        }));
    }
    for caller in callers {
        within("a caller", caller)
            .await
            .expect("expected the caller's task to finish");
    }

    let mut next = [0_u64; CALLERS];
    let frames = link.sent();
    assert_eq!(
        frames.len(),
        CALLERS * ROUNDS * 3,
        "expected every frame on the wire exactly once"
    );
    for frame in &frames {
        let frame: Value = serde_json::from_str(frame).expect("expected a whole JSON frame");
        let caller = frame["params"]["caller"].as_u64().expect("a caller") as usize;
        let seq = frame["params"]["seq"].as_u64().expect("a sequence number");
        assert_eq!(
            seq, next[caller],
            "expected caller {caller}'s frames in the order it sent them: {} | received {seq}",
            next[caller]
        );
        next[caller] += 1;
    }
    assert_eq!(outbox_holds(&client), (0, 0));
    assert!(!client.is_closed());
    client.close().await.expect("expected a clean close");
}

/// Callers queue requests and give them up at once while the writer, on another thread, takes
/// them. Each request is either on the wire whole, with its write reported, or absent, with its
/// write reported failed; never both, never neither, and the link survives all of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_given_up_under_the_writer_are_written_whole_or_not_at_all() {
    const CALLERS: usize = 4;
    const ROUNDS: usize = 200;
    let link = ScriptedLink::new();
    let handler = RecordingHandler::arc(None);
    let mut roomy = options();
    roomy.max_pending_requests = usize::MAX;
    let client = Arc::new(Client::connect(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        roomy,
    ));

    let mut callers = Vec::new();
    for caller in 0..CALLERS {
        let client = Arc::clone(&client);
        callers.push(tokio::spawn(async move {
            let mut outcomes = Vec::new();
            for round in 0..ROUNDS {
                let (written, reply) = client
                    .submit_request(
                        "racing",
                        json!({"caller": caller, "round": round}),
                        RequestOptions::new(),
                    )
                    .expect("expected the request queued")
                    .into_parts();
                if round % 3 == 0 {
                    tokio::task::yield_now().await;
                }
                drop(reply);
                // Read after the caller let go: from here the frame's fate is decided, and this
                // must already say which way.
                let began = written.started();
                let outcome = written.await;
                outcomes.push((round, began, outcome.is_ok()));
            }
            (caller, outcomes)
        }));
    }
    let mut reported = std::collections::HashSet::new();
    for caller in callers {
        let (caller, outcomes) = within("a caller", caller)
            .await
            .expect("expected the caller's task to finish");
        for (round, began, written) in outcomes {
            assert_eq!(
                began, written,
                "expected `started` after the caller let go to say whether the frame was written: caller {caller} round {round}"
            );
            if written {
                reported.insert((caller as u64, round as u64));
            }
        }
    }
    let on_wire: std::collections::HashSet<(u64, u64)> = link
        .sent()
        .iter()
        .map(|frame| {
            let frame: Value = serde_json::from_str(frame).expect("expected a whole JSON frame");
            (
                frame["params"]["caller"].as_u64().expect("a caller"),
                frame["params"]["round"].as_u64().expect("a round"),
            )
        })
        .collect();
    assert_eq!(
        link.sent().len(),
        on_wire.len(),
        "expected no frame on the wire twice"
    );
    assert_eq!(
        on_wire, reported,
        "expected the wire to hold exactly the frames whose write was reported"
    );
    assert_eq!(outbox_holds(&client), (0, 0));
    assert!(!client.is_closed());
    assert!(handler.terminations.lock().await.is_empty());
    client.close().await.expect("expected a clean close");
}

/// A queued frame that waited out its whole deadline before the writer could begin it has put
/// nothing on the wire. It fails alone, as timed out, and the link carries the next frame.
#[tokio::test(start_paused = true)]
async fn a_queued_frame_that_expires_before_its_write_begins_fails_alone() {
    let gated = gated(options(), WireOptions::new());
    // The writer has to enter a request among the pending calls before writing it, and waits
    // here for that map: the one way a frame at the head of the queue can be kept from starting.
    let map = gated.client.state.pending.lock().await;
    let (written, reply) = gated
        .client
        .submit_request(
            "late",
            json!({}),
            RequestOptions::new().with_timeout(Duration::from_secs(60)),
        )
        .expect("expected the request queued")
        .into_parts();
    tokio::time::sleep(Duration::from_secs(6)).await;
    drop(map);

    let outcome = eventually("the expired frame's write", written).await;
    assert!(
        matches!(&outcome, Err(Error::Timeout { operation, .. }) if operation == "JSON-RPC frame write"),
        "expected the frame timed out unwritten: Timeout(JSON-RPC frame write) | received {outcome:?}"
    );
    let (cause, error) = failure(eventually("the expired request's reply", reply).await);
    assert_eq!(
        cause,
        CallFailureCause::Unwritten,
        "expected the reply failed as unwritten | received {cause:?}"
    );
    assert!(
        matches!(&error, Error::Vendor(vendor) if vendor.message == "the ACP agent request frame was not written"),
        "expected the error `request` returns for it | received {error:?}"
    );
    assert!(
        gated.link.sent().is_empty(),
        "expected nothing written: [] | received {:?}",
        gated.link.sent()
    );
    gated
        .client
        .notify("after", json!({}))
        .await
        .expect("expected the link still usable");
    assert!(!gated.client.is_closed());
    assert!(gated.handler.terminations.lock().await.is_empty());
    gated.client.close().await.expect("expected a clean close");
}

/// Queueing is synchronous so that code with no runtime around it can do it: a plain thread
/// holding a lock, say. The reply's timer is made when the reply is first waited on.
#[tokio::test]
async fn a_request_can_be_queued_from_a_thread_outside_the_runtime() {
    let gated = gated(options(), WireOptions::new());
    let client = Arc::clone(&gated.client);
    let submitted = std::thread::spawn(move || {
        client.submit_request("from-a-thread", json!({}), RequestOptions::new())
    })
    .join()
    .expect("expected queueing off the runtime not to panic")
    .expect("expected the request queued");
    let id = submitted.id().as_json().clone();
    let (written, reply) = submitted.into_parts();

    within("the write", written)
        .await
        .expect("expected the request written");
    gated
        .link
        .push_line(json!({"id": id, "result": "pong"}).to_string());
    let answer = within("the answer", reply)
        .await
        .expect("expected the answer");
    assert_eq!(answer, json!("pong"), "received {answer}");
    gated.client.close().await.expect("expected a clean close");
}

/// The per-frame bound holds for a caller that waits for its own write too, and refuses it the
/// same way: that call only, nothing written.
#[tokio::test]
async fn a_waiting_callers_frame_over_the_frame_limit_is_refused_alone() {
    let exact = r#"{"jsonrpc":"2.0","method":"fits","params":{"pad":"xxxx"}}"#;
    let gated = gated(options(), limits(exact.len(), usize::MAX, usize::MAX));

    let error = gated
        .client
        .notify("fits", json!({"pad": "xxxxx"}))
        .await
        .expect_err("expected the frame one byte over refused");
    assert!(
        matches!(
            error.cause(),
            Error::LimitExceeded { subject: FRAME_SUBJECT, limit, received }
                if *limit == exact.len() && *received == exact.len() + 1
        ),
        "expected LimitExceeded {{ {FRAME_SUBJECT}, limit: {}, received: {} }} | received {error:?}",
        exact.len(),
        exact.len() + 1
    );
    assert_eq!(error.dispatch(), Dispatch::NotSubmitted);
    let request = gated
        .client
        .request::<_, Value>(
            "fits",
            json!({"pad": "a request frame carries an id as well"}),
        )
        .await;
    assert!(
        matches!(
            request.as_ref().map_err(Error::cause),
            Err(Error::LimitExceeded {
                subject: FRAME_SUBJECT,
                ..
            })
        ),
        "expected the oversized request refused the same way | received {request:?}"
    );
    assert!(gated.client.state.pending.lock().await.is_empty());

    gated
        .client
        .notify("fits", json!({"pad": "xxxx"}))
        .await
        .expect("expected a frame of exactly the limit written");
    assert_eq!(gated.link.sent(), vec![exact.to_owned()]);
    assert!(!gated.client.is_closed());
    assert!(gated.handler.terminations.lock().await.is_empty());
    gated.client.close().await.expect("expected a clean close");
}

/// A close that runs out of its grace behind a write the peer never reads has not closed the
/// link. What was queued behind that write must not be written afterwards.
#[tokio::test(start_paused = true)]
async fn a_close_that_times_out_leaves_no_queued_frame_to_be_written() {
    let grace = Duration::from_millis(200);
    let gated = gated(options(), WireOptions::new());
    let held = link_held(&gated).await;
    let queued = gated
        .client
        .submit_notification("queued", json!({}))
        .expect("expected a frame queued behind the held one");

    let closed = gated.client.close().await;
    assert!(
        matches!(&closed, Err(Error::Timeout { operation, after }) if operation == "JSON-RPC link shutdown" && *after == grace),
        "expected the close to run out of its grace: Timeout(JSON-RPC link shutdown, 200ms) | received {closed:?}"
    );

    gated.gate.add_permits(1);
    eventually("the held frame", held)
        .await
        .expect("expected the write already in progress to finish");
    let unwritten = eventually("the queued frame", queued).await;
    assert!(
        matches!(unwritten, Err(Error::Link { .. })),
        "expected the queued frame refused after the close gave up | received {unwritten:?}"
    );
    let order = methods(&gated.link);
    assert_eq!(
        order,
        vec!["hold"],
        "expected nothing written after the close: [hold] | received {order:?}"
    );
}

/// The budget is passed by whichever thread queues the frame too many, and several can find it
/// passed at once. The connection ends once, with the typed reason, and nobody queues after.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_overflow_found_by_several_threads_ends_the_connection_once() {
    const CALLERS: usize = 8;
    const LIMIT: usize = 4;
    let gated = gated(options(), limits(usize::MAX, usize::MAX, LIMIT));
    let held = link_held(&gated).await;

    let mut callers = Vec::new();
    for caller in 0..CALLERS {
        let client = Arc::clone(&gated.client);
        callers.push(tokio::spawn(async move {
            let mut refused = 0;
            let mut admitted = Vec::new();
            for round in 0..LIMIT {
                match client.submit_notification("burst", json!({"caller": caller, "round": round})) {
                    Ok(written) => admitted.push(written),
                    Err(error) => {
                        assert!(
                            matches!(
                                error.cause(),
                                Error::LimitExceeded { subject: QUEUED_FRAMES, limit: LIMIT, .. }
                                    | Error::Link { .. }
                                    | Error::Closed { .. }
                            ),
                            "expected the overflow, or the ended connection it leaves | received {error:?}"
                        );
                        refused += 1;
                    }
                }
            }
            (refused, admitted)
        }));
    }
    let mut refused = 0;
    let mut admitted = Vec::new();
    for caller in callers {
        let (refusals, queued) = within("a caller", caller)
            .await
            .expect("expected the caller's task to finish");
        refused += refusals;
        admitted.extend(queued);
    }
    // The held frame and three more fit; every other one of the 32 was refused.
    assert_eq!(
        refused,
        CALLERS * LIMIT - (LIMIT - 1),
        "expected all but {} frames refused | received {refused}",
        LIMIT - 1
    );
    let termination = one_termination(&gated.handler).await;
    assert_eq!(
        termination,
        PeerTermination::OutboundBackpressure {
            subject: QUEUED_FRAMES,
            limit: LIMIT,
            received: LIMIT + 1
        },
        "received {termination:?}"
    );
    gated.gate.add_permits(1);
    within("the held frame", held)
        .await
        .expect("expected the frame already being written to finish");
    for queued in admitted {
        let unwritten = within("a frame queued before the overflow", queued).await;
        assert!(
            unwritten.is_err(),
            "expected a frame still queued at the overflow never written | received {unwritten:?}"
        );
    }
    let order = methods(&gated.link);
    assert_eq!(
        order,
        vec!["hold"],
        "expected nothing written after the overflow: [hold] | received {order:?}"
    );
    gated.client.close().await.expect("expected a clean close");
}

/// An answer too large for the per-frame bound cannot be refused back to the handler that
/// already returned it. The peer is told its question failed, and the connection goes on.
#[tokio::test]
async fn an_answer_over_the_frame_limit_is_replaced_by_an_internal_error_reply() {
    let link = ScriptedLink::new();
    let handler = RecordingHandler::arc(Some(ServerRequestOutcome::Answer(
        json!({"text": "x".repeat(200)}),
    )));
    let client = Client::connect_with(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options(),
        limits(120, usize::MAX, usize::MAX),
    );

    link.push_line(r#"{"jsonrpc":"2.0","id":7,"method":"fs/read_text_file"}"#);
    sent(&link, 1).await;
    let reply: Value = serde_json::from_str(&link.sent()[0]).expect("expected a JSON reply");
    assert_eq!(
        reply,
        json!({"jsonrpc": "2.0", "id": 7, "error": {"code": -32603, "message": "the answer was too large to send"}}),
        "expected the question answered with an internal error in place of the oversized answer | received {reply}"
    );
    assert!(!client.is_closed());
    assert!(handler.terminations.lock().await.is_empty());
    client
        .notify("after", json!({}))
        .await
        .expect("expected the link still usable");
    client.close().await.expect("expected a clean close");
}

/// The queue's bounds are on what the queue holds. A caller that waits for its own write holds
/// its own frame, so only the per-frame bound applies to it.
#[tokio::test]
async fn a_waiting_callers_frame_is_not_measured_against_the_queue_bounds() {
    let gated = gated(options(), limits(usize::MAX, 16, 1));
    gated
        .client
        .notify("wider-than-the-queue", json!({"pad": "xxxxxxxxxxxxxxxx"}))
        .await
        .expect("expected a waiting caller's frame written whatever the queue's bounds");
    assert_eq!(gated.link.sent().len(), 1);
    assert!(!gated.client.is_closed());
    gated.client.close().await.expect("expected a clean close");
}

fn spawn_notify(gated: &Gated, method: &'static str) -> tokio::task::JoinHandle<crate::Result<()>> {
    let client = Arc::clone(&gated.client);
    tokio::spawn(async move { client.notify(method, json!({})).await })
}

/// Puts the connection on its queue for good, with nothing left in it.
async fn queueing_begun(gated: &Gated) {
    let first = gated
        .client
        .submit_notification("begin", json!({}))
        .expect("expected the first frame queued");
    within("the first queued frame", first)
        .await
        .expect("expected the first queued frame written");
}

/// Whether a caller holds the link and nobody is still on the way to it. A caller the lock was
/// handed to stays counted until its task runs again, so the lock alone says too little.
fn link_in_use(gated: &Gated) -> bool {
    gated.client.state.outbox.direct().0 == 0 && gated.client.state.sender.try_lock().is_err()
}

/// A caller that waits for its own write, behind a link in use, stays counted as queued until the
/// outbox holds the link for it. Counted only until the outbox took its entry, a later caller
/// could find the queue looking empty and the link free, and write ahead of it.
async fn a_queued_turn_stays_counted_until_the_link_is_held_for_it() {
    let gated = gated(options(), WireOptions::new());
    queueing_begun(&gated).await;
    let first = spawn_notify(&gated, "hold");
    // The first caller found the link free and holds it inside the gated send.
    until("the first caller to hold the link", || link_in_use(&gated)).await;
    let taken = gated.client.state.outbox.taken();
    let second = spawn_notify(&gated, "second");
    // The outbox has taken the second caller's entry off the queue and waits for the link.
    until("the outbox to reach the queued turn", || {
        gated.client.state.outbox.taken() > taken
    })
    .await;
    let owed = gated.client.state.outbox.turns();
    assert_eq!(
        owed, 1,
        "expected the turn counted after the outbox took its entry, while the link is in use: 1 | received {owed}"
    );

    gated.gate.add_permits(1);
    within("the first caller", first)
        .await
        .expect("expected the first task to finish")
        .expect("expected the first frame written");
    within("the second caller", second)
        .await
        .expect("expected the second task to finish")
        .expect("expected the second frame written");
    let owed = gated.client.state.outbox.turns();
    assert_eq!(
        owed, 0,
        "expected no turn owed once it was served: 0 | received {owed}"
    );
    let order = methods(&gated.link);
    assert_eq!(order, vec!["begin", "hold", "second"], "received {order:?}");
    gated.client.close().await.expect("expected a clean close");
}

#[tokio::test]
async fn a_queued_turn_stays_counted_until_the_link_is_held_for_it_on_one_thread() {
    a_queued_turn_stays_counted_until_the_link_is_held_for_it().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_turn_stays_counted_until_the_link_is_held_for_it_on_four_threads() {
    a_queued_turn_stays_counted_until_the_link_is_held_for_it().await;
}

/// A connection that never queues a frame never involves the queue: callers that wait for their
/// own writes wait on the link's lock and nothing else, contended or not. One that gives up
/// waiting, by its deadline or by being dropped, leaves nothing behind and the next goes on.
#[tokio::test(start_paused = true)]
async fn callers_that_all_wait_for_their_writes_share_the_link_without_the_queue() {
    let gated = gated(options(), WireOptions::new());
    let outbox = &gated.client.state.outbox;
    let first = spawn_notify(&gated, "hold");
    until("the first caller to hold the link", || link_in_use(&gated)).await;

    let timed_out = {
        let client = Arc::clone(&gated.client);
        tokio::spawn(async move {
            let brief = RequestOptions::new().with_timeout(Duration::from_secs(1));
            client
                .request_with::<_, Value>("timed-out", json!({}), brief)
                .await
        })
    };
    let dropped = spawn_notify(&gated, "dropped");
    until("both callers waiting on the link", || {
        outbox.direct().0 == 2
    })
    .await;
    dropped.abort();
    let cancelled = eventually("the dropped caller", dropped).await;
    assert!(
        matches!(&cancelled, Err(error) if error.is_cancelled()),
        "expected the dropped caller's task cancelled | received {cancelled:?}"
    );
    let waiting = outbox.direct();
    assert_eq!(
        waiting,
        (1, false),
        "expected the dropped caller no longer counted: (1, false) | received {waiting:?}"
    );
    // The other waits out its whole deadline behind the write that has not finished.
    let outcome = eventually("the caller that timed out", timed_out)
        .await
        .expect("expected the caller's task to finish");
    assert!(
        matches!(&outcome, Err(Error::Timeout { after, .. }) if *after == Duration::from_secs(1)),
        "expected the waiting caller timed out: Timeout after 1s | received {outcome:?}"
    );
    let waiting = outbox.direct();
    assert_eq!(
        waiting,
        (0, false),
        "expected nobody counted as waiting once both gave up: (0, false) | received {waiting:?}"
    );
    gated.gate.add_permits(1);
    eventually("the first caller", first)
        .await
        .expect("expected the first task to finish")
        .expect("expected the first caller's frame written");
    gated
        .client
        .notify("next", json!({}))
        .await
        .expect("expected the next caller served");
    let order = methods(&gated.link);
    assert_eq!(
        order,
        vec!["hold", "next"],
        "expected only the callers that stayed on the wire: [hold, next] | received {order:?}"
    );
    let untouched = (outbox.taken(), outbox.turns(), outbox.held());
    assert_eq!(
        untouched,
        (0, 0, (0, 0)),
        "expected the queue never used: (0, 0, (0, 0)) | received {untouched:?}"
    );
}

/// The same contention without anybody running out of time: the callers are served in the order
/// they asked for the lock, and the queue's task takes nothing.
#[tokio::test]
async fn contended_waiting_callers_write_in_the_order_they_asked() {
    let gated = gated(options(), WireOptions::new());
    let outbox = &gated.client.state.outbox;
    let first = spawn_notify(&gated, "hold");
    until("the first caller to hold the link", || link_in_use(&gated)).await;
    let second = spawn_notify(&gated, "second");
    until("the second caller waiting", || outbox.direct().0 == 1).await;
    let gone = spawn_notify(&gated, "gone");
    until("the third caller waiting", || outbox.direct().0 == 2).await;
    let fourth = spawn_notify(&gated, "fourth");
    until("the fourth caller waiting", || outbox.direct().0 == 3).await;
    gone.abort();
    let _ = within("the dropped caller", gone).await;

    gated.gate.add_permits(1);
    for (name, caller) in [("first", first), ("second", second), ("fourth", fourth)] {
        within(name, caller)
            .await
            .expect("expected the caller's task to finish")
            .expect("expected the caller's frame written");
    }
    let order = methods(&gated.link);
    assert_eq!(
        order,
        vec!["hold", "second", "fourth"],
        "expected the callers served in the order they asked, without the one that left: [hold, second, fourth] | received {order:?}"
    );
    let untouched = (outbox.taken(), outbox.direct());
    assert_eq!(
        untouched,
        (0, (0, false)),
        "expected the queue never used: (0, (0, false)) | received {untouched:?}"
    );
    gated.client.close().await.expect("expected a clean close");
}

/// The first frame queued on a connection goes behind the callers already waiting on the link,
/// and ahead of every caller that comes after it.
async fn the_first_queued_frame_goes_behind_callers_already_waiting() {
    let gated = gated(options(), WireOptions::new());
    let outbox = &gated.client.state.outbox;
    let first = spawn_notify(&gated, "hold");
    until("the first caller to hold the link", || link_in_use(&gated)).await;
    let second = spawn_notify(&gated, "second");
    until("the second caller waiting", || outbox.direct().0 == 1).await;
    let gone = spawn_notify(&gated, "gone");
    until("the third caller waiting", || outbox.direct().0 == 2).await;

    let third = gated
        .client
        .submit_notification("third", json!({}))
        .expect("expected the first queued frame");
    let fourth = spawn_notify(&gated, "fourth");
    until("the later caller's turn queued", || outbox.turns() == 1).await;
    gone.abort();
    let _ = within("the dropped caller", gone).await;

    gated.gate.add_permits(1);
    for (name, caller) in [("first", first), ("second", second)] {
        within(name, caller)
            .await
            .expect("expected the caller's task to finish")
            .expect("expected the caller's frame written");
    }
    within("the queued frame", third)
        .await
        .expect("expected the queued frame written");
    within("the later caller", fourth)
        .await
        .expect("expected the caller's task to finish")
        .expect("expected the later caller's frame written");
    let order = methods(&gated.link);
    assert_eq!(
        order,
        vec!["hold", "second", "third", "fourth"],
        "expected call order across the first queued frame: [hold, second, third, fourth] | received {order:?}"
    );
    let left = (outbox.direct().0, outbox.turns(), outbox.held());
    assert_eq!(
        left,
        (0, 0, (0, 0)),
        "expected nothing counted afterwards: (0, 0, (0, 0)) | received {left:?}"
    );
    gated.client.close().await.expect("expected a clean close");
}

#[tokio::test]
async fn the_first_queued_frame_goes_behind_callers_already_waiting_on_one_thread() {
    the_first_queued_frame_goes_behind_callers_already_waiting().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_first_queued_frame_goes_behind_callers_already_waiting_on_four_threads() {
    the_first_queued_frame_goes_behind_callers_already_waiting().await;
}

/// Sets what runs the next time a waiting caller has been told how it gets the link.
fn when_a_turn_is_decided(gated: &Gated, hook: impl FnOnce() + Send + 'static) {
    *gated
        .client
        .state
        .after_turn_decided
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(hook));
}

/// The narrowest version of that: a caller has been sent to the link's lock and has not asked
/// for it yet when the first frame is queued, on another thread free to run. The queue's task
/// stops for the caller, which called first, and does not take the idle link from under it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_frame_queued_as_a_caller_goes_for_the_lock_waits_for_that_caller() {
    let gated = gated(options(), WireOptions::new());
    let stopped_for_the_caller = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let queued = Arc::new(std::sync::Mutex::new(None));
    {
        let client = Arc::clone(&gated.client);
        let stopped = Arc::clone(&stopped_for_the_caller);
        let queued = Arc::clone(&queued);
        when_a_turn_is_decided(&gated, move || {
            let written = client.submit_notification("queued", json!({}));
            *queued.lock().unwrap_or_else(PoisonError::into_inner) = Some(written);
            // The caller's own thread stands still here; the queue's task runs on another.
            let began = std::time::Instant::now();
            while client.state.outbox.waited_for_direct() == 0 {
                if began.elapsed() > Duration::from_secs(5) {
                    return;
                }
                std::thread::yield_now();
            }
            stopped.store(true, std::sync::atomic::Ordering::Release);
        });
    }

    gated
        .client
        .notify("waited", json!({}))
        .await
        .expect("expected the waiting caller's frame written");
    assert!(
        stopped_for_the_caller.load(std::sync::atomic::Ordering::Acquire),
        "expected the queue's task to stop for the caller on its way to the lock: stopped | received it went on within 5s"
    );
    let written = queued
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
        .expect("expected the hook to have run")
        .expect("expected the frame queued");
    within("the queued frame", written)
        .await
        .expect("expected the queued frame written");
    let order = methods(&gated.link);
    assert_eq!(
        order,
        vec!["waited", "queued"],
        "expected the caller that asked first on the wire first: [waited, queued] | received {order:?}"
    );
    gated.client.close().await.expect("expected a clean close");
}

/// On a connection that queues, a caller handed the free link there and then has it: a frame
/// queued the instant after goes behind it, and the queue's task waits for the link.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_frame_queued_as_a_caller_is_handed_the_free_link_goes_behind_it() {
    let gated = gated(options(), WireOptions::new());
    queueing_begun(&gated).await;
    let reached = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let queued = Arc::new(std::sync::Mutex::new(None));
    {
        let client = Arc::clone(&gated.client);
        let reached = Arc::clone(&reached);
        let queued = Arc::clone(&queued);
        when_a_turn_is_decided(&gated, move || {
            let taken = client.state.outbox.taken();
            let written = client.submit_notification("queued", json!({}));
            *queued.lock().unwrap_or_else(PoisonError::into_inner) = Some(written);
            // Stand still until the queue's task, on another thread, has the frame in hand.
            let began = std::time::Instant::now();
            while client.state.outbox.taken() == taken {
                if began.elapsed() > Duration::from_secs(5) {
                    return;
                }
                std::thread::yield_now();
            }
            reached.store(true, std::sync::atomic::Ordering::Release);
        });
    }

    gated
        .client
        .notify("waited", json!({}))
        .await
        .expect("expected the waiting caller's frame written");
    assert!(
        reached.load(std::sync::atomic::Ordering::Acquire),
        "expected the queue's task to take the frame while the caller held the link: taken | received not within 5s"
    );
    let written = queued
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
        .expect("expected the hook to have run")
        .expect("expected the frame queued");
    within("the queued frame", written)
        .await
        .expect("expected the queued frame written");
    let order = methods(&gated.link);
    assert_eq!(
        order,
        vec!["begin", "waited", "queued"],
        "expected the caller handed the link on the wire first: [begin, waited, queued] | received {order:?}"
    );
    gated.client.close().await.expect("expected a clean close");
}

/// A caller dropped while its turn is still in the queue, behind a frame being written, leaves
/// no turn owed: the outbox passes over it and the next caller goes on.
async fn a_caller_dropped_while_its_turn_is_queued_leaves_no_turn_owed() {
    let gated = gated(options(), WireOptions::new());
    let outbox = &gated.client.state.outbox;
    let held = link_held(&gated).await;
    let dropped = spawn_notify(&gated, "dropped");
    until("the caller's turn queued", || outbox.turns() == 1).await;
    dropped.abort();
    let _ = within("the dropped caller", dropped).await;
    let next = spawn_notify(&gated, "next");
    until("the next caller's turn queued", || outbox.turns() == 2).await;

    gated.gate.add_permits(1);
    within("the held frame", held)
        .await
        .expect("expected the held frame written");
    within("the next caller", next)
        .await
        .expect("expected the caller's task to finish")
        .expect("expected the next caller's frame written");
    let owed = outbox.turns();
    assert_eq!(
        owed, 0,
        "expected no turn owed after a caller left the queue: 0 | received {owed}"
    );
    let order = methods(&gated.link);
    assert_eq!(
        order,
        vec!["hold", "next"],
        "expected the dropped caller absent from the wire: [hold, next] | received {order:?}"
    );
    gated.client.close().await.expect("expected a clean close");
}

#[tokio::test]
async fn a_caller_dropped_while_its_turn_is_queued_leaves_no_turn_owed_on_one_thread() {
    a_caller_dropped_while_its_turn_is_queued_leaves_no_turn_owed().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_caller_dropped_while_its_turn_is_queued_leaves_no_turn_owed_on_four_threads() {
    a_caller_dropped_while_its_turn_is_queued_leaves_no_turn_owed().await;
}

/// A caller dropped after the outbox reached its turn, while the outbox waits for a link another
/// caller is writing on, is not waited for: its turn is over before the link comes free.
async fn a_caller_dropped_while_the_outbox_waits_on_the_link_for_it_is_passed_over() {
    let gated = gated(options(), WireOptions::new());
    let outbox = &gated.client.state.outbox;
    queueing_begun(&gated).await;
    let first = spawn_notify(&gated, "hold");
    until("the first caller to hold the link", || link_in_use(&gated)).await;
    let taken = outbox.taken();
    let dropped = spawn_notify(&gated, "dropped");
    until("the outbox to reach the queued turn", || {
        outbox.taken() > taken
    })
    .await;
    dropped.abort();
    let _ = within("the dropped caller", dropped).await;

    // The link is still in use: nothing has been released.
    until("the dropped caller's turn given up", || outbox.turns() == 0).await;
    assert!(
        link_in_use(&gated),
        "expected the turn given up while the link was still in use: in use | received free"
    );
    let next = gated
        .client
        .submit_notification("next", json!({}))
        .expect("expected a frame queued behind the abandoned turn");
    gated.gate.add_permits(1);
    within("the first caller", first)
        .await
        .expect("expected the first task to finish")
        .expect("expected the first frame written");
    within("the next frame", next)
        .await
        .expect("expected the next frame written");
    let order = methods(&gated.link);
    assert_eq!(
        order,
        vec!["begin", "hold", "next"],
        "expected the dropped caller absent from the wire: [begin, hold, next] | received {order:?}"
    );
    gated.client.close().await.expect("expected a clean close");
}

#[tokio::test]
async fn a_caller_dropped_while_the_outbox_waits_on_the_link_for_it_is_passed_over_on_one_thread() {
    a_caller_dropped_while_the_outbox_waits_on_the_link_for_it_is_passed_over().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_caller_dropped_while_the_outbox_waits_on_the_link_for_it_is_passed_over_on_four_threads()
{
    a_caller_dropped_while_the_outbox_waits_on_the_link_for_it_is_passed_over().await;
}

/// A close whose caller stops waiting has not closed the link. What was queued behind the write
/// it was waiting on must not be written afterwards.
async fn a_close_dropped_before_it_finishes_leaves_no_queued_frame_to_be_written() {
    let gated = gated(options(), WireOptions::new());
    let held = link_held(&gated).await;
    let queued = gated
        .client
        .submit_notification("queued", json!({}))
        .expect("expected a frame queued behind the held one");

    let closing = {
        let client = Arc::clone(&gated.client);
        tokio::spawn(async move { client.close().await })
    };
    until("the close to have begun", || gated.client.is_closed()).await;
    closing.abort();
    let abandoned = within("the abandoned close", closing).await;
    assert!(
        matches!(&abandoned, Err(error) if error.is_cancelled()),
        "expected the close cut off before it finished | received {abandoned:?}"
    );

    gated.gate.add_permits(1);
    within("the held frame", held)
        .await
        .expect("expected the write already in progress to finish");
    let unwritten = within("the queued frame", queued).await;
    assert!(
        matches!(unwritten, Err(Error::Link { .. })),
        "expected the queued frame refused after the close was abandoned: Err(Link) | received {unwritten:?}"
    );
    let order = methods(&gated.link);
    assert_eq!(
        order,
        vec!["hold"],
        "expected nothing written after the abandoned close: [hold] | received {order:?}"
    );
}

#[tokio::test]
async fn a_close_dropped_before_it_finishes_leaves_no_queued_frame_to_be_written_on_one_thread() {
    a_close_dropped_before_it_finishes_leaves_no_queued_frame_to_be_written().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_close_dropped_before_it_finishes_leaves_no_queued_frame_to_be_written_on_four_threads() {
    a_close_dropped_before_it_finishes_leaves_no_queued_frame_to_be_written().await;
}

/// A queue that may hold nothing refuses every frame the way it refuses one too large for it:
/// that call only. Nothing about the peer made the frame not fit, so the connection goes on,
/// for callers that wait for their own writes.
#[tokio::test]
async fn a_queue_bounded_to_nothing_refuses_each_frame_alone() {
    for (bounds, subject) in [
        (limits(usize::MAX, usize::MAX, 0), QUEUED_FRAMES),
        (limits(usize::MAX, 0, usize::MAX), FRAME_SUBJECT),
    ] {
        let gated = gated(options(), bounds);
        let notification = gated.client.submit_notification("queued", json!({}));
        let request = gated
            .client
            .submit_request("asked", json!({}), ordered_options());
        for refused in [notification.map(drop), request.map(drop)] {
            let error = refused.expect_err("expected the frame refused");
            assert!(
                matches!(
                    error.cause(),
                    Error::LimitExceeded { subject: counted, limit: 0, received }
                        if *counted == subject && *received > 0
                ),
                "expected LimitExceeded {{ {subject}, limit: 0 }} | received {error:?}"
            );
            assert_eq!(
                error.dispatch(),
                Dispatch::NotSubmitted,
                "expected the refused frame reported as never submitted | received {:?}",
                error.dispatch()
            );
        }
        let state = (gated.client.is_closed(), gated.client.ended());
        assert_eq!(
            state,
            (false, None),
            "expected the connection untouched by a bound of nothing: (false, None) | received {state:?}"
        );
        let places = gated.client.state.ordered_places.available_permits();
        assert_eq!(
            places, 64,
            "expected the refused request's ordered place given back: 64 | received {places}"
        );
        gated
            .client
            .notify("waited", json!({}))
            .await
            .expect("expected a caller that waits for its write still served");
        let switched = gated.client.state.outbox.direct();
        assert_eq!(
            switched,
            (0, false),
            "expected a refused frame not to start the queue: (0, false) | received {switched:?}"
        );
        assert_eq!(methods(&gated.link), vec!["waited"]);
        assert!(gated.handler.terminations.lock().await.is_empty());
        gated.client.close().await.expect("expected a clean close");
    }
}

/// The reply that stands in for an answer too large carries the peer's own id. A peer whose id
/// alone passes the bound cannot be answered in any form: the connection ends, naming the bound.
#[tokio::test]
async fn an_id_too_large_for_any_reply_ends_the_connection() {
    let link = ScriptedLink::new();
    let handler = RecordingHandler::arc(Some(ServerRequestOutcome::Answer(json!("ok"))));
    let client = Client::connect_with(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options(),
        limits(120, usize::MAX, usize::MAX),
    );

    let id = "i".repeat(200);
    link.push_line(json!({"jsonrpc": "2.0", "id": id, "method": "fs/read_text_file"}).to_string());
    let termination = one_termination(&handler).await;
    assert!(
        matches!(&termination, PeerTermination::LinkFailed(cause) if cause.contains(FRAME_SUBJECT) && cause.contains("120")),
        "expected the link failed naming the bound: LinkFailed(.. {FRAME_SUBJECT} .. 120 ..) | received {termination:?}"
    );
    assert!(
        link.sent().is_empty(),
        "expected nothing written: [] | received {:?}",
        link.sent()
    );
    assert!(client.is_closed());
    assert_eq!(client.ended(), Some(ConnectionEnd::Peer(termination)));
    client.close().await.expect("expected a clean close");
}

/// A failed reply says which side failed it. The peer's own error arrives as the peer sent it,
/// whatever its code, and never stands for something this side did.
#[tokio::test]
async fn a_reply_the_peer_refused_carries_the_peers_error_and_no_end() {
    let gated = gated(options(), WireOptions::new());
    let submitted = gated
        .client
        .submit_request("session/new", json!({}), RequestOptions::new())
        .expect("expected the request queued");
    let id = submitted.id().as_json().clone();
    let (written, reply) = submitted.into_parts();
    within("the request written", written)
        .await
        .expect("expected the request written");
    gated.link.push_line(
        json!({"id": id, "error": {"code": -32000, "message": "auth required", "data": {"a": 1}}})
            .to_string(),
    );

    let (cause, error) = failure(within("the reply", reply).await);
    assert_eq!(
        cause,
        CallFailureCause::Peer(JsonRpcError {
            code: -32000,
            message: String::from("auth required"),
            data: Some(json!({"a": 1})),
        }),
        "expected the peer's error as it sent it | received {cause:?}"
    );
    assert!(
        matches!(&error, Error::Vendor(vendor) if vendor.message == "auth required"),
        "expected the error `request` returns for it | received {error:?}"
    );
    assert_eq!(gated.client.ended(), None);

    // A peer that exits fails the next one from this side, with the same code in the error
    // `request` returns and a cause that tells the two apart.
    let (written, reply) = gated
        .client
        .submit_request("session/prompt", json!({}), RequestOptions::new())
        .expect("expected the second request queued")
        .into_parts();
    within("the second request written", written)
        .await
        .expect("expected the second request written");
    gated.link.end();
    let (cause, error) = failure(within("the second reply", reply).await);
    assert_eq!(
        cause,
        CallFailureCause::Ended(ConnectionEnd::Peer(PeerTermination::Exited)),
        "expected the reply failed by the peer's exit | received {cause:?}"
    );
    assert!(
        matches!(&error, Error::Vendor(vendor) if vendor.message == "the ACP agent exited"),
        "expected the error `request` returns for it | received {error:?}"
    );
    assert_eq!(
        gated.client.ended(),
        Some(ConnectionEnd::Peer(PeerTermination::Exited))
    );
    gated.client.close().await.expect("expected a clean close");
}

/// Two things can end a connection at nearly the same moment, and the first on record is kept.
/// The handler is told that one, so it, the client and every failed reply name the same reason.
#[tokio::test]
async fn the_handler_is_told_the_reason_on_record_when_two_causes_race() {
    let gated = gated(options(), WireOptions::new());
    let (written, reply) = gated
        .client
        .submit_request("asked", json!({}), RequestOptions::new())
        .expect("expected the request queued")
        .into_parts();
    within("the request written", written)
        .await
        .expect("expected the request written");
    // A caller on another thread passed the outbound budget and put that on record; the pump,
    // already past its wait, finds the peer gone.
    let recorded = PeerTermination::OutboundBackpressure {
        subject: QUEUED_FRAMES,
        limit: 2,
        received: 3,
    };
    gated.client.state.ended_with(
        ConnectionEnd::Peer(recorded.clone()),
        gated.client.state.closed_error(),
    );
    gated.link.end();

    let told = one_termination(&gated.handler).await;
    assert_eq!(
        told, recorded,
        "expected the handler told the reason on record: {recorded:?} | received {told:?}"
    );
    let (cause, _) = failure(within("the reply", reply).await);
    assert_eq!(
        cause,
        CallFailureCause::Ended(ConnectionEnd::Peer(recorded.clone())),
        "expected the reply failed with the same reason | received {cause:?}"
    );
    let ended = gated.client.ended();
    assert_eq!(
        ended,
        Some(ConnectionEnd::Peer(recorded)),
        "expected the client to give the same reason | received {ended:?}"
    );
    gated.client.close().await.expect("expected a clean close");
}

/// A reply whose frame went down with the queue's task says the connection ended only when that
/// is on record. Otherwise its frame was simply not written.
#[tokio::test]
async fn a_reply_lost_with_the_queue_task_claims_no_end_that_is_not_on_record() {
    let gated = gated(options(), WireOptions::new());
    // The queue's task waits here for the map before it writes a request: stopped at that
    // point it has put nothing on the wire and has nothing to report.
    let map = gated.client.state.pending.lock().await;
    let (written, reply) = gated
        .client
        .submit_request("lost", json!({}), RequestOptions::new())
        .expect("expected the request queued")
        .into_parts();
    until("the queue's task to take the request", || {
        gated.client.state.outbox.taken() == 1
    })
    .await;
    gated
        .client
        .state
        .writer_abort
        .get()
        .expect("expected the queue's task to be running")
        .abort();

    let (cause, _) = failure(within("the reply", reply).await);
    let ended = gated.client.ended();
    assert_eq!(
        (&cause, &ended),
        (&CallFailureCause::Unwritten, &None),
        "expected the reply failed as unwritten with no end on record: (Unwritten, None) | received ({cause:?}, {ended:?})"
    );
    let unwritten = within("the write", written).await;
    assert!(
        unwritten.is_err(),
        "expected the write reported failed: Err | received {unwritten:?}"
    );
    drop(map);
    assert!(
        gated.link.sent().is_empty(),
        "expected nothing written: [] | received {:?}",
        gated.link.sent()
    );
    gated.client.close().await.expect("expected a clean close");
}

/// Only the first close answers for the link. A second one, given up while the first is still
/// writing what was queued before it, does not take the link from under the first.
#[tokio::test]
async fn a_second_close_given_up_does_not_stop_the_first_from_flushing() {
    let gated = gated(options(), WireOptions::new());
    let held = link_held(&gated).await;
    let queued = gated
        .client
        .submit_notification("queued", json!({}))
        .expect("expected a frame queued behind the held one");
    let closing = {
        let client = Arc::clone(&gated.client);
        tokio::spawn(async move { client.close().await })
    };
    until("the first close to have begun", || {
        gated
            .client
            .state
            .close_begun
            .load(std::sync::atomic::Ordering::Acquire)
    })
    .await;

    {
        // Polled once, so it is inside the close, and then let go.
        let mut second = Box::pin(gated.client.close());
        let finished =
            std::future::poll_fn(|cx| std::task::Poll::Ready(second.as_mut().poll(cx).is_ready()))
                .await;
        assert!(
            !finished,
            "expected the second close still waiting behind the held frame: pending | received finished"
        );
    }

    gated.gate.add_permits(1);
    within("the held frame", held)
        .await
        .expect("expected the held frame written");
    within("the queued frame", queued)
        .await
        .expect("expected the frame queued before the first close written");
    within("the first close", closing)
        .await
        .expect("expected the close task to finish")
        .expect("expected a clean close");
    let order = methods(&gated.link);
    assert_eq!(
        order,
        vec!["hold", "queued"],
        "expected what was queued before the first close on the wire: [hold, queued] | received {order:?}"
    );
}
