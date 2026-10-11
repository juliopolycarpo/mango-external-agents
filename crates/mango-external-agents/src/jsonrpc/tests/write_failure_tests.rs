//! Completed transport failures and safe local refusals have different link consequences.

use super::outbox_tests::{gated, limits, link_held, methods, options, until, within};
use super::*;
use crate::Dispatch;
use crate::jsonrpc::{ConnectionEnd, OutboundBound, WireOptions, Written};
use std::future::{Future, poll_fn};
use std::task::Poll;

/// Holds the first physical send until the test has queued a second writer.
struct HeldFirstSend {
    inner: Box<dyn crate::link::LinkSender>,
    release: Arc<tokio::sync::Semaphore>,
    first: bool,
}

#[async_trait::async_trait]
impl crate::link::LinkSender for HeldFirstSend {
    async fn send(&mut self, message: String) -> crate::Result<()> {
        if self.first {
            self.first = false;
            self.release
                .acquire()
                .await
                .expect("expected the send gate to remain open")
                .forget();
        }
        self.inner.send(message).await
    }

    async fn close(&mut self) -> crate::Result<()> {
        self.inner.close().await
    }
}

/// A local transport admission refusal does not put bytes on the wire or damage the sender.
pub(super) struct RefuseFirstSend {
    pub(super) inner: Box<dyn crate::link::LinkSender>,
    pub(super) first: bool,
}

#[async_trait::async_trait]
impl crate::link::LinkSender for RefuseFirstSend {
    async fn send(&mut self, message: String) -> crate::Result<()> {
        if self.first {
            self.first = false;
            return Err(Error::LimitExceeded {
                subject: "local outgoing frame bytes",
                limit: 1,
                received: message.len(),
            }
            .with_dispatch(crate::Dispatch::NotSubmitted));
        }
        self.inner.send(message).await
    }

    async fn close(&mut self) -> crate::Result<()> {
        self.inner.close().await
    }
}

async fn assert_one_epipe_termination(handler: &RecordingHandler, client: &Client) {
    let terminations = tokio::time::timeout(Duration::from_secs(5), terminations_after(handler, 1))
        .await
        .expect("expected termination after completed EPIPE, received an open connection")
        .expect("expected one termination, received none");
    assert!(
        matches!(&terminations[..], [PeerTermination::LinkFailed(cause)] if cause.contains("EPIPE")),
        "expected one LinkFailed naming the original EPIPE, received {terminations:?}"
    );
    assert!(client.is_closed(), "expected the failed link to be closed");
    assert!(
        client.state.pending.lock().await.is_empty(),
        "expected completed failure to release every pending correlation"
    );
}

#[tokio::test]
async fn a_completed_request_write_failure_ends_the_link_and_preserves_dispatch() {
    let link = ScriptedLink::new();
    link.fail_sends("EPIPE");
    let handler = RecordingHandler::arc(None);
    let client = client(link.clone(), Arc::clone(&handler));
    let started = AtomicBool::new(false);

    let error = client
        .request_tracking_write::<_, Value>("account/rateLimits/read", json!({}), &started)
        .await
        .expect_err("expected the completed transport write to fail");
    assert!(
        matches!(error.cause(), Error::Link { message, .. } if message == "EPIPE"),
        "expected the original typed EPIPE, received {error:?}"
    );
    assert!(started.load(Ordering::Acquire));
    assert_eq!(error.dispatch(), crate::Dispatch::AcceptanceUnknown);
    assert_one_epipe_termination(&handler, &client).await;
    assert_eq!(link.refused_sends(), 1);
    client.close().await.expect("expected bounded close");
}

#[tokio::test]
async fn a_completed_notification_write_failure_also_ends_the_link() {
    let link = ScriptedLink::new();
    link.fail_sends("EPIPE");
    let handler = RecordingHandler::arc(None);
    let client = client(link.clone(), Arc::clone(&handler));
    let error = client
        .notify("ping", json!({}))
        .await
        .expect_err("expected the completed notification write to fail");
    assert!(matches!(error.cause(), Error::Link { message, .. } if message == "EPIPE"));
    assert_one_epipe_termination(&handler, &client).await;
    client.close().await.expect("expected bounded close");
}

