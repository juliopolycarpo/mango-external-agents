//! What dropping a `Client` does to its reader while a close that is kept, and not polled again,
//! holds the lock on the reader's handle.
//!
//! A close borrows the client it is called on, so a client cannot be dropped while a close future
//! is alive unless that future owns a share of it; the reader's lifetime is then bounded by the
//! future's. The one way to drop the client under a kept close is to leak the future, which
//! `Drop` has to answer without the lock.

use super::abandoned_close_tests::{Closing, park, peer_exited_with_the_handler_held};
use super::outbox_tests::until;
use super::*;
use std::sync::atomic::Ordering;

/// A close kept inside an owned future holds a share of the client, so dropping every other share
/// leaves the client and its reader alone: the handler's call is still running. Dropping the kept
/// future takes the reader down through the close's own guard, and the client with it.
#[tokio::test(start_paused = true)]
async fn a_kept_close_holds_the_client_and_the_reader_until_it_is_dropped() {
    let (client, handler) = peer_exited_with_the_handler_held().await;
    let client = Arc::new(client);
    let mut first: Closing = {
        let client = Arc::clone(&client);
        Box::pin(async move { client.close().await })
    };
    park("the first close", &mut first).await;

    drop(client);
    tokio::time::sleep(Duration::from_millis(10)).await;
    let while_kept = (
        handler.entered.load(Ordering::Acquire),
        handler.cut.load(Ordering::Acquire),
    );
    assert_eq!(
        while_kept,
        (1, 0),
        "expected the handler's call still running while the kept close holds the client: (1, 0) | received {while_kept:?} (entered, cut)"
    );

    drop(first);
    until(
        "the handler's call to be cut once the kept close was dropped",
        || handler.cut.load(Ordering::Acquire) == 1,
    )
    .await;
    assert_eq!(
        handler.returned.load(Ordering::Acquire),
        0,
        "expected the cut call not to have returned | received it returned"
    );
}

/// A close future that is leaked, polled to where it holds the lock on the reader's handle and
/// never dropped, does not keep the reader alive after the client is dropped: `Drop` stops the
/// reader without that lock.
#[tokio::test(start_paused = true)]
async fn dropping_the_client_stops_the_reader_a_leaked_close_holds() {
    let (client, handler) = peer_exited_with_the_handler_held().await;
    let mut first: Closing = Box::pin(client.close());
    park("the first close", &mut first).await;
    assert!(
        client.pump.try_lock().is_err(),
        "expected the kept close to hold the lock on the reader's handle | received it free"
    );

    std::mem::forget(first);
    drop(client);
    until(
        "the handler's call to be cut after the client was dropped under a leaked close",
        || handler.cut.load(Ordering::Acquire) == 1,
    )
    .await;
}
