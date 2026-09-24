use super::*;
use mango_external_agents::{ByteSink, CancelToken};
use std::sync::Mutex;
use std::time::Duration;

#[derive(Clone, Default)]
struct GatedSink {
    entered: CancelToken,
    release: CancelToken,
    writes: Arc<Mutex<Vec<Vec<u8>>>>,
}

#[async_trait::async_trait]
impl ByteSink for GatedSink {
    async fn write_all(&mut self, bytes: &[u8]) -> mango_external_agents::Result<()> {
        self.entered.cancel();
        self.release.cancelled().await;
        self.writes.lock().expect("writes").push(bytes.to_vec());
        Ok(())
    }
    async fn close(&mut self) -> mango_external_agents::Result<()> {
        Ok(())
    }
}

fn notification() -> String {
    String::from(r#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#)
}

fn output(sink: GatedSink) -> OutgoingLines {
    Box::pin(super::super::outgoing_lines(Box::new(sink)))
}

struct FinishedPeer;

impl<R: Role> ConnectTo<R> for FinishedPeer {
    async fn connect_to(
        self,
        _transport: impl ConnectTo<R::Counterpart>,
    ) -> agent_client_protocol::Result<()> {
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn a_finished_direct_peer_does_not_wait_for_physical_input_eof() {
    let transport = BoundedTransport::new(
        output(GatedSink::default()),
        Box::pin(futures::stream::pending()),
        Limits::default(),
    );
    tokio::time::timeout(
        Duration::from_secs(1),
        <BoundedTransport as ConnectTo<agent_client_protocol::Client>>::connect_to(
            transport,
            FinishedPeer,
        ),
    )
    .await
    .expect("expected completed peer to stop input without physical EOF")
    .expect("peer completed successfully");
}

#[tokio::test(start_paused = true)]
async fn an_unread_sdk_channel_hits_the_incoming_count_budget() {
    let transport = BoundedTransport::new(
        output(GatedSink::default()),
        Box::pin(futures::stream::iter([
            Ok(notification()),
            Ok(notification()),
        ])),
        Limits {
            turn_channel_capacity: 1,
            max_pending_requests: 1,
            ..Limits::default()
        },
    );
    let (mut channel, drive) = transport.parts();
    let result = tokio::time::timeout(Duration::from_secs(1), drive).await;
    let error = result
        .expect("expected input budget failure, received a stalled SDK queue")
        .expect_err("expected overflow before a second frame enters the SDK");
    assert!(
        serde_json::to_string(&error)
            .expect("error")
            .contains("incoming queue")
    );
    assert!(channel.rx.next().await.is_some());
    assert!(channel.rx.next().await.is_none());
}

#[tokio::test]
async fn queued_input_bytes_and_batch_members_have_independent_limits() {
    for (lines, limits, expected) in [
        (
            vec![notification(), notification()],
            Limits {
                turn_buffer_bytes: notification().len(),
                ..Limits::default()
            },
            "byte budget",
        ),
        (
            vec![format!("[{},{}]", notification(), notification())],
            Limits {
                turn_channel_capacity: 1,
                max_pending_requests: 1,
                ..Limits::default()
            },
            "received 2 messages",
        ),
    ] {
        let (tx, _rx) = futures::channel::mpsc::unbounded();
        let error = read_frames(
            Box::pin(futures::stream::iter(lines.into_iter().map(Ok))),
            tx,
            limits,
            &OverflowSlot::default(),
        )
        .await
        .expect_err("expected an explicit bounded-frame refusal");
        assert!(
            serde_json::to_string(&error)
                .expect("error")
                .contains(expected)
        );
    }
}

#[tokio::test]
async fn a_notification_burst_beyond_the_request_cap_fits_the_frame_budget() {
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    let limits = Limits {
        max_pending_requests: 2,
        turn_channel_capacity: 16,
        ..Limits::default()
    };
    let lines = std::iter::repeat_with(notification).take(12).map(Ok);
    tokio::time::timeout(
        Duration::from_secs(1),
        read_frames(
            Box::pin(futures::stream::iter(lines)),
            tx,
            limits,
            &OverflowSlot::default(),
        ),
    )
    .await
    .expect("expected the burst to be queued, received a stalled reader")
    .unwrap_or_else(|error| {
        panic!("expected 12 unread notifications to fit a 16-frame budget, received {error:?}")
    });
    let mut queued = 0;
    while rx.next().await.is_some() {
        queued += 1;
    }
    assert_eq!(queued, 12, "expected every notification to reach the SDK");
}

#[tokio::test(start_paused = true)]
async fn an_undrained_transport_accepts_a_burst_larger_than_the_request_cap() {
    let transport = BoundedTransport::new(
        output(GatedSink::default()),
        Box::pin(futures::stream::iter(
            std::iter::repeat_with(notification).take(12).map(Ok),
        )),
        Limits {
            max_pending_requests: 2,
            turn_channel_capacity: 16,
            ..Limits::default()
        },
    );
    let (mut channel, drive) = transport.parts();
    channel.tx.close_channel();
    tokio::time::timeout(Duration::from_secs(1), drive)
        .await
        .expect("expected the transport to finish, received a stalled drive")
        .unwrap_or_else(|error| {
            panic!(
                "expected 12 undrained notifications to fit a 16-frame budget, received {error:?}"
            )
        });
    let mut queued = 0;
    while channel.rx.next().await.is_some() {
        queued += 1;
    }
    assert_eq!(queued, 12, "expected every notification to reach the SDK");
}

#[tokio::test]
async fn an_unread_burst_past_the_frame_budget_names_received_and_expected_counts() {
    let (tx, _rx) = futures::channel::mpsc::unbounded();
    let limits = Limits {
        max_pending_requests: 1,
        turn_channel_capacity: 3,
        ..Limits::default()
    };
    let lines = std::iter::repeat_with(notification).take(5).map(Ok);
    let error = read_frames(
        Box::pin(futures::stream::iter(lines)),
        tx,
        limits,
        &OverflowSlot::default(),
    )
    .await
    .expect_err("expected the fourth unread frame to exceed a 3-frame budget");
    let error = serde_json::to_string(&error).expect("error");
    assert!(
        error.contains("received 4 messages") && error.contains("expected at most 3 messages"),
        "expected received 4 messages against a 3-message budget, received {error}"
    );
}

#[tokio::test]
async fn unread_batches_are_charged_per_member_against_the_message_budget() {
    let (tx, _rx) = futures::channel::mpsc::unbounded();
    let limits = Limits {
        turn_channel_capacity: 5,
        max_pending_requests: 1,
        ..Limits::default()
    };
    let batch = |members: usize| {
        format!(
            "[{}]",
            std::iter::repeat_with(notification)
                .take(members)
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    let error = read_frames(
        Box::pin(futures::stream::iter([Ok(batch(3)), Ok(batch(3))])),
        tx,
        limits,
        &OverflowSlot::default(),
    )
    .await
    .expect_err("expected two unread 3-member batches to exceed a 5-message budget");
    let error = serde_json::to_string(&error).expect("error");
    assert!(
        error.contains("received 6 messages") && error.contains("expected at most 5 messages"),
        "expected received 6 messages against a 5-message budget, received {error}"
    );
}

fn response(id: usize) -> String {
    format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{}}}}"#)
}

#[tokio::test]
async fn a_batch_of_admitted_responses_fits_despite_a_small_turn_capacity() {
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    let limits = Limits {
        turn_channel_capacity: 1,
        max_pending_requests: 4,
        ..Limits::default()
    };
    let batch = format!("[{},{},{}]", response(1), response(2), response(3));
    read_frames(Box::pin(futures::stream::iter([Ok(batch)])), tx, limits, &OverflowSlot::default())
        .await
        .unwrap_or_else(|error| {
            panic!("expected 3 responses to fit 4 admitted requests with turn capacity 1, received {error:?}")
        });
    assert!(
        rx.next().await.is_some(),
        "expected the batch to reach the SDK"
    );
}

#[test]
fn an_unbounded_count_is_clamped_to_what_tokio_can_allocate() {
    let limits = Limits {
        turn_channel_capacity: usize::MAX,
        ..Limits::default()
    };
    assert_eq!(frame_limit(&limits), usize::MAX);
    assert_eq!(outgoing_capacity(&limits), Semaphore::MAX_PERMITS);
}

#[tokio::test]
async fn a_transport_with_an_unbounded_count_runs_without_panicking() {
    let transport = BoundedTransport::new(
        output(GatedSink::default()),
        Box::pin(futures::stream::iter([Ok(notification())])),
        Limits {
            turn_channel_capacity: usize::MAX,
            ..Limits::default()
        },
    );
    let (mut channel, drive) = transport.parts();
    channel.tx.close_channel();
    let result = tokio::spawn(tokio::time::timeout(Duration::from_secs(1), drive))
        .await
        .unwrap_or_else(|panic| {
            panic!("expected no panic for usize::MAX turn capacity, received {panic}")
        });
    result
        .expect("expected the transport to finish, received a stalled drive")
        .expect("expected the transport to accept the frame");
    assert!(channel.rx.next().await.is_some());
}

#[tokio::test]
async fn an_oversized_frame_names_its_size_and_the_byte_budget() {
    let (tx, _rx) = futures::channel::mpsc::unbounded();
    let line = notification();
    let limits = Limits {
        turn_buffer_bytes: line.len() - 1,
        ..Limits::default()
    };
    let error = read_frames(
        Box::pin(futures::stream::iter([Ok(line.clone())])),
        tx,
        limits,
        &OverflowSlot::default(),
    )
    .await
    .expect_err("expected a frame larger than the byte budget to be refused");
    let error = serde_json::to_string(&error).expect("error");
    assert!(
        error.contains(&format!("{} bytes;", line.len()))
            && error.contains(&format!("and {} bytes", line.len() - 1)),
        "expected received {} bytes against a {}-byte budget, received {error}",
        line.len(),
        line.len() - 1
    );
}

#[tokio::test]
async fn consumed_input_releases_its_byte_budget() {
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    let limits = Limits {
        turn_buffer_bytes: notification().len(),
        ..Limits::default()
    };
    let (consumed, acknowledgements) = tokio::sync::mpsc::channel(1);
    let source = futures::stream::unfold(
        (0, acknowledgements),
        async |(index, mut acknowledgements)| {
            if index > 0 {
                acknowledgements.recv().await.expect("prior frame consumed");
            }
            (index < 3).then(|| (Ok(notification()), (index + 1, acknowledgements)))
        },
    );
    let slot = OverflowSlot::default();
    let reader = read_frames(Box::pin(source), tx, limits, &slot);
    let consumer = async move {
        let mut count = 0;
        while rx.next().await.is_some() {
            count += 1;
            consumed.send(()).await.expect("reader awaits consumption");
        }
        count
    };
    let (result, count) = tokio::join!(reader, consumer);
    result.expect("each consumed frame releases room for the next");
    assert_eq!(count, 3);
}

#[tokio::test]
async fn outgoing_bytes_remain_owned_until_the_physical_write_finishes() {
    let line = notification();
    let budget = Arc::new(Semaphore::new(line.len()));
    let (frames, input) = futures::channel::mpsc::unbounded();
    frames
        .unbounded_send(TransportFrame::parse_json(&line))
        .expect("frame");
    drop(frames);
    let (pending, writing) = mpsc::channel(2);
    queue_output(
        input,
        pending,
        Arc::clone(&budget),
        Limits::default(),
        &OverflowSlot::default(),
    )
    .await
    .expect("queued");
    assert_eq!(budget.available_permits(), 0);
    let sink = GatedSink::default();
    let writer = tokio::spawn(write_frames(output(sink.clone()), writing));
    sink.entered.cancelled().await;
    assert_eq!(
        budget.available_permits(),
        0,
        "the blocked physical write must still own its bytes"
    );
    sink.release.cancel();
    writer.await.expect("writer task").expect("write");
    assert_eq!(budget.available_permits(), line.len());
    assert_eq!(
        sink.writes.lock().expect("writes").as_slice(),
        [format!("{line}\n").into_bytes()]
    );
}

#[tokio::test]
async fn a_stalled_writer_refuses_outgoing_count_and_byte_pressure() {
    for bytes in [notification().len(), usize::MAX] {
        let (frames, input) = futures::channel::mpsc::unbounded();
        for _ in 0..3 {
            frames
                .unbounded_send(TransportFrame::parse_json(&notification()))
                .expect("frame");
        }
        drop(frames);
        let (pending, _writing) = mpsc::channel(1);
        let budget = Arc::new(Semaphore::new(bytes.min(u32::MAX as usize)));
        let error = queue_output(
            input,
            pending,
            budget,
            Limits::default(),
            &OverflowSlot::default(),
        )
        .await
        .expect_err("expected bounded output refusal");
        let error = serde_json::to_string(&error).expect("error");
        let expected = if bytes == usize::MAX {
            "received 2 queued frames; expected at most 1"
        } else {
            "byte budget"
        };
        assert!(
            error.contains(expected),
            "expected {expected:?} in the refusal, received {error}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_budget_failure_fills_the_shared_overflow_slot() {
    let transport = BoundedTransport::new(
        output(GatedSink::default()),
        Box::pin(futures::stream::iter([
            Ok(notification()),
            Ok(notification()),
        ])),
        Limits {
            turn_channel_capacity: 1,
            max_pending_requests: 1,
            ..Limits::default()
        },
    );
    let slot = transport.overflow();
    let (_channel, drive) = transport.parts();
    let _ = tokio::time::timeout(Duration::from_secs(1), drive).await;
    let text = slot.get().map(|overflow| overflow.error().to_string());
    assert_eq!(
        text.as_deref(),
        Some("expected at most 1 JSON-RPC messages queued from the ACP agent, received 2"),
        "expected the slot to name the message budget, received {text:?}"
    );
}

#[tokio::test]
async fn a_budget_refusal_records_its_overflow_before_returning() {
    let slot = OverflowSlot::default();
    let (tx, _rx) = futures::channel::mpsc::unbounded();
    let limits = Limits {
        turn_channel_capacity: 1,
        max_pending_requests: 1,
        ..Limits::default()
    };
    let batch = format!("[{},{}]", notification(), notification());
    let _ = read_frames(
        Box::pin(futures::stream::iter([Ok(batch)])),
        tx,
        limits,
        &slot,
    )
    .await;
    let recorded = slot.get().copied();
    assert_eq!(
        recorded,
        Some(Overflow::incoming_messages(1, 2)),
        "expected the 2-message batch under a 1-message cap to be recorded, received {recorded:?}"
    );
}

/// Records whether the overflow slot was filled at each wake of the SDK's incoming receiver.
struct SlotAtWake {
    slot: OverflowSlot,
    seen: Mutex<Vec<bool>>,
}

impl futures::task::ArcWake for SlotAtWake {
    fn wake_by_ref(this: &Arc<Self>) {
        this.seen
            .lock()
            .expect("seen")
            .push(this.slot.get().is_some());
    }
}

/// The SDK fails pending requests when its incoming channel closes, possibly on another worker,
/// so the budget must already be recorded at the instant that close wakes the receiver.
#[tokio::test]
async fn the_overflow_is_recorded_before_the_incoming_channel_closes() {
    let batch = format!("[{},{}]", notification(), notification());
    let transport = BoundedTransport::new(
        output(GatedSink::default()),
        Box::pin(futures::stream::iter([Ok(batch)])),
        Limits {
            turn_channel_capacity: 1,
            max_pending_requests: 1,
            ..Limits::default()
        },
    );
    let watcher = Arc::new(SlotAtWake {
        slot: transport.overflow(),
        seen: Mutex::new(Vec::new()),
    });
    let (mut channel, drive) = transport.parts();
    let waker = futures::task::waker(Arc::clone(&watcher));
    let mut context = std::task::Context::from_waker(&waker);
    assert!(
        channel.rx.poll_next_unpin(&mut context).is_pending(),
        "expected an empty incoming channel before the transport runs"
    );
    let _ = tokio::time::timeout(Duration::from_secs(1), drive).await;
    let seen = watcher.seen.lock().expect("seen").clone();
    assert_eq!(
        seen,
        vec![true],
        "expected one close wake with the overflow already recorded, received {seen:?}"
    );
}
