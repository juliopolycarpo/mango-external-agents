//! One turn's events, bounded, and the only way a harness can produce them.
//!
//! A harness reducer pushes raw vendor-shaped facts through an [`EventSink`], which normalises
//! them and puts them on a bounded channel. The two bounds are deliberate and neither can be
//! bypassed: a value the vendor wrote is cut or refused before the host sees it, and a host that
//! stops reading stops the vendor rather than growing the library's memory.

use tokio::sync::mpsc;

use crate::error::{Error, Result, VendorError};
use crate::event::{AgentEvent, EventKind, SessionId, TurnId};
use crate::host::Clock;
use crate::session::CancelReason;
use std::sync::Arc;

/// One turn's events, in order.
///
/// The channel is bounded (see [`Limits::turn_channel_capacity`](crate::Limits)): a host that
/// stops reading applies backpressure all the way to the vendor process, which is the behaviour
/// worth having when the alternative is buffering a runaway stream until the process dies.
///
/// The bound is a number of events, not a number of bytes, and each event was already bounded by
/// the line cap it arrived under — so the honest ceiling before backpressure engages is the two
/// multiplied together. A host that cares about the byte figure sets
/// [`Limits::turn_channel_capacity`](crate::Limits) against its own line cap rather than reading
/// the default as a memory guarantee.
pub struct TurnStream {
    /// The host's own id for this turn, echoed on every event.
    pub turn_id: TurnId,
    /// The vendor's handle for this turn, for the calls that name one.
    pub native_turn_id: String,
    /// The events themselves.
    pub events: mpsc::Receiver<AgentEvent>,
}

impl TurnStream {
    /// The next event, or `None` once the turn is over.
    ///
    /// A convenience for `self.events.recv().await`, so a host that only reads never touches the
    /// channel type.
    pub async fn recv(&mut self) -> Option<AgentEvent> {
        self.events.recv().await
    }
}

impl std::fmt::Debug for TurnStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TurnStream")
            .field("turn_id", &self.turn_id)
            .field("native_turn_id", &self.native_turn_id)
            .finish_non_exhaustive()
    }
}

/// A review's stream, plus the thread the vendor decided to run it on.
///
/// The extra field is the whole reason starting a review is awaited: a turn has nothing to report
/// beyond its handle, whereas a review's response names a thread. A harness returns the vendor's
/// value rather than echoing the session's, so a host can refuse a thread it is not subscribed to
/// instead of streaming a review nobody would see.
#[derive(Debug)]
pub struct ReviewStream {
    /// The events, exactly as an ordinary turn's.
    pub turn: TurnStream,
    /// The thread the vendor ran the review on.
    pub review_thread_id: String,
}

/// Where a harness's reducer puts what it read, and the only door into a [`TurnStream`].
///
/// Cloneable: a reducer that fans a turn out over several tasks shares one sink and the events
/// stay in the order they were sent.
#[derive(Clone)]
pub struct EventSink {
    session_id: SessionId,
    turn_id: TurnId,
    clock: Arc<dyn Clock>,
    sender: mpsc::Sender<AgentEvent>,
}

