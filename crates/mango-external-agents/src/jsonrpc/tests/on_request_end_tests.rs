//! What becomes of a question the peer asked, answered on a task of its own by `on_request`,
//! at each way the connection can end: whether the call is run to the end or dropped at its
//! next await, and what the peer is told.

use super::outbox_tests::{limits, options, until, within};
use super::*;
use crate::jsonrpc::WireOptions;
use std::sync::atomic::AtomicUsize;
use std::sync::{Mutex as StdMutex, PoisonError};

/// A handler whose questions never finish, which is what an approval waiting for a person looks
/// like, and which counts how many of its `on_request` futures were dropped. A notification
/// named `block` parks too, to fill the queue behind it.
struct WaitingHandler {
    asked: AtomicUsize,
    dropped: AtomicUsize,
    terminations: StdMutex<Vec<PeerTermination>>,
    terminated: Notify,
}

struct CountOnDrop<'a>(&'a AtomicUsize);

impl Drop for CountOnDrop<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

#[async_trait::async_trait]
impl PeerHandler for WaitingHandler {
    async fn on_notification(&self, method: String, _params: Value) {
        if method == "block" {
            std::future::pending::<()>().await;
        }
    }

    async fn on_request(
        &self,
        _method: String,
        _params: Value,
        _id: RequestId,
    ) -> ServerRequestOutcome {
        let _dropped = CountOnDrop(&self.dropped);
        self.asked.fetch_add(1, Ordering::AcqRel);
        std::future::pending::<()>().await;
        ServerRequestOutcome::Answer(Value::Null)
    }

    async fn on_terminated(&self, termination: PeerTermination) {
        self.terminations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(termination);
        self.terminated.notify_one();
    }
}

/// A receiver that reports the peer's output closed when told to, while the link behind it
/// goes on taking writes: a peer that exited and a pipe that has not yet noticed.
struct ExitableReceiver {
    inner: Box<dyn crate::link::LinkReceiver>,
    exit: Arc<Notify>,
}

#[async_trait::async_trait]
impl crate::link::LinkReceiver for ExitableReceiver {
    async fn recv(&mut self) -> crate::Result<Option<String>> {
        tokio::select! {
            () = self.exit.notified() => Ok(None),
            message = self.inner.recv() => message,
        }
    }
}

/// A client over a link whose sends of frames naming `hold` wait at a gate, with a question
/// already out that nobody answers.
struct Asked {
    link: ScriptedLink,
    exit: Arc<Notify>,
    gate: Arc<tokio::sync::Semaphore>,
    handler: Arc<WaitingHandler>,
    client: Arc<Client>,
}