#[tokio::test]
async fn a_writer_queued_before_a_completed_failure_never_reaches_the_physical_link() {
    let link = ScriptedLink::new();
    link.fail_sends("EPIPE");
    let (sender, receiver) = link.clone().into_link().split();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let handler = RecordingHandler::arc(None);
    let client = Client::connect(
        crate::link::Link::new(
            Box::new(HeldFirstSend {
                inner: sender,
                release: Arc::clone(&release),
                first: true,
            }),
            receiver,
        ),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        ClientOptions::new("test peer").with_request_timeout(Duration::from_secs(5)),
    );
    let mut first = std::pin::pin!(client.state.write(String::from("first")));
    let mut queued = std::pin::pin!(client.state.write(String::from("queued")));
    assert!(
        poll_fn(|cx| Poll::Ready(first.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    assert!(
        poll_fn(|cx| Poll::Ready(queued.as_mut().poll(cx)))
            .await
            .is_pending()
    );

    release.add_permits(1);
    let (failed, queued) = tokio::join!(first, queued);
    assert!(matches!(failed, Err(Error::Link { message, .. }) if message == "EPIPE"));
    assert!(
        queued.is_err(),
        "expected the queued writer to be refused locally"
    );
    assert_eq!(
        link.refused_sends(),
        1,
        "expected only the first write to reach the broken physical link"
    );
    assert_one_epipe_termination(&handler, &client).await;
    client.close().await.expect("expected bounded close");
}

#[tokio::test]
async fn a_completed_local_transport_refusal_preserves_a_usable_link_and_its_dispatch() {
    let link = ScriptedLink::new();
    let (sender, receiver) = link.clone().into_link().split();
    let handler = RecordingHandler::arc(None);
    let client = Client::connect(
        crate::link::Link::new(
            Box::new(RefuseFirstSend {
                inner: sender,
                first: true,
            }),
            receiver,
        ),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        ClientOptions::new("test peer"),
    );
    let error = client
        .request::<_, Value>("too-large", json!({}))
        .await
        .expect_err("expected a local transport admission refusal");
    assert!(matches!(error.cause(), Error::LimitExceeded { .. }));
    assert_eq!(error.dispatch(), crate::Dispatch::NotSubmitted);
    assert!(client.state.pending.lock().await.is_empty());

    link.push_line(r#"{"id":"2","result":"fresh"}"#);
    let answer: String = client
        .request("fresh", json!({}))
        .await
        .expect("expected a usable next send");
    assert_eq!(answer, "fresh");
    assert_eq!(link.sent().len(), 1);
    assert!(!client.is_closed());
    assert!(handler.terminations.lock().await.is_empty());
    client.close().await.expect("expected bounded close");
}

#[tokio::test(start_paused = true)]
async fn a_request_that_times_out_while_queued_does_not_poison_the_healthy_sender() {
    let link = ScriptedLink::new();
    let (sender, receiver) = link.clone().into_link().split();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let handler = RecordingHandler::arc(None);
    let client = Client::connect(
        crate::link::Link::new(
            Box::new(HeldFirstSend {
                inner: sender,
                release: Arc::clone(&release),
                first: true,
            }),
            receiver,
        ),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        ClientOptions::new("test peer").with_request_timeout(Duration::from_secs(5)),
    );
    let mut held = std::pin::pin!(client.notify("held", json!({})));
    assert!(
        poll_fn(|cx| Poll::Ready(held.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    let error = client
        .request_with_timeout::<_, Value>("queued", json!({}), Duration::from_secs(1))
        .await
        .expect_err("expected the queued call's local deadline");
    assert!(matches!(error, Error::Timeout { .. }));
    assert!(client.state.pending.lock().await.is_empty());
    release.add_permits(1);
    held.await
        .expect("expected the healthy first write to complete");
    client
        .notify("fresh", json!({}))
        .await
        .expect("expected a usable next send");
    assert_eq!(link.sent().len(), 2);
    assert!(!client.is_closed());
    assert!(handler.terminations.lock().await.is_empty());
    client.close().await.expect("expected bounded close");
}

/// A handler that says, on a channel the test awaits, what it was told.
struct SayingHandler {
    said: tokio::sync::mpsc::UnboundedSender<&'static str>,
}

#[async_trait::async_trait]
impl PeerHandler for SayingHandler {
    async fn on_notification(&self, _method: String, _params: Value) {}

    async fn on_terminated(&self, _termination: PeerTermination) {
        let _ = self.said.send("told of a termination");
    }
}

/// A link whose write fails, and a close that puts its own end on record just before the reader
/// records the failure. The close owns that end: the reader lets the failure go, the handler is
/// told of no termination, and the client reports the close it was given, as for a failure that
/// lands a moment later.
#[tokio::test]
async fn a_link_failure_that_a_close_overtakes_while_it_is_recorded_is_not_reported() {
    let link = ScriptedLink::new();
    let (say, mut said) = tokio::sync::mpsc::unbounded_channel();
    let client = Client::connect(
        link.into_link(),
        Arc::new(SayingHandler { said: say.clone() }) as Arc<dyn PeerHandler>,
        options(),
    );
    *client
        .state
        .after_write_failure_ignored
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(move || {
        let _ = say.send("let go by the reader");
    }));
    {
        let state = Arc::clone(&client.state);
        *client
            .state
            .before_link_failure_recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(move || {
            // What the first step of a close does.
            state.ended_with(ConnectionEnd::Closed, state.closed_error());
        }));
    }

    client
        .state
        .signal_write_failure(&client.state.write_timeout());
    let first = within("what the reader did with the failure", said.recv()).await;
    client.close().await.expect("expected a clean close");
    let later = said.try_recv().ok();

    let observed = (first, later, client.ended());
    let expected = (
        Some("let go by the reader"),
        None,
        Some(ConnectionEnd::Closed),
    );
    assert_eq!(
        observed, expected,
        "expected the reader to let the failure go under the close on record, and the handler told nothing: {expected:?} | received {observed:?}"
    );
}

/// The reader is woken to end the connection, and before it has looked at why, a close puts its
/// end on record and this side passes its outbound budget. The reader then finds the overflow
/// and is handed the close's record by the end it records. The close owns that end: the handler
/// is told of no termination, and the client reports the close it was given. The reader took the
/// overflow to get there; had it gone the link's way, a marker says so.
#[tokio::test]
async fn an_outbound_overflow_landing_on_a_close_after_the_reader_woke_is_not_reported() {
    let link = ScriptedLink::new();
    let (say, mut said) = tokio::sync::mpsc::unbounded_channel();
    let client = Client::connect(
        link.into_link(),
        Arc::new(SayingHandler { said: say.clone() }) as Arc<dyn PeerHandler>,
        options(),
    );
    let state = Arc::downgrade(&client.state);
    *client
        .state
        .after_reader_woken_to_end
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(move || {
        let Some(state) = state.upgrade() else {
            return;
        };
        // What the first step of a close does, and then what a caller that found the queue
        // full before the closed flag went up does.
        state.ended_with(ConnectionEnd::Closed, state.closed_error());
        let _ = state.outbound_overflow("frames", 1, 2);
    }));
    let went_the_links_way = say.clone();
    *client
        .state
        .before_link_failure_recorded
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(move || {
        let _ = went_the_links_way.send("went the link's way");
    }));
    *client
        .state
        .after_write_failure_ignored
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(move || {
        let _ = say.send("let go by the reader");
    }));

    client.state.write_failed.notify_one();
    let first = within("what the reader did with the overflow", said.recv()).await;
    client.close().await.expect("expected a clean close");
    let later = said.try_recv().ok();

    let observed = (first, later, client.ended());
    let expected = (
        Some("let go by the reader"),
        None,
        Some(ConnectionEnd::Closed),
    );
    assert_eq!(
        observed, expected,
        "expected the reader to take the overflow, let it go under the close recorded after it woke, and the handler told nothing: {expected:?} | received {observed:?}"
    );
}

/// A receiver whose read fails when told to.
struct FailingReceiver {
    fail: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl crate::link::LinkReceiver for FailingReceiver {
    async fn recv(&mut self) -> crate::Result<Option<String>> {
        self.fail.notified().await;
        Err(Error::Link {
            peer: String::from("fake agent"),
            message: String::from("the pipe broke"),
        })
    }
}

/// The same for a read that fails: it ends through the same function, and a close that
/// overtakes it while it is recorded owns the end.
#[tokio::test]
async fn a_read_failure_that_a_close_overtakes_while_it_is_recorded_is_not_reported() {
    let (say, mut said) = tokio::sync::mpsc::unbounded_channel();
    let fail = Arc::new(tokio::sync::Notify::new());
    let (sender, _unused) = ScriptedLink::new().into_link().split();
    let client = Client::connect(
        crate::link::Link::new(
            sender,
            Box::new(FailingReceiver {
                fail: Arc::clone(&fail),
            }),
        ),
        Arc::new(SayingHandler { said: say.clone() }) as Arc<dyn PeerHandler>,
        options(),
    );
    *client
        .state
        .after_write_failure_ignored
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(move || {
        let _ = say.send("let go by the reader");
    }));
    {
        let state = Arc::clone(&client.state);
        *client
            .state
            .before_link_failure_recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(move || {
            // What the first step of a close does.
            state.ended_with(ConnectionEnd::Closed, state.closed_error());
        }));
    }

    fail.notify_one();
    let first = within("what the reader did with the failure", said.recv()).await;
    client.close().await.expect("expected a clean close");
    let later = said.try_recv().ok();

    let observed = (first, later, client.ended());
    let expected = (
        Some("let go by the reader"),
        None,
        Some(ConnectionEnd::Closed),
    );
    assert_eq!(
        observed, expected,
        "expected the reader to let the read failure go under the close on record, and the handler told nothing: {expected:?} | received {observed:?}"
    );
}

/// A receiver that reports the peer's exit when told to.
struct ExitingReceiver {
    exit: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl crate::link::LinkReceiver for ExitingReceiver {
    async fn recv(&mut self) -> crate::Result<Option<String>> {
        self.exit.notified().await;
        Ok(None)
    }
}

/// The peer's exit read in the moment a close puts its own end on record: the close owns the
/// end, the handler is told of no termination, and the client reports the close. A close
/// closes the link, so the reader reads the exit of a peer that was told to go; the end is the
/// host's.
#[tokio::test]
async fn a_peer_exit_that_a_close_overtakes_while_it_is_recorded_is_not_reported() {
    let (say, mut said) = tokio::sync::mpsc::unbounded_channel();
    let exit = Arc::new(tokio::sync::Notify::new());
    let (sender, _unused) = ScriptedLink::new().into_link().split();
    let client = Client::connect(
        crate::link::Link::new(
            sender,
            Box::new(ExitingReceiver {
                exit: Arc::clone(&exit),
            }),
        ),
        Arc::new(SayingHandler { said: say.clone() }) as Arc<dyn PeerHandler>,
        options(),
    );
    *client
        .state
        .after_write_failure_ignored
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(move || {
        let _ = say.send("let go by the reader");
    }));
    {
        let state = Arc::clone(&client.state);
        *client
            .state
            .before_peer_end_recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(move || {
            // What the first step of a close does.
            state.ended_with(ConnectionEnd::Closed, state.closed_error());
        }));
    }

    exit.notify_one();
    let first = within("what the reader did with the exit", said.recv()).await;
    client.close().await.expect("expected a clean close");
    let later = said.try_recv().ok();

    let observed = (first, later, client.ended());
    let expected = (
        Some("let go by the reader"),
        None,
        Some(ConnectionEnd::Closed),
    );
    assert_eq!(
        observed, expected,
        "expected the reader to let the peer's exit go under the close on record, and the handler told nothing: {expected:?} | received {observed:?}"
    );
}