impl std::fmt::Debug for EventSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EventSink")
            .field("session_id", &self.session_id)
            .field("turn_id", &self.turn_id)
            .field("is_closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

impl EventSink {
    /// A sink and the receiver its events arrive on.
    ///
    /// `capacity` is how many events may wait unread before [`EventSink::emit`] stops returning
    /// until the host reads one.
    pub fn new(
        session_id: SessionId,
        turn_id: TurnId,
        clock: Arc<dyn Clock>,
        capacity: usize,
    ) -> (Self, mpsc::Receiver<AgentEvent>) {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        (
            Self {
                session_id,
                turn_id,
                clock,
                sender,
            },
            receiver,
        )
    }

    /// Normalises one event and puts it on the channel, waiting if the host is behind.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidVendorValue`] when a vendor id does not survive bounding, and
    /// [`Error::Closed`] when the host dropped the stream. Both end the reducer's loop: there is
    /// nobody to tell, and a turn nobody is reading is a turn to stop feeding.
    pub async fn emit(&self, kind: EventKind) -> Result<()> {
        let event = self.event(kind)?;
        self.sender.send(event).await.map_err(|_| Error::Closed {
            subject: "turn stream",
        })
    }

    /// Ends the turn, marking why it stopped first.
    ///
    /// [`EventKind::Cancelled`] is a marker rather than a terminal, so it is always followed by
    /// [`EventKind::Completed`]: a host that does not recognise the marker still sees its turn end
    /// rather than hanging on a kind it consumed and dropped.
    ///
    /// # Errors
    ///
    /// As [`EventSink::emit`].
    pub async fn cancel(&self, reason: CancelReason) -> Result<()> {
        self.emit(EventKind::Cancelled { reason }).await?;
        self.complete().await
    }

    /// Offers a cancellation without leaving a lone marker if shutdown interrupts this future.
    ///
    /// Reserves both events together. A one-slot channel receives only `Completed` because it
    /// cannot fit both events atomically. Ordinary turn cancellation uses [`Self::cancel`].
    ///
    /// # Example
    ///
    /// ```no_run
    /// # async fn example(sink: &mango_external_agents::stream::EventSink) {
    /// use mango_external_agents::CancelReason;
    /// let _ = tokio::time::timeout(std::time::Duration::from_millis(200),
    ///     sink.cancel_on_close(CancelReason::Shutdown)).await;
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// As [`Self::emit`].
    pub async fn cancel_on_close(&self, reason: CancelReason) -> Result<()> {
        let completed = self.event(EventKind::Completed)?;
        // A channel with room for one cannot reserve the marker and its terminal together. A
        // terminal alone is the only shape that cannot be cut in half by a shutdown timeout.
        if self.sender.max_capacity() < 2 {
            return self
                .sender
                .send(completed)
                .await
                .map_err(|_| Error::Closed {
                    subject: "turn stream",
                });
        }
        let cancelled = self.event(EventKind::Cancelled { reason })?;
        let mut permits = self
            .sender
            .reserve_many(2)
            .await
            .map_err(|_| Error::Closed {
                subject: "turn stream",
            })?;
        permits
            .next()
            .expect("expected a permit for the cancellation marker")
            .send(cancelled);
        permits
            .next()
            .expect("expected a permit for the cancellation terminal")
            .send(completed);
        Ok(())
    }

    /// Ends the turn with a failure.
    ///
    /// # Errors
    ///
    /// As [`EventSink::emit`].
    pub async fn fail(&self, error: VendorError) -> Result<()> {
        self.emit(EventKind::Error { error }).await
    }

    /// Ends the turn.
    ///
    /// # Errors
    ///
    /// As [`EventSink::emit`].
    pub async fn complete(&self) -> Result<()> {
        self.emit(EventKind::Completed).await
    }

    fn event(&self, kind: EventKind) -> Result<AgentEvent> {
        Ok(AgentEvent {
            session_id: self.session_id.clone(),
            turn_id: self.turn_id.clone(),
            at: self.clock.now(),
            kind: kind.normalized()?,
        })
    }

    /// Whether the host has dropped the stream.
    ///
    /// Worth checking before doing expensive work for an event nobody will read.
    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }

    /// The session these events belong to.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The turn these events belong to.
    pub fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }
}

#[cfg(test)]
mod tests {
    use super::{EventSink, TurnStream};
    use crate::error::Error;
    use crate::event::{EventKind, SessionId, TurnId};
    use crate::host::{Clock, SystemClock};
    use crate::session::CancelReason;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    /// A clock that never moves, so an event's stamp is a value a test can assert on.
    struct FrozenClock(SystemTime);

    impl Clock for FrozenClock {
        fn now(&self) -> SystemTime {
            self.0
        }
    }

    fn sink(
        capacity: usize,
    ) -> (
        EventSink,
        tokio::sync::mpsc::Receiver<crate::event::AgentEvent>,
    ) {
        EventSink::new(
            SessionId::new("session-1"),
            TurnId::new("turn-1"),
            Arc::new(SystemClock),
            capacity,
        )
    }

    #[tokio::test]
    async fn stamps_every_event_with_its_session_turn_and_time() {
        let stamped = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let (sink, mut events) = EventSink::new(
            SessionId::new("session-1"),
            TurnId::new("turn-1"),
            Arc::new(FrozenClock(stamped)),
            4,
        );

        sink.emit(EventKind::TextDelta {
            text: String::from("hello"),
        })
        .await
        .expect("expected the event to be sent");

        let event = events.recv().await.expect("expected an event");
        assert_eq!(event.session_id, SessionId::new("session-1"));
        assert_eq!(event.turn_id, TurnId::new("turn-1"));
        assert_eq!(event.at, stamped);
    }

    #[tokio::test]
    async fn normalises_on_the_way_through_so_a_reducer_cannot_bypass_the_bounds() {
        let (sink, mut events) = sink(4);

        sink.emit(EventKind::TextDelta {
            text: String::from("safe\u{202e}text"),
        })
        .await
        .expect("expected the event to be sent");

        let event = events.recv().await.expect("expected an event");
        assert_eq!(
            event.kind,
            EventKind::TextDelta {
                text: String::from("safetext")
            }
        );
    }

