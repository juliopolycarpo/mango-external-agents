//! A close given up after it recorded its end still owns the end, and a second close finishes it.
//!
//! The first two tests and the one in `cancellation_hold_tests` are characterisation tests: they
//! pin what a close given up leaves behind, they were not written to fail first. Where they ask
//! whether the link is still open they ask the sender itself, which is the child's input and
//! bypasses the client's own refusal of writes; what a caller can do is asked of the client.
//! The last two cover a second close made while the first is held and never polled again. The
//! first of those failed before the wait for the reader was bounded; the second pins behaviour.

use super::outbox_tests::{gated, link_held, methods, options, until, within};
use super::*;
use crate::jsonrpc::{ConnectionEnd, WireOptions, is_link_shutdown_timeout};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Semaphore;

/// A close that is held: polled to wherever it parks, and then kept without being polled again.
type Closing<'a> = Pin<Box<dyn Future<Output = crate::Result<()>> + 'a>>;

/// The `shutdown_timeout` every client here runs under.
fn shutdown() -> Duration {
    options().shutdown_timeout
}

/// Runs `close` until it parks, and no further, then leaves it where it is.
///
/// On a paused clock a sleep ends only once the runtime has nothing else to run, so a close that
/// is still pending then is parked on what it waits for and not on a step it could still take.
/// The sleep is shorter than anything the close waits out, so the close has not timed out.
async fn park(what: &str, close: &mut Closing<'_>) {
    tokio::select! {
        biased;
        finished = close.as_mut() => {
            panic!("expected {what} parked | received finished with {finished:?}");
        }
        () = tokio::time::sleep(shutdown() / 4) => {}
    }
}

/// Whether the link still takes a write, asked of the sender itself. That is the child's input,
/// not what a caller can do: a write through the client is refused whatever this says.
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
///
/// A characterisation test. That the link is open is asked of the sender; that a caller can write
/// nothing is asked of the client.
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
    let late = client.submit_notification("late", json!({}));
    let refused_to_callers =
        matches!(&late, Err(error) if matches!(error.cause(), Error::Closed { .. }));
    let wire_before_probe = methods(&link);
    let still_open = link_takes_a_write(&client).await;
    let still_pending = !asking.is_finished();
    let told_before = handler.terminations.lock().await.len();
    drop(map);

    let second = within("the second close", client.close()).await;
    let answer = within("the pending call's answer", asking)
        .await
        .expect("expected the request task to finish");
    let observed = (
        (
            cut,
            refused_to_callers,
            wire_before_probe,
            still_open,
            still_pending,
            told_before,
        ),
        second.is_ok(),
        answer.is_err(),
        link_takes_a_write(&client).await,
        handler.terminations.lock().await.len(),
        client.ended(),
    );
    let expected = (
        (true, true, vec![String::from("ask")], true, true, 0),
        true,
        true,
        false,
        0,
        Some(ConnectionEnd::Closed),
    );
    assert_eq!(
        observed, expected,
        "expected the first close cut, callers refused, the link open and the call pending, then a second to fail the call, close the link and tell the handler nothing: {expected:?} | received {observed:?} with the answer {answer:?}"
    );
}