/// A receiver whose read, once begun, holds the reader's thread until it is released, and then
/// reports the peer's exit: a poll of the reader that began before a close and ends after it.
struct HeldRead {
    entered: Arc<std::sync::atomic::AtomicBool>,
    released: std::sync::mpsc::Receiver<()>,
}

#[async_trait::async_trait]
impl crate::link::LinkReceiver for HeldRead {
    async fn recv(&mut self) -> crate::Result<Option<String>> {
        self.entered
            .store(true, std::sync::atomic::Ordering::Release);
        // The thread is held on purpose: the reader's poll is under way, and has checked that
        // the connection is not shut down, for as long as this waits.
        let _ = self
            .released
            .recv_timeout(std::time::Duration::from_secs(10));
        Ok(None)
    }
}

/// The order a handler told `Exited` while `Client::ended` said `Closed` needs, from reading the
/// reader: its poll begins and finds the connection open, a real `Client::close` puts its end
/// on record and shuts the connection down, and only then does the poll read the peer's exit.
/// The close owns the end: the handler is told of no termination and the client reports the
/// close. A stress run of the whole suite showed the outcome once; this order was not observed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_exit_read_by_a_poll_that_began_before_a_close_is_not_reported() {
    let (say, mut said) = tokio::sync::mpsc::unbounded_channel();
    let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (release, released) = std::sync::mpsc::channel::<()>();
    let (sender, _unused) = ScriptedLink::new().into_link().split();
    let client = Arc::new(Client::connect(
        crate::link::Link::new(
            sender,
            Box::new(HeldRead {
                entered: Arc::clone(&entered),
                released,
            }),
        ),
        Arc::new(SayingHandler { said: say.clone() }) as Arc<dyn PeerHandler>,
        options(),
    ));
    *client
        .state
        .after_write_failure_ignored
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(move || {
        let _ = say.send("let go by the reader");
    }));
    until("the reader to be inside its read", || {
        entered.load(std::sync::atomic::Ordering::Acquire)
    })
    .await;

    let closing = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.close().await })
    };
    until(
        "the close to be on record and the connection shut down",
        || client.ended() == Some(ConnectionEnd::Closed) && client.state.shutdown.is_cancelled(),
    )
    .await;
    release.send(()).expect("expected the read still waiting");
    let first = within("what the reader did with the exit", said.recv()).await;
    within("the close", closing)
        .await
        .expect("expected the close task to finish")
        .expect("expected a clean close");
    let later = said.try_recv().ok();

    let observed = (first, later, client.ended());
    let expected = (
        Some("let go by the reader"),
        None,
        Some(ConnectionEnd::Closed),
    );
    assert_eq!(
        observed, expected,
        "expected the reader to let the exit its earlier poll read go under the close on record, and the handler told nothing: {expected:?} | received {observed:?}"
    );
}