    #[tokio::test]
    async fn refuses_an_event_whose_vendor_id_cannot_survive_bounding() {
        let (sink, _events) = sink(4);

        let error = sink
            .emit(EventKind::SessionStarted {
                native_session_id: String::from("  "),
                resumed: false,
            })
            .await
            .expect_err("expected a refusal, received a send");
        assert!(
            matches!(
                error,
                Error::InvalidVendorValue {
                    field: "native session id",
                    ..
                }
            ),
            "expected an invalid session id, received {error:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_host_that_stops_reading_stalls_the_writer_instead_of_growing_memory() {
        let (sink, mut events) = sink(2);

        for index in 0..2 {
            sink.emit(EventKind::TextDelta {
                text: index.to_string(),
            })
            .await
            .expect("expected the event to be sent");
        }

        // The channel is full and nobody is reading: the next emit must not return.
        let blocked = tokio::time::timeout(
            Duration::from_secs(30),
            sink.emit(EventKind::TextDelta {
                text: String::from("third"),
            }),
        )
        .await;
        assert!(
            blocked.is_err(),
            "expected the writer to stall on a full channel, received {blocked:?}"
        );

        // Reading one frees exactly one slot, and the writer proceeds.
        events.recv().await.expect("expected an event");
        sink.emit(EventKind::TextDelta {
            text: String::from("third"),
        })
        .await
        .expect("expected the event to be sent once a slot freed");
    }

    #[tokio::test]
    async fn a_one_slot_stream_ends_cleanly_when_cancelled() {
        let (sink, mut events) = sink(1);
        let sending = tokio::spawn(async move { sink.cancel(CancelReason::Shutdown).await });
        assert_eq!(
            events.recv().await.map(|event| event.kind),
            Some(crate::event::EventKind::Cancelled {
                reason: CancelReason::Shutdown
            })
        );
        assert_eq!(
            events.recv().await.map(|event| event.kind),
            Some(crate::event::EventKind::Completed)
        );
        sending
            .await
            .expect("sender task")
            .expect("cancellation sent");
        assert!(events.recv().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn closing_never_leaves_a_cancellation_marker_without_a_terminal() {
        let (sink, mut events) = sink(2);
        sink.emit(EventKind::TextDelta {
            text: String::from("queued"),
        })
        .await
        .expect("queued event");
        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                sink.cancel_on_close(CancelReason::Shutdown)
            )
            .await
            .is_err()
        );
        assert!(matches!(
            events.recv().await.expect("original event").kind,
            EventKind::TextDelta { .. }
        ));
        assert!(
            events.try_recv().is_err(),
            "expected no partial cancellation marker"
        );
        sink.cancel_on_close(CancelReason::Shutdown)
            .await
            .expect("atomic cancellation");
        assert!(matches!(
            events.recv().await.expect("marker").kind,
            EventKind::Cancelled {
                reason: CancelReason::Shutdown
            }
        ));
        assert_eq!(
            events.recv().await.expect("terminal").kind,
            EventKind::Completed
        );
    }

    #[tokio::test]
    async fn one_slot_close_falls_back_to_a_terminal() {
        let (sink, mut events) = sink(1);
        sink.cancel_on_close(CancelReason::Shutdown)
            .await
            .expect("close terminal");
        assert_eq!(
            events.recv().await.expect("terminal").kind,
            EventKind::Completed
        );
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_dropped_stream_closes_the_sink() {
        let (sink, events) = sink(4);
        drop(events);

        let error = sink
            .emit(EventKind::Completed)
            .await
            .expect_err("expected a refusal, received a send");
        assert!(
            matches!(
                error,
                Error::Closed {
                    subject: "turn stream"
                }
            ),
            "expected a closed stream, received {error:?}"
        );
        assert!(sink.is_closed());
    }

    /// A failure is a terminal on its own: unlike a cancellation it is not followed by a
    /// completion, so a host reading `is_terminal` still sees its turn end exactly once.
    #[tokio::test]
    async fn a_failed_turn_ends_on_the_error_itself() {
        use crate::error::{ErrorCode, VendorError};

        let (sink, mut events) = sink(4);
        sink.fail(VendorError::new(
            ErrorCode::from_static("fake-stream-broken"),
            "the vendor exited mid-turn",
        ))
        .await
        .expect("expected the failure to be sent");

        let event = events.recv().await.expect("expected an error event");
        assert!(event.is_terminal(), "received {:?}", event.kind);
        let EventKind::Error { error } = event.kind else {
            panic!("expected an error event");
        };
        assert_eq!(error.message, "the vendor exited mid-turn");
    }

    #[tokio::test]
    async fn a_cancelled_turn_still_completes() {
        let (sink, events) = sink(4);
        let mut turn = TurnStream {
            turn_id: TurnId::new("turn-1"),
            native_turn_id: String::from("native-1"),
            events,
        };

        sink.cancel(CancelReason::Requested)
            .await
            .expect("expected the turn to end");

        let cancelled = turn.recv().await.expect("expected a cancellation");
        assert_eq!(
            cancelled.kind,
            EventKind::Cancelled {
                reason: CancelReason::Requested
            }
        );
        assert!(!cancelled.is_terminal());

        let completed = turn.recv().await.expect("expected a completion");
        assert_eq!(completed.kind, EventKind::Completed);
        assert!(completed.is_terminal());
    }
}
