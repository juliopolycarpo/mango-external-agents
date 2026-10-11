//! Which outbound bound an error or an end names, and which timeout is a close giving up, told
//! by type from the values the client really produces.

use super::outbox_tests::{eventually, gated, limits, link_held, one_termination, options, within};
use super::*;
use crate::Dispatch;
use crate::jsonrpc::{OutboundBound, WireOptions, is_link_shutdown_timeout};

/// What an error says of the outbound bounds and of a close giving up.
fn told(error: &Error) -> (Option<OutboundBound>, bool) {
    (
        OutboundBound::exceeded_by(error),
        is_link_shutdown_timeout(error),
    )
}

/// The text each bound is named by is the text it has always been named by, and reads back as
/// that bound. A subject that is not one of them names none.
#[test]
fn each_bound_is_named_by_its_own_subject_and_read_back_from_it() {
    let observed: Vec<(&str, Option<OutboundBound>)> = OutboundBound::ALL
        .into_iter()
        .map(|bound| {
            (
                bound.subject(),
                OutboundBound::from_subject(bound.subject()),
            )
        })
        .collect();
    let expected = vec![
        (
            "bytes of one outgoing JSON-RPC frame",
            Some(OutboundBound::FrameBytes),
        ),
        (
            "bytes of outgoing JSON-RPC frames awaiting their write",
            Some(OutboundBound::QueuedBytes),
        ),
        (
            "outgoing JSON-RPC frames awaiting their write",
            Some(OutboundBound::QueuedFrames),
        ),
    ];
    assert_eq!(
        observed, expected,
        "expected each bound under its own subject: {expected:?} | received {observed:?}"
    );
    let other = OutboundBound::from_subject("bytes of something else");
    assert_eq!(
        other, None,
        "expected no bound for a subject that is none of them: None | received {other:?}"
    );
}

/// A frame larger than one frame may be is refused where it is submitted, and the refusal
/// names the frame bound, as it was returned and without its dispatch annotation. Nothing
/// ended, so the handler is told of no end.
#[tokio::test]
async fn a_refused_frame_names_the_frame_bound() {
    let exact = r#"{"jsonrpc":"2.0","method":"fits","params":{"pad":"xxxx"}}"#;
    let gated = gated(options(), limits(exact.len(), usize::MAX, usize::MAX));

    let error = gated
        .client
        .submit_notification("fits", json!({"pad": "xxxxx"}))
        .expect_err("expected the frame one byte over refused");
    let observed = (
        told(&error),
        told(error.cause()),
        error.dispatch(),
        gated.client.is_closed(),
    );
    let expected = (
        (Some(OutboundBound::FrameBytes), false),
        (Some(OutboundBound::FrameBytes), false),
        Dispatch::NotSubmitted,
        false,
    );
    assert_eq!(
        observed, expected,
        "expected the frame bound named by the refusal, annotated or not, and the connection open: {expected:?} | received {observed:?}"
    );
    gated.client.close().await.expect("expected a clean close");
}

/// A caller that waits for its own write is refused the same way, and the refusal names the
/// same bound.
#[tokio::test]
async fn a_waiting_callers_refused_frame_names_the_frame_bound() {
    let exact = r#"{"jsonrpc":"2.0","method":"fits","params":{"pad":"xxxx"}}"#;
    let gated = gated(options(), limits(exact.len(), usize::MAX, usize::MAX));

    let error = gated
        .client
        .notify("fits", json!({"pad": "xxxxx"}))
        .await
        .expect_err("expected the frame one byte over refused");
    let observed = (told(&error), gated.client.is_closed());
    let expected = ((Some(OutboundBound::FrameBytes), false), false);
    assert_eq!(
        observed, expected,
        "expected the frame bound named for a waiting caller, the connection open: {expected:?} | received {observed:?} ({error:?})"
    );
    gated.client.close().await.expect("expected a clean close");
}

