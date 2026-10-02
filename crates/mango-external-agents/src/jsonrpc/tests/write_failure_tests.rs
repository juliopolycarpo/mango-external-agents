//! Completed transport failures and safe local refusals have different link consequences.

use super::*;
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
struct RefuseFirstSend {
    inner: Box<dyn crate::link::LinkSender>,
    first: bool,
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