/// The same landing, over a writer that is held behind a write with a notification queued behind
/// it: the close is on record, so the overflow only refuses its own frame. What was queued before
/// the close is still written before the link goes, and the link is not made unusable.
#[tokio::test]
async fn an_outbound_overflow_landing_on_a_close_leaves_what_was_queued_before_it_to_be_written() {
    let gated = gated(options(), WireOptions::new());
    let held = link_held(&gated).await;
    let queued = gated
        .client
        .submit_notification("queued", json!({}))
        .expect("expected a frame queued behind the held one");
    let ignored = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let state = Arc::downgrade(&gated.client.state);
        *gated
            .client
            .state
            .after_reader_woken_to_end
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(move || {
            let Some(state) = state.upgrade() else {
                return;
            };
            // What the first step of a close does, and then what a caller that found the queue
            // full before the closed flag went up does.
            state.ended_with(ConnectionEnd::Closed, state.closed_error());
            let _ = state.outbound_overflow("frames", 1, 2);
        }));
    }
    {
        let ignored = Arc::clone(&ignored);
        *gated
            .client
            .state
            .after_write_failure_ignored
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(move || {
            ignored.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }));
    }

    gated.client.state.write_failed.notify_one();
    until("the reader to let the overflow go", || {
        ignored.load(std::sync::atomic::Ordering::Acquire) == 1
    })
    .await;
    let poisoned = gated
        .client
        .state
        .link_poisoned
        .load(std::sync::atomic::Ordering::Acquire);
    gated.gate.add_permits(1);
    let held = within("the held frame", held).await;
    let queued = within("the queued frame", queued).await;
    let closed = within("the close", gated.client.close()).await;

    let observed = (
        poisoned,
        held.is_ok(),
        queued.is_ok(),
        methods(&gated.link),
        closed.is_ok(),
    );
    let expected = (
        false,
        true,
        true,
        vec![String::from("hold"), String::from("queued")],
        true,
    );
    assert_eq!(
        observed, expected,
        "expected the link usable and both frames queued before the close written before it went: {expected:?} | received {observed:?}"
    );
}