/// A queue that may hold no frame at all refuses each frame alone, and names the bound on
/// queued frames for it: the bound is named, and nothing ends. Whether the connection ended is
/// for `Client::ended` to say.
#[tokio::test]
async fn a_queue_bounded_to_no_frames_names_the_bound_and_ends_nothing() {
    let gated = gated(options(), limits(usize::MAX, usize::MAX, 0));

    let error = gated
        .client
        .submit_notification("queued", json!({}))
        .expect_err("expected the frame refused");
    let observed = (told(&error), gated.client.is_closed(), gated.client.ended());
    let expected = ((Some(OutboundBound::QueuedFrames), false), false, None);
    assert_eq!(
        observed, expected,
        "expected the bound on queued frames named and the connection untouched: {expected:?} | received {observed:?} ({error:?})"
    );
    gated.client.close().await.expect("expected a clean close");
}

/// A queue that holds too many frames ends the connection. The refusal the caller receives
/// and the end the handler is told of both name the bound on queued frames.
#[tokio::test]
async fn a_full_queue_names_the_bound_on_queued_frames() {
    let gated = gated(options(), limits(usize::MAX, usize::MAX, 2));
    let _held = link_held(&gated).await;
    let _queued = gated
        .client
        .submit_notification("queued", json!({}))
        .expect("expected the second frame queued");

    let error = gated
        .client
        .submit_notification("one-too-many", json!({}))
        .expect_err("expected the third frame refused");
    let termination = one_termination(&gated.handler).await;
    let observed = (told(&error), termination.outbound_bound());
    let expected = (
        (Some(OutboundBound::QueuedFrames), false),
        Some(OutboundBound::QueuedFrames),
    );
    assert_eq!(
        observed, expected,
        "expected the bound on queued frames named by the refusal and by the end: {expected:?} | received {observed:?} ({termination:?})"
    );
}

/// The same for a queue that holds too many bytes: the bound on queued bytes, not the one on
/// frames and not the one on a single frame.
#[tokio::test]
async fn a_queue_past_its_bytes_names_the_bound_on_queued_bytes() {
    let hold = r#"{"jsonrpc":"2.0","method":"hold","params":{}}"#;
    let next = r#"{"jsonrpc":"2.0","method":"next","params":{}}"#;
    let gated = gated(
        options(),
        limits(usize::MAX, hold.len() + next.len(), usize::MAX),
    );
    let _held = link_held(&gated).await;
    let _next = gated
        .client
        .submit_notification("next", json!({}))
        .expect("expected a frame that fills the budget exactly queued");

    let error = gated
        .client
        .submit_notification("next", json!({}))
        .expect_err("expected one frame more refused");
    let termination = one_termination(&gated.handler).await;
    let observed = (told(&error), termination.outbound_bound());
    let expected = (
        (Some(OutboundBound::QueuedBytes), false),
        Some(OutboundBound::QueuedBytes),
    );
    assert_eq!(
        observed, expected,
        "expected the bound on queued bytes named by the refusal and by the end: {expected:?} | received {observed:?} ({termination:?})"
    );
}

/// A close that runs out of its grace behind a write the peer never takes is told as the
/// link shutdown giving up, with or without a dispatch annotation, and names no bound.
#[tokio::test(start_paused = true)]
async fn a_close_that_gives_up_is_told_as_the_link_shutdown_timing_out() {
    let gated = gated(options(), WireOptions::new());
    let held = link_held(&gated).await;

    let error = eventually("the close", gated.client.close())
        .await
        .expect_err("expected the close to run out of its grace");
    let annotated = error.clone().with_dispatch(Dispatch::Accepted);
    let observed = (told(&error), told(&annotated));
    let expected = ((None, true), (None, true));
    assert_eq!(
        observed, expected,
        "expected a link shutdown that timed out, annotated or not, naming no bound: {expected:?} | received {observed:?} ({error:?})"
    );

    gated.gate.add_permits(1);
    eventually("the held frame", held)
        .await
        .expect("expected the write already in progress to finish");
}

