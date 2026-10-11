//! A close given up after it recorded its end still owns the end, and a second close finishes it.

use super::outbox_tests::{until, within};
use super::*;
use crate::jsonrpc::{ConnectionEnd, WireOptions};

/// Whether the link still takes a write, asked of the sender itself.
async fn link_takes_a_write(client: &Client) -> bool {
    client
        .state
        .sender
        .lock()
        .await
        .send(String::from("probe"))
        .await
        .is_ok()
}

/// A close whose future is dropped while it fails the calls still pending has recorded its end
/// and gone no further: the handler is told nothing, the call is still pending and the link is
/// still open. A second close fails the call and closes the link.
#[tokio::test]
async fn a_close_dropped_before_it_failed_the_pending_calls_is_finished_by_a_second() {
    let link = ScriptedLink::new();
    let handler = RecordingHandler::arc(None);
    let client = Arc::new(super::client(link.clone(), Arc::clone(&handler)));
    let asking = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.request::<_, Value>("ask", json!({})).await })
    };
    within("the request on the wire", link.wait_for_sent(1)).await;

    // The map is held, so the close parks where it fails the pending calls.
    let map = client.state.pending.lock().await;
    let first = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.close().await })
    };
    until("the first close to have recorded its end", || {
        client.is_closed()
    })
    .await;
    first.abort();
    let abandoned = within("the abandoned close", first).await;
    let cut = matches!(&abandoned, Err(error) if error.is_cancelled());
    let still_open = link_takes_a_write(&client).await;
    let still_pending = !asking.is_finished();
    let told_before = handler.terminations.lock().await.len();
    drop(map);

    let second = within("the second close", client.close()).await;
    let answer = within("the pending call's answer", asking)
        .await
        .expect("expected the request task to finish");
    let observed = (
        (cut, still_open, still_pending, told_before),
        second.is_ok(),
        answer.is_err(),
        link_takes_a_write(&client).await,
        handler.terminations.lock().await.len(),
        client.ended(),
    );
    assert_eq!(
        observed,
        (
            (true, true, true, 0),
            true,
            true,
            false,
            0,
            Some(ConnectionEnd::Closed)
        ),
        "expected the first close cut with the link open and the call pending, then a second to fail the call, close the link and tell the handler nothing: (((true, true, true, 0), true, true, false, 0, Some(Closed))) | received {observed:?} with the answer {answer:?}"
    );
}

/// A close dropped while it waits behind a write the peer has not taken has not reached the link:
/// nothing queued behind that write is written afterwards. A second close, once the write is
/// done, closes the link.
#[tokio::test]
async fn a_close_dropped_behind_a_held_write_is_finished_by_a_second() {
    let gated = super::outbox_tests::gated(super::outbox_tests::options(), WireOptions::new());
    let held = super::outbox_tests::link_held(&gated).await;
    let _queued = gated
        .client
        .submit_notification("queued", json!({}))
        .expect("expected a frame queued behind the held one");
    let first = {
        let client = Arc::clone(&gated.client);
        tokio::spawn(async move { client.close().await })
    };
    until("the first close to have recorded its end", || {
        gated.client.is_closed()
    })
    .await;
    first.abort();
    let _ = within("the abandoned close", first).await;
    gated.gate.add_permits(1);
    within("the held frame", held)
        .await
        .expect("expected the write already in progress to finish");
    let second = within("the second close", gated.client.close()).await;
    let observed = (second.is_ok(), link_takes_a_write(&gated.client).await);
    assert_eq!(
        observed,
        (true, false),
        "expected the second close to finish and to leave the link closed: (true, false) | received {observed:?}"
    );
}