/// A close cut while it waits for the writer behind a write the peer has not taken has not
/// reached the link: what was queued behind that write is refused, notification and request
/// alike, and only the write already in progress reaches the wire. A second close, once that
/// write is done, closes the link.
///
/// A characterisation test. The close is parked on the writer's barrier, not before it: on a
/// paused clock the runtime is idle only when it has no step left to take.
#[tokio::test(start_paused = true)]
async fn a_close_cut_behind_a_held_write_writes_nothing_queued_and_a_second_finishes() {
    let gated = gated(options(), WireOptions::new());
    let held = link_held(&gated).await;
    let queued = gated
        .client
        .submit_notification("queued", json!({}))
        .expect("expected a notification queued behind the held frame");
    let (asked, reply) = gated
        .client
        .submit_request("asked", json!({}), RequestOptions::new())
        .expect("expected a request queued behind the held frame")
        .into_parts();
    let mut first: Closing = Box::pin(gated.client.close());
    park("the first close", &mut first).await;
    let parked = (
        gated.client.is_closed(),
        gated
            .client
            .state
            .closes_in_progress
            .load(Ordering::Acquire),
        gated.link.sent().len(),
    );
    drop(first);
    let poisoned = gated.client.state.link_poisoned.load(Ordering::Acquire);

    gated.gate.add_permits(1);
    let held = within("the held frame", held).await;
    let queued = within("the queued notification", queued).await;
    let asked = within("the queued request's write", asked).await;
    let reply = within("the queued request's reply", reply).await;
    let wire = methods(&gated.link);
    let second = within("the second close", gated.client.close()).await;
    let wire_after = methods(&gated.link);
    let observed = (
        parked,
        poisoned,
        held.is_ok(),
        queued.is_err(),
        asked.is_err(),
        reply.is_err(),
        wire,
        second.is_ok(),
        wire_after,
        link_takes_a_write(&gated.client).await,
        gated.handler.terminations.lock().await.len(),
        gated.client.ended(),
    );
    let expected = (
        (true, 1, 0),
        true,
        true,
        true,
        true,
        true,
        vec![String::from("hold")],
        true,
        vec![String::from("hold")],
        false,
        0,
        Some(ConnectionEnd::Closed),
    );
    assert_eq!(
        observed, expected,
        "expected the close parked on the held write and cut there, the link unusable, only the held frame on the wire, the queued notification and request refused, a second close to finish and the handler told nothing: {expected:?} | received {observed:?}"
    );
}

/// A handler whose `on_terminated` does not return until the test lets it.
struct ParkedTermination {
    entered: AtomicUsize,
    returned: AtomicUsize,
    gate: Semaphore,
}

impl ParkedTermination {
    fn arc() -> Arc<Self> {
        Arc::new(Self {
            entered: AtomicUsize::new(0),
            returned: AtomicUsize::new(0),
            gate: Semaphore::new(0),
        })
    }
}

#[async_trait::async_trait]
impl PeerHandler for ParkedTermination {
    async fn on_notification(&self, _method: String, _params: Value) {}

    async fn on_terminated(&self, _termination: PeerTermination) {
        self.entered.fetch_add(1, Ordering::AcqRel);
        self.gate
            .acquire()
            .await
            .expect("expected the termination gate to stay open")
            .forget();
        self.returned.fetch_add(1, Ordering::AcqRel);
    }
}

/// A second close made while the first is held, never to be polled again, inside its wait for the
/// handler to be told that the peer exited. The first keeps the reader's lock, and the second gives
/// up there within the bound a close documents, twice `shutdown_timeout`, with a `Timeout` that
/// names the reader and is not the link's: the link was closed. It does not wait for good.
#[tokio::test(start_paused = true)]
async fn a_second_close_gives_up_on_a_first_held_while_the_handler_is_being_told() {
    let link = ScriptedLink::new();
    let handler = ParkedTermination::arc();
    let client = Client::connect(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options(),
    );
    link.end();
    until("the handler to be told of the exit", || {
        handler.entered.load(Ordering::Acquire) == 1
    })
    .await;
    let mut first: Closing = Box::pin(client.close());
    park("the first close", &mut first).await;
    let holds_the_reader = client.pump.try_lock().is_err();

    let began = tokio::time::Instant::now();
    let bound = shutdown() * 2;
    let second = tokio::time::timeout(bound * 5, client.close()).await;
    let waited = began.elapsed();
    let outcome = match &second {
        Ok(Ok(())) => String::from("closed cleanly"),
        Ok(Err(error)) => format!("{error:?}"),
        Err(_) => String::from("still waiting"),
    };
    let observed = (
        holds_the_reader,
        matches!(&second, Ok(Err(Error::Timeout { operation, .. })) if operation.contains("reader")),
        matches!(&second, Ok(Err(error)) if is_link_shutdown_timeout(error)),
        waited <= bound,
    );
    assert_eq!(
        observed,
        (true, true, false, true),
        "expected the first close holding the reader and the second giving up with a reader timeout, not the link's, within 2x shutdown_timeout ({bound:?}): (true, true, false, true) | received {observed:?}: the second close {outcome} after {waited:?}"
    );

    // The first is let go: the reader it holds is taken down with it, mid-call, and the handler's
    // call never returns. A third close has nothing left to wait for.
    drop(first);
    let third = within("the third close", client.close()).await;
    let told = (
        handler.entered.load(Ordering::Acquire),
        handler.returned.load(Ordering::Acquire),
        third.is_ok(),
    );
    assert_eq!(
        told,
        (1, 0, true),
        "expected the handler's call cut by the first close being dropped and a third close to finish: (1, 0, true) | received {told:?}"
    );
}

