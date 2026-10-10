//! Frames queued without waiting: their order on the wire, their bounds, and what becomes of
//! them when their caller, the link or the client goes away.

use super::*;
use crate::Dispatch;
use crate::jsonrpc::outbox::{Refusal, admission};
use crate::jsonrpc::{Reply, WireOptions, Written};
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

/// Waits until the writer is inside the gated send, which is when the link is held.
async fn link_held(gated: &Gated) -> Written {
    let held = gated
        .client
        .submit_notification("hold", json!({}))
        .expect("expected the held frame queued");
    let mut yields = 0;
    while !held.started() {
        yields += 1;
        assert!(
            yields < 10_000,
            "expected the writer to take the held frame: started | received still queued after {yields} yields"
        );
        tokio::task::yield_now().await;
    }
    held
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
    let mut yields = 0;
    while !second.is_finished() && gated.client.state.outbox.turns() == 0 {
        yields += 1;
        assert!(
            yields < 10_000,
            "expected the waiting caller's turn queued: 1 | received 0 after {yields} yields"
        );
        tokio::task::yield_now().await;
    }
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
            |failure| failure.message == "the ACP agent outbound queue reached its limit"
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
    let failed = within("the queued request's reply", queued_reply).await;
    assert!(
        matches!(&failed, Err(Error::Vendor(vendor)) if vendor.message == "the ACP agent outbound queue reached its limit"),
        "expected the queued request failed with what ended the connection | received {failed:?}"
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
    let failed = within("the second reply", second_reply).await;
    assert!(
        matches!(&failed, Err(Error::Vendor(vendor)) if vendor.message == "the ACP agent request frame was not written"),
        "expected the reply failed as unwritten | received {failed:?}"
    );
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
    let failed = within("the reply", reply).await;
    assert!(failed.is_err(), "received {failed:?}");
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
    let mut yields = 0;
    while !gated.client.is_closed() {
        yields += 1;
        assert!(yields < 10_000, "expected the close to have begun");
        tokio::task::yield_now().await;
    }
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
    let failed = within("the queued request's reply", list_reply).await;
    assert!(
        matches!(&failed, Err(Error::Vendor(vendor)) if vendor.message == "the ACP agent connection was closed"),
        "expected the reply failed by the close | received {failed:?}"
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

    let asked = eventually("the written request's reply", asked_reply).await;
    assert!(
        matches!(&asked, Err(Error::Vendor(vendor)) if vendor.message == "the ACP agent connection was closed"),
        "expected the written request failed by the drop | received {asked:?}"
    );
    let queued = eventually("the queued request's reply", queued_reply).await;
    assert!(queued.is_err(), "received {queued:?}");
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

    let outcome = eventually("the reply", reply).await;
    assert!(
        matches!(&outcome, Err(Error::Timeout { after, .. }) if *after == Duration::from_secs(2)),
        "expected the reply to time out: Timeout after 2s | received {outcome:?}"
    );
    assert_eq!(began.elapsed(), Duration::from_secs(2));
    // The reply is gone, and with it the call's entry. The drop may have had to defer that.
    let mut yields = 0;
    while !gated.client.state.pending.lock().await.is_empty() {
        yields += 1;
        assert!(yields < 10_000, "expected the abandoned call cleaned up");
        tokio::task::yield_now().await;
    }
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

/// A caller that waits for its own write, behind a link in use, stays counted as queued until the
/// outbox holds the link for it. Counted only until the outbox took its entry, a later caller
/// could find the queue looking empty and the link free, and write ahead of it.
#[tokio::test]
async fn a_queued_turn_stays_counted_until_the_link_is_held_for_it() {
    let gated = gated(options(), WireOptions::new());
    let first = {
        let client = Arc::clone(&gated.client);
        tokio::spawn(async move { client.notify("hold", json!({})).await })
    };
    // The first caller found the link free and holds it inside the gated send.
    let mut yields = 0;
    while gated.client.state.sender.try_lock().is_ok() {
        yields += 1;
        assert!(
            yields < 10_000,
            "expected the first caller to hold the link"
        );
        tokio::task::yield_now().await;
    }
    let second = {
        let client = Arc::clone(&gated.client);
        tokio::spawn(async move { client.notify("second", json!({})).await })
    };
    let mut yields = 0;
    while gated.client.state.outbox.turns() == 0 {
        yields += 1;
        assert!(yields < 10_000, "expected the second caller's turn queued");
        tokio::task::yield_now().await;
    }
    // Enough turns of the scheduler for the outbox to have taken the entry and parked on the
    // link. The turn is still owed, so it is still counted.
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    let owed = gated.client.state.outbox.turns();
    assert_eq!(
        owed, 1,
        "expected the waiting caller's turn counted while the link is in use: 1 | received {owed}"
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
    assert_eq!(order, vec!["hold", "second"], "received {order:?}");
    gated.client.close().await.expect("expected a clean close");
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
    let failed = eventually("the expired request's reply", reply).await;
    assert!(
        matches!(&failed, Err(Error::Vendor(vendor)) if vendor.message == "the ACP agent request frame was not written"),
        "expected the reply failed as unwritten | received {failed:?}"
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
