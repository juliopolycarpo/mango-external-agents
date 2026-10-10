//! How the reader counts the peer's share of the handoff queue, which ordered responses share.

use super::*;
use crate::jsonrpc::{QueuedWork, dispatch, handoff_capacity, peer_share};
use std::sync::{Mutex as StdMutex, PoisonError};
use tokio::sync::Semaphore;
use tokio::sync::mpsc;

const NOTIFICATION: &str = r#"{"jsonrpc":"2.0","method":"item/updated","params":{}}"#;

#[test]
fn the_handoff_channel_holds_both_shares_and_one_entry_more() {
    for (notifications, ordered, expected) in [
        (1, 0, 2),
        (1, 1, 3),
        (256, 64, 321),
        (usize::MAX, 64, Semaphore::MAX_PERMITS),
        (64, usize::MAX, Semaphore::MAX_PERMITS),
        (Semaphore::MAX_PERMITS, 0, Semaphore::MAX_PERMITS),
    ] {
        let capacity = handoff_capacity(notifications, ordered);
        assert_eq!(
            capacity, expected,
            "expected room for {notifications} notifications, {ordered} ordered responses and one more: {expected} | received {capacity}"
        );
    }
}

#[test]
fn the_peers_share_is_the_queue_less_its_ordered_responses() {
    for (length, responses, expected) in [
        (0, 0, 0),
        (2, 0, 2),
        (3, 1, 2),
        (64, 64, 0),
        // The worker took a response out and has not dropped it yet: the count is one ahead of
        // the length, and the share is nothing, not a wrapped number.
        (0, 1, 0),
        (2, 3, 0),
    ] {
        let share = peer_share(length, responses);
        assert_eq!(
            share, expected,
            "expected {length} queued with {responses} responses to leave the peer {expected} | received {share}"
        );
    }
}

/// A client whose reader and worker are idle, a queue the test owns, and one ordered request on
/// the wire, so `dispatch` can be driven a frame at a time.
struct Bench {
    client: Arc<Client>,
    queue: mpsc::Sender<QueuedWork>,
    worker_end: Arc<StdMutex<mpsc::Receiver<QueuedWork>>>,
    prompt: tokio::task::JoinHandle<crate::Result<Value>>,
}

async fn bench(notifications: usize) -> Bench {
    let link = ScriptedLink::new();
    let client = Arc::new(Client::connect(
        link.clone().into_link(),
        RecordingHandler::arc(None),
        ClientOptions::new("ACP agent")
            .with_request_timeout(Duration::from_secs(5))
            .with_max_pending_notifications(notifications),
    ));
    let prompt = {
        let client = Arc::clone(&client);
        tokio::spawn(async move {
            client
                .request_with::<_, Value>(
                    "session/prompt",
                    json!({}),
                    RequestOptions::new().after_earlier_notifications(),
                )
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), link.wait_for_sent(1))
        .await
        .expect("expected the ordered request on the wire");
    let (queue, worker_end) = mpsc::channel(handoff_capacity(notifications, 64));
    Bench {
        client,
        queue,
        worker_end: Arc::new(StdMutex::new(worker_end)),
        prompt,
    }
}

impl Bench {
    async fn read(&self, line: &str) -> std::result::Result<(), PeerTermination> {
        dispatch(&self.client.state, &self.queue, line.to_owned()).await
    }
}

/// Does what the worker does with the entry at the head of the queue.
fn take_one(worker_end: &StdMutex<mpsc::Receiver<QueuedWork>>) {
    let taken = worker_end
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .try_recv();
    match taken {
        Ok(QueuedWork::Response(answer)) => answer.deliver(),
        Ok(QueuedWork::Peer { .. }) => {}
        Err(error) => panic!("expected an entry to take | received {error:?}"),
    }
}

/// The reader admits a notification on two reads, the queue's length and the responses in it.
/// The worker delivering a response between them must not make one notification short of the
/// limit look like the limit, which ends a connection that did nothing wrong.
#[tokio::test]
async fn a_response_delivered_between_the_admission_reads_does_not_end_the_connection() {
    let mut bench = bench(2).await;
    bench
        .read(r#"{"jsonrpc":"2.0","id":"1","result":"pong"}"#)
        .await
        .expect("expected the ordered response queued");
    bench
        .read(NOTIFICATION)
        .await
        .expect("expected the first notification queued");

    // The queue holds a response and one notification: length two, the limit, and a share of one.
    let worker_end = Arc::clone(&bench.worker_end);
    *bench
        .client
        .state
        .after_queue_length_read
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(move || take_one(&worker_end)));
    let admitted = bench.read(NOTIFICATION).await;
    assert!(
        admitted.is_ok(),
        "expected the second of two allowed notifications admitted: Ok | received {admitted:?}"
    );

    let answer = tokio::time::timeout(Duration::from_secs(5), &mut bench.prompt)
        .await
        .expect("expected the delivered answer within 5s")
        .expect("expected the request task to finish")
        .expect("expected the answer");
    assert_eq!(answer, json!("pong"), "received {answer}");
    let refused = bench.read(NOTIFICATION).await;
    assert!(
        matches!(
            refused,
            Err(PeerTermination::NotificationBackpressure {
                limit: 2,
                received: 3
            })
        ),
        "expected the third notification past the limit of two refused: NotificationBackpressure {{ limit: 2, received: 3 }} | received {refused:?}"
    );
}

/// A queued response takes none of the peer's share, and the share itself is unchanged: as many
/// notifications wait as the limit says, and the next one ends the connection.
#[tokio::test]
async fn a_queued_response_leaves_the_notification_limit_where_it_was() {
    let bench = bench(2).await;
    bench
        .read(r#"{"jsonrpc":"2.0","id":"1","result":"pong"}"#)
        .await
        .expect("expected the ordered response queued");
    for nth in 1..=2 {
        let admitted = bench.read(NOTIFICATION).await;
        assert!(
            admitted.is_ok(),
            "expected notification {nth} of two admitted behind a queued response: Ok | received {admitted:?}"
        );
    }
    let refused = bench.read(NOTIFICATION).await;
    assert!(
        matches!(
            refused,
            Err(PeerTermination::NotificationBackpressure {
                limit: 2,
                received: 3
            })
        ),
        "expected the third notification refused: NotificationBackpressure {{ limit: 2, received: 3 }} | received {refused:?}"
    );
    bench.prompt.abort();
}