/// A client whose queue is full: the writer is held on one frame with another queued behind it,
/// under a bound of two frames, the frame being written counted.
async fn full_queue() -> (super::outbox_tests::Gated, Written, Written) {
    let gated = gated(options(), limits(usize::MAX, usize::MAX, 2));
    let held = link_held(&gated).await;
    let queued = gated
        .client
        .submit_notification("queued", json!({}))
        .expect("expected a frame queued behind the held one");
    (gated, held, queued)
}

/// What a caller whose frame found the queue full is told, by type: the bound passed, whether
/// anything was submitted, and the limit with the count that passed it.
fn overflow_refusal(error: &Error) -> (Option<OutboundBound>, Dispatch, Option<(usize, usize)>) {
    let counts = match error.cause() {
        Error::LimitExceeded {
            limit, received, ..
        } => Some((*limit, *received)),
        _ => None,
    };
    (OutboundBound::exceeded_by(error), error.dispatch(), counts)
}

/// A real close, and a real `submit_notification` that finds a real full queue in the moment
/// between the close's record and its seal, which is the only moment the queue both has a close on
/// record and still measures a frame against its bounds. The close is on record first, so it owns
/// the queue: the overflowing call is refused with the bound it passed, the link is not made
/// unusable, the handler is told nothing, and what was queued before the close is written before
/// the link goes.
#[tokio::test]
async fn a_frame_that_overflows_the_queue_after_a_close_recorded_is_refused_alone() {
    let (gated, held, queued) = full_queue().await;
    let landed: Arc<std::sync::Mutex<Option<(Error, bool)>>> = Arc::default();
    {
        let client = Arc::downgrade(&gated.client);
        let landed = Arc::clone(&landed);
        *gated
            .client
            .state
            .before_seal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(move || {
            let Some(client) = client.upgrade() else {
                return;
            };
            let refused = client.submit_notification("overflow", json!({}));
            let poisoned = client
                .state
                .link_poisoned
                .load(std::sync::atomic::Ordering::Acquire);
            if let Err(error) = refused {
                *landed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((error, poisoned));
            }
        }));
    }
    let closing = {
        let client = Arc::clone(&gated.client);
        tokio::spawn(async move { client.close().await })
    };
    until("the overflowing call to have landed", || {
        landed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    })
    .await;
    gated.gate.add_permits(1);
    let held = within("the held frame", held).await;
    let queued = within("the queued frame", queued).await;
    let closed = within("the close", closing)
        .await
        .expect("expected the close task to finish");

    let (refused, poisoned) = landed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .expect("expected the overflowing call to have been refused");
    let observed = (
        overflow_refusal(&refused),
        poisoned,
        held.is_ok(),
        queued.is_ok(),
        methods(&gated.link),
        closed.is_ok(),
        gated.handler.terminations.lock().await.len(),
        gated.client.ended(),
    );
    let expected = (
        (
            Some(OutboundBound::QueuedFrames),
            Dispatch::NotSubmitted,
            Some((2, 3)),
        ),
        false,
        true,
        true,
        vec![String::from("hold"), String::from("queued")],
        true,
        0,
        Some(ConnectionEnd::Closed),
    );
    assert_eq!(
        observed, expected,
        "expected the overflowing frame refused alone with the link usable, the frames queued before the close written before it went, and the handler told nothing: {expected:?} | received {observed:?}"
    );
}