async fn asked(options: ClientOptions, wire: WireOptions) -> Asked {
    let link = ScriptedLink::new();
    let (sender, receiver) = link.clone().into_link().split();
    let exit = Arc::new(Notify::new());
    let receiver = Box::new(ExitableReceiver {
        inner: receiver,
        exit: Arc::clone(&exit),
    });
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let handler = Arc::new(WaitingHandler {
        asked: AtomicUsize::new(0),
        dropped: AtomicUsize::new(0),
        terminations: StdMutex::new(Vec::new()),
        terminated: Notify::new(),
    });
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
    link.push_line(r#"{"jsonrpc":"2.0","id":"1","method":"session/request_permission"}"#);
    until("the question to be asked", || {
        handler.asked.load(Ordering::Acquire) == 1
    })
    .await;
    Asked {
        link,
        exit,
        gate,
        handler,
        client,
    }
}

impl Asked {
    /// How many `on_request` futures were dropped, what the peer was sent, and how many ends
    /// the handler was told of.
    async fn after_the_end(&self) -> (usize, Vec<String>, usize) {
        let handler = &self.handler;
        until("the question's call to be dropped", || {
            handler.dropped.load(Ordering::Acquire) == 1
        })
        .await;
        let wire = self
            .link
            .sent()
            .iter()
            .map(|frame| {
                let frame: Value = serde_json::from_str(frame).expect("expected a JSON frame");
                format!("{}:{}", frame["id"], frame["error"]["code"])
            })
            .collect();
        let told = handler
            .terminations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len();
        (handler.dropped.load(Ordering::Acquire), wire, told)
    }
}

fn refused_as_closed() -> Vec<String> {
    vec![String::from("\"1\":-32000")]
}

/// A close drops the call at its next await and writes the peer the refusal, before the link
/// is closed. The handler is told of no end: the host that closed the client owns it.
#[tokio::test]
async fn a_close_drops_the_call_and_the_peer_is_refused() {
    let asked = asked(options(), WireOptions::new()).await;

    asked.client.close().await.expect("expected a clean close");
    let observed = asked.after_the_end().await;
    assert_eq!(
        observed,
        (1, refused_as_closed(), 0),
        "expected the call dropped, the peer refused with -32000 and the handler told nothing: (1, {:?}, 0) | received {observed:?}",
        refused_as_closed()
    );
}

/// The peer's exit drops the call and refuses the question on the link, so long as the link
/// still takes the write.
#[tokio::test]
async fn the_peers_exit_drops_the_call_and_the_peer_is_refused_while_the_link_takes_it() {
    let asked = asked(options(), WireOptions::new()).await;

    asked.exit.notify_one();
    within("the end", asked.handler.terminated.notified()).await;
    let observed = asked.after_the_end().await;
    assert_eq!(
        observed,
        (1, refused_as_closed(), 1),
        "expected the call dropped, the peer refused with -32000 and the handler told once: (1, {:?}, 1) | received {observed:?}",
        refused_as_closed()
    );
    asked.client.close().await.expect("expected a clean close");
}

/// The peer's exit on a link that is closed to writes too drops the call, and the refusal has
/// nowhere to go: the pipe to a process that has gone refuses it.
#[tokio::test]
async fn the_peers_exit_on_a_link_closed_to_writes_drops_the_call_and_writes_nothing() {
    let asked = asked(options(), WireOptions::new()).await;

    asked.link.end();
    within("the end", asked.handler.terminated.notified()).await;
    let observed = asked.after_the_end().await;
    assert_eq!(
        observed,
        (1, Vec::<String>::new(), 1),
        "expected the call dropped, nothing written and the handler told once: (1, [], 1) | received {observed:?}"
    );
    asked.client.close().await.expect("expected a clean close");
}

/// A link that fails drops the call, and the refusal has no link to go over.
#[tokio::test]
async fn a_failed_link_drops_the_call_and_nothing_is_written() {
    let asked = asked(options(), WireOptions::new()).await;

    asked.link.fail_sends("broken pipe");
    let _ = asked.client.submit_notification("ping", json!({}));
    within("the end", asked.handler.terminated.notified()).await;
    let observed = asked.after_the_end().await;
    assert_eq!(
        observed,
        (1, Vec::<String>::new(), 1),
        "expected the call dropped, nothing written and the handler told once: (1, [], 1) | received {observed:?}"
    );
    asked.client.close().await.expect("expected a clean close");
}

/// The peer passing an inbound budget drops the call and refuses the question on the link,
/// which still takes it.
#[tokio::test]
async fn an_inbound_overflow_drops_the_call_and_the_peer_is_refused() {
    let asked = asked(
        options().with_max_pending_notifications(1),
        WireOptions::new(),
    )
    .await;

    asked
        .link
        .push_line(r#"{"jsonrpc":"2.0","method":"block","params":{}}"#);
    for _ in 0..3 {
        asked
            .link
            .push_line(r#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#);
    }
    within("the end", asked.handler.terminated.notified()).await;
    let observed = asked.after_the_end().await;
    assert_eq!(
        observed,
        (1, refused_as_closed(), 1),
        "expected the call dropped, the peer refused with -32000 and the handler told once: (1, {:?}, 1) | received {observed:?}",
        refused_as_closed()
    );
    asked.client.close().await.expect("expected a clean close");
}

/// This side passing its outbound budget drops the call, and the refusal is not written: the
/// link is unusable from that moment.
#[tokio::test]
async fn an_outbound_overflow_drops_the_call_and_nothing_is_written_for_it() {
    let asked = asked(options(), limits(usize::MAX, usize::MAX, 2)).await;
    let held = asked
        .client
        .submit_notification("hold", json!({}))
        .expect("expected the held frame queued");
    until("the writer to take the held frame", || held.started()).await;
    let _queued = asked
        .client
        .submit_notification("queued", json!({}))
        .expect("expected the second frame queued");
    let refused = asked.client.submit_notification("one-too-many", json!({}));

    within("the end", asked.handler.terminated.notified()).await;
    let observed = (refused.is_err(), asked.after_the_end().await);
    assert_eq!(
        observed,
        (true, (1, Vec::<String>::new(), 1)),
        "expected the third frame refused, the call dropped, nothing written for it and the handler told once: (true, (1, [], 1)) | received {observed:?}"
    );
    asked.gate.add_permits(1);
    asked.client.close().await.expect("expected a clean close");
}

/// Dropping the client drops the call at once, writes nothing, and tells the handler nothing.
#[tokio::test]
async fn dropping_the_client_drops_the_call_and_writes_nothing() {
    let Asked {
        link,
        gate,
        handler,
        client,
        ..
    } = asked(options(), WireOptions::new()).await;

    drop(client);
    until("the question's call to be dropped", || {
        handler.dropped.load(Ordering::Acquire) == 1
    })
    .await;
    let told = handler
        .terminations
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .len();
    let observed = (link.sent().len(), told);
    assert_eq!(
        observed,
        (0, 0),
        "expected nothing written and the handler told nothing: (0, 0) | received {observed:?}"
    );
    drop(gate);
}