/// The other timeouts are not a close giving up: a request the peer never answers, and a
/// frame the peer never takes.
#[tokio::test(start_paused = true)]
async fn a_request_or_a_write_that_times_out_is_not_a_link_shutdown() {
    let gated = gated(options(), WireOptions::new());
    let unanswered = eventually(
        "the unanswered request",
        gated.client.request_with::<_, Value>(
            "session/new",
            json!({}),
            RequestOptions::new().with_timeout(Duration::from_secs(1)),
        ),
    )
    .await
    .expect_err("expected the request to time out");

    let handler = RecordingHandler::arc(None);
    let stalled = stalled_client(Arc::clone(&handler), "session/prompt");
    // Without a deadline of its own, so that only the write can run out of time.
    let unwritten = eventually(
        "the stalled write",
        stalled.request_with::<_, Value>(
            "session/prompt",
            json!({}),
            RequestOptions::new().without_deadline(),
        ),
    )
    .await
    .expect_err("expected the write to time out");

    let observed = (
        matches!(unanswered.cause(), Error::Timeout { operation, .. } if operation == "a JSON-RPC request"),
        told(&unanswered),
        matches!(unwritten.cause(), Error::Timeout { operation, .. } if operation == "JSON-RPC frame write"),
        told(&unwritten),
    );
    let expected = (true, (None, false), true, (None, false));
    assert_eq!(
        observed, expected,
        "expected a request that timed out and a write that timed out, neither a link shutdown and neither naming a bound: {expected:?} | received {observed:?} ({unanswered:?}, {unwritten:?})"
    );
}

/// A limit that is not one of the outbound bounds names none: this client's own count of the
/// requests awaiting an answer, and a limit some other layer reports in the same error.
#[tokio::test]
async fn a_limit_that_is_not_an_outbound_bound_names_none() {
    let mut one_at_a_time = options();
    one_at_a_time.max_pending_requests = 1;
    let gated = gated(one_at_a_time, WireOptions::new());
    let (written, _reply) = gated
        .client
        .submit_request("session/new", json!({}), RequestOptions::new())
        .expect("expected the first request queued")
        .into_parts();
    within("the first request written", written)
        .await
        .expect("expected the first request written");
    let error = within(
        "the second request refused",
        gated
            .client
            .request_with::<_, Value>("session/new", json!({}), RequestOptions::new()),
    )
    .await
    .expect_err("expected the second request refused");
    let elsewhere = Error::LimitExceeded {
        subject: "frames queued for the ACP agent",
        limit: 1,
        received: 2,
    };

    let observed = (
        matches!(error.cause(), Error::LimitExceeded { .. }),
        told(&error),
        told(&elsewhere),
        told(&Error::Closed { subject: "link" }),
    );
    let expected = (true, (None, false), (None, false), (None, false));
    assert_eq!(
        observed, expected,
        "expected a limit on pending requests, another layer's limit and a closed link to name no bound: {expected:?} | received {observed:?} ({error:?})"
    );
}

/// An end that is not this side holding too much names no bound: a peer that exits, and a
/// peer that sends more than the handler can keep up with.
#[tokio::test]
async fn an_end_that_is_not_outbound_backpressure_names_no_bound() {
    let gated = gated(options(), WireOptions::new());
    gated.link.end();
    let exited = one_termination(&gated.handler).await;
    let flooded = PeerTermination::NotificationBackpressure {
        limit: 1,
        received: 2,
    };
    let failed = PeerTermination::LinkFailed(String::from(
        "outgoing JSON-RPC frames awaiting their write",
    ));

    let observed = (
        exited.clone(),
        exited.outbound_bound(),
        flooded.outbound_bound(),
        failed.outbound_bound(),
    );
    let expected = (PeerTermination::Exited, None, None, None);
    assert_eq!(
        observed, expected,
        "expected a peer that exited, one that flooded and a failed link to name no bound: {expected:?} | received {observed:?}"
    );
    let _ = within("the close", gated.client.close()).await;
}