/// The overflow that is decided before a close seals the queue and put on record after the close
/// has marked the connection closed. The close owns the end, so the overflowing call is refused
/// with the bound it passed and nothing else comes of it: the overflow is not put on record for
/// the reader, the link is not made unusable, the handler is told nothing, and what was queued is
/// written before the link goes.
#[tokio::test]
async fn a_frame_refused_for_room_just_before_a_close_is_refused_alone_and_ends_nothing() {
    let (gated, held, queued) = full_queue().await;
    {
        let state = Arc::downgrade(&gated.client.state);
        *gated
            .client
            .state
            .before_overflow_recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(move || {
            let Some(state) = state.upgrade() else {
                return;
            };
            // The first two steps of a close.
            state.ended_with(ConnectionEnd::Closed, state.closed_error());
            state.mark_closed();
        }));
    }
    let refused = gated
        .client
        .submit_notification("overflow", json!({}))
        .expect_err("expected the frame that found the queue full refused");
    let poisoned = gated
        .client
        .state
        .link_poisoned
        .load(std::sync::atomic::Ordering::Acquire);
    let recorded_for_the_reader = gated
        .client
        .state
        .outbound_overflow
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some();
    gated.gate.add_permits(1);
    let held = within("the held frame", held).await;
    let queued = within("the queued frame", queued).await;
    let closed = within("the close", gated.client.close()).await;

    let observed = (
        overflow_refusal(&refused),
        poisoned,
        recorded_for_the_reader,
        held.is_ok(),
        queued.is_ok(),
        methods(&gated.link),
        closed.is_ok(),
        gated.handler.terminations.lock().await.len(),
        gated.client.ended(),
    );
    let expected = (
        (
            Some(OutboundBound::QueuedFrames),
            Dispatch::NotSubmitted,
            Some((2, 3)),
        ),
        false,
        false,
        true,
        true,
        vec![String::from("hold"), String::from("queued")],
        true,
        0,
        Some(ConnectionEnd::Closed),
    );
    assert_eq!(
        observed, expected,
        "expected the overflowing frame refused alone, nothing put on record for the reader, the link usable and the queued frames written before it went: {expected:?} | received {observed:?}"
    );
}