/// A sender whose `close` does not return until the test lets it.
struct HeldClose {
    inner: Box<dyn crate::link::LinkSender>,
    gate: Arc<Semaphore>,
    closes: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl crate::link::LinkSender for HeldClose {
    async fn send(&mut self, message: String) -> crate::Result<()> {
        self.inner.send(message).await
    }

    async fn close(&mut self) -> crate::Result<()> {
        self.closes.fetch_add(1, Ordering::AcqRel);
        self.gate
            .acquire()
            .await
            .expect("expected the close gate to stay open")
            .forget();
        self.inner.close().await
    }
}

/// A second close made while the first is held, never to be polled again, inside the closing of
/// the link. The first keeps the sender, so the second cannot reach the link: it gives up after
/// `shutdown_timeout` with the link's `Timeout` and has not closed it. While the first is alive
/// the second does not make the link unusable; dropping the first does, since it never reached it.
#[tokio::test(start_paused = true)]
async fn a_second_close_times_out_on_a_first_held_inside_the_closing_of_the_link() {
    let link = ScriptedLink::new();
    let (sender, receiver) = link.clone().into_link().split();
    let gate = Arc::new(Semaphore::new(0));
    let closes = Arc::new(AtomicUsize::new(0));
    let client = Client::connect_with(
        crate::link::Link::new(
            Box::new(HeldClose {
                inner: sender,
                gate: Arc::clone(&gate),
                closes: Arc::clone(&closes),
            }),
            receiver,
        ),
        RecordingHandler::arc(None),
        options(),
        WireOptions::new(),
    );
    let mut first: Closing = Box::pin(client.close());
    park("the first close", &mut first).await;
    let holds_the_sender = client.state.sender.try_lock().is_err();

    let began = tokio::time::Instant::now();
    let second = within("the second close", client.close()).await;
    let waited = began.elapsed();
    let during = (
        holds_the_sender,
        matches!(&second, Err(error) if is_link_shutdown_timeout(error)),
        waited,
        closes.load(Ordering::Acquire),
        client.state.closes_in_progress.load(Ordering::Acquire),
        client.state.link_poisoned.load(Ordering::Acquire),
    );
    assert_eq!(
        during,
        (true, true, shutdown(), 1, 1, false),
        "expected the second close to give up after shutdown_timeout with the link's timeout, having asked the link to close no more than the first did, and not to make the link unusable while the first lives: (true, true, {:?}, 1, 1, false) | received {during:?}",
        shutdown()
    );

    drop(first);
    let poisoned = client.state.link_poisoned.load(Ordering::Acquire);
    gate.add_permits(1);
    let third = within("the third close", client.close()).await;
    let after = (poisoned, third.is_ok(), closes.load(Ordering::Acquire));
    assert_eq!(
        after,
        (true, true, 2),
        "expected the first close, once dropped, to leave the link unusable and a third close to close it: (true, true, 2) | received {after:?}"
    );
}