/// What a caller can do after a close has begun, on a queue that is full: its frame is turned
/// away as closed, not as an overflow, whatever the bounds say. Nothing of it is written, the link
/// is not made unusable by it, and the handler is told nothing.
#[tokio::test]
async fn a_frame_submitted_after_a_close_began_is_refused_as_closed_not_as_an_overflow() {
    let (gated, held, queued) = full_queue().await;
    let closing = {
        let client = Arc::clone(&gated.client);
        tokio::spawn(async move { client.close().await })
    };
    until("the close to have begun", || gated.client.is_closed()).await;
    let refused = gated
        .client
        .submit_notification("late", json!({}))
        .expect_err("expected a frame submitted after the close began refused");
    let poisoned = gated
        .client
        .state
        .link_poisoned
        .load(std::sync::atomic::Ordering::Acquire);
    gated.gate.add_permits(1);
    let held = within("the held frame", held).await;
    let queued = within("the queued frame", queued).await;
    let closed = within("the close", closing)
        .await
        .expect("expected the close task to finish");

    let observed = (
        matches!(refused.cause(), Error::Closed { subject: "link" }),
        OutboundBound::exceeded_by(&refused),
        poisoned,
        held.is_ok(),
        queued.is_ok(),
        methods(&gated.link),
        closed.is_ok(),
        gated.handler.terminations.lock().await.len(),
    );
    let expected = (
        true,
        None,
        false,
        true,
        true,
        vec![String::from("hold"), String::from("queued")],
        true,
        0,
    );
    assert_eq!(
        observed, expected,
        "expected the late frame refused as closed and never written, the frames queued before the close written, and the handler told nothing: {expected:?} | received {observed:?}"
    );
}
