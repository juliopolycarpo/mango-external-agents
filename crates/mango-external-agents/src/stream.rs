//! Bounded turn transcripts with terminal commitment independent of reader progress.
//!
//! Payload overflow fails the stream explicitly. Control events have their own count reserve,
//! and a terminal is retained outside that queue so shutdown never waits for the UI.

mod buffer;
use buffer::Buffer;
pub use buffer::EventReceiver;

use tokio::sync::mpsc;

use crate::error::{Error, Result, VendorError};
use crate::event::{AgentEvent, EventKind, SessionId, TurnId};
use crate::host::Clock;
use crate::operation::{AttemptId, Dispatch, OperationRef};
use crate::session::CancelReason;
use std::sync::Arc;

/// One turn's events, in order.
///
/// Payloads obey the event and byte budgets in [`crate::Limits`]. At most two terminal events
/// are reserved separately, so a live consumer can read them after the native work has stopped.
/// Dropping this owner is abandonment; a browser disconnect should only detach from the host's
/// supervisor, which keeps this stream and decides when to cancel it.
pub struct TurnStream {
    turn_id: TurnId,
    attempt: AttemptId,
    native_turn_id: String,
    dispatch: Dispatch,
    events: EventReceiver,
}

impl TurnStream {
    /// The stream of an attempt the vendor accepted.
    ///
    /// [`Dispatch::Accepted`] is the only honest verdict here: the vendor answered with a handle,
    /// so the work is running. A dispatch that did not get this far never produces a stream — it
    /// produces an [`Error`], whose [`Error::dispatch`](crate::Error::dispatch) says how far it
    /// got.
    pub fn accepted(
        turn_id: TurnId,
        attempt: AttemptId,
        native_turn_id: impl Into<String>,
        events: EventReceiver,
    ) -> Self {
        Self {
            turn_id,
            attempt,
            native_turn_id: native_turn_id.into(),
            dispatch: Dispatch::Accepted,
            events,
        }
    }

    /// The host's own id for this logical turn.
    pub fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    /// Which dispatch of that turn this stream belongs to.
    pub fn attempt(&self) -> &AttemptId {
        &self.attempt
    }

    /// The vendor's handle for this turn, for the calls that name one.
    pub fn native_turn_id(&self) -> &str {
        &self.native_turn_id
    }

    /// How certain this attempt's arrival at the vendor is.
    pub fn dispatch(&self) -> Dispatch {
        self.dispatch
    }

    /// Which session, turn and attempt this stream belongs to.
    pub fn operation(&self, session_id: SessionId) -> OperationRef {
        OperationRef::new(session_id, self.turn_id.clone(), self.attempt)
    }

    /// The committed outcome, available even if the transcript has not been read.
    ///
    /// For example, a retrying supervisor checks this before reconciling uncertain dispatch.
    pub fn terminal_status(&self) -> Option<TerminalStatus> {
        self.events.terminal_status()
    }

    /// Marks dispatch uncertainty without giving up the owned event stream.
    ///
    /// For example, a vendor that has no prompt acknowledgement uses AcceptanceUnknown until
    /// its terminal response proves completion. This never authorizes automatic replay.
    #[must_use]
    pub fn with_dispatch(mut self, dispatch: Dispatch) -> Self {
        self.dispatch = dispatch;
        self
    }

    /// The next event, or `None` once the turn is over.
    pub async fn recv(&mut self) -> Option<AgentEvent> {
        self.events.recv().await
    }

    /// The next event if one is already waiting.
    ///
    /// For a caller that must not block — a conformance check draining what a turn left behind, a
    /// host polling on its own schedule. `Err` means "nothing right now", which includes a stream
    /// that has ended.
    ///
    /// # Errors
    ///
    /// [`tokio::sync::mpsc::error::TryRecvError`] when no event is waiting.
    pub fn try_recv(&mut self) -> std::result::Result<AgentEvent, mpsc::error::TryRecvError> {
        self.events.try_recv()
    }
}

impl std::fmt::Debug for TurnStream {
    /// Reports that the handle routes a turn, not which one.
    ///
    /// This is the value a host actually logs. `turn_id` is the host's own, and `native_turn_id`
    /// is the vendor's on ACP and Codex — the same two ids [`EventSink`]'s `Debug` already reports
    /// as flags, reached through the handle instead of through the sink.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TurnStream")
            .field("has_turn_id", &true)
            .field("has_native_turn_id", &!self.native_turn_id.is_empty())
            .field("dispatch", &self.dispatch)
            .finish_non_exhaustive()
    }
}

/// The logical terminal outcome, independent of transcript delivery and process reaping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalStatus {
    /// Native work completed successfully.
    Completed,
    /// The owned attempt was cancelled.
    Cancelled {
        /// The reason the owner supplied.
        reason: CancelReason,
    },
    /// Native work or stream delivery failed. Dispatch certainty still governs replay.
    Failed {
        /// The normalized failure category.
        code: crate::ErrorCode,
    },
}

/// A review's stream, plus the thread the vendor decided to run it on.
///
/// The extra field is the whole reason starting a review is awaited: a turn has nothing to report
/// beyond its handle, whereas a review's response names a thread. A harness returns the vendor's
/// value rather than echoing the session's, so a host can refuse a thread it is not subscribed to
/// instead of streaming a review nobody would see.
pub struct ReviewStream {
    /// The events, exactly as an ordinary turn's.
    pub turn: TurnStream,
    /// The thread the vendor ran the review on.
    pub review_thread_id: String,
}

impl std::fmt::Debug for ReviewStream {
    /// Reports that the review names a thread, not which one: the id is the vendor's own.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReviewStream")
            .field("turn", &self.turn)
            .field("has_review_thread_id", &!self.review_thread_id.is_empty())
            .finish_non_exhaustive()
    }
}

/// Where a harness's reducer puts what it read, and the only door into a [`TurnStream`].
///
/// Cloneable: a reducer that fans a turn out over several tasks shares one sink and the events
/// stay in the order they were sent.
pub struct EventSink {
    session_id: SessionId,
    turn_id: TurnId,
    attempt: AttemptId,
    clock: Arc<dyn Clock>,
    buffer: Arc<Buffer>,
}

impl Clone for EventSink {
    fn clone(&self) -> Self {
        self.buffer.add_sender();
        Self {
            session_id: self.session_id.clone(),
            turn_id: self.turn_id.clone(),
            attempt: self.attempt,
            clock: Arc::clone(&self.clock),
            buffer: Arc::clone(&self.buffer),
        }
    }
}

impl Drop for EventSink {
    fn drop(&mut self) {
        self.buffer.remove_sender();
    }
}

impl std::fmt::Debug for EventSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EventSink")
            .field("has_session_id", &true)
            .field("has_turn_id", &true)
            .field("is_closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

impl EventSink {
    /// A sink and the receiver its events arrive on.
    ///
    /// `capacity` is how many events may wait unread before [`EventSink::emit`] stops returning
    /// until the host reads one.
    ///
    /// The attempt is carried so every event this sink stamps names the dispatch it came from. A
    /// sink built per attempt is what makes a late event from an abandoned one recognisable.
    pub fn new(
        session_id: SessionId,
        turn_id: TurnId,
        attempt: AttemptId,
        clock: Arc<dyn Clock>,
        capacity: usize,
    ) -> (Self, EventReceiver) {
        Self::with_limits(
            session_id,
            turn_id,
            attempt,
            clock,
            &crate::Limits {
                turn_channel_capacity: capacity,
                ..crate::Limits::default()
            },
        )
    }

    /// Creates a stream under the host's event, byte and pending-interaction budgets.
    ///
    /// For example, harnesses pass `host.limits()` to give every stream the same policy.
    pub fn with_limits(
        session_id: SessionId,
        turn_id: TurnId,
        attempt: AttemptId,
        clock: Arc<dyn Clock>,
        limits: &crate::Limits,
    ) -> (Self, EventReceiver) {
        let (buffer, events) = Buffer::new(*limits);
        (
            Self {
                session_id,
                turn_id,
                attempt,
                clock,
                buffer,
            },
            events,
        )
    }

    /// Normalizes and queues an event without waiting on the consumer.
    ///
    /// For example, a protocol callback can publish text while continuing to acknowledge RPCs.
    /// Overflow returns `LimitExceeded` and commits a reserved `stream-overflow` failure; the
    /// driver must stop native work. Events after a terminal are refused.
    pub async fn emit(&self, kind: EventKind) -> Result<()> {
        if matches!(kind, EventKind::Completed) {
            return self.complete().await;
        }
        if let EventKind::Error { error } = kind {
            return self.fail(error).await;
        }
        let result = self.buffer.push(self.event(kind)?);
        if let Err(error @ Error::LimitExceeded { .. }) = &result {
            let _ = self
                .fail(VendorError::new(
                    crate::ErrorCode::from_static("stream-overflow"),
                    error.to_string(),
                ))
                .await;
        }
        result
    }

    /// Commits cancellation and its compatibility terminal together, even when the queue is full.
    ///
    /// For example, shutdown records its reason without waiting for transcript consumption.
    pub async fn cancel(&self, reason: CancelReason) -> Result<()> {
        self.buffer.finish(
            vec![
                self.event(EventKind::Cancelled { reason })?,
                self.event(EventKind::Completed)?,
            ],
            TerminalStatus::Cancelled { reason },
        )
    }

    /// Commits cancellation during close using the same reserved terminal as ordinary cancel.
    ///
    /// For example, `sink.cancel_on_close(CancelReason::Shutdown).await` never waits on a reader.
    pub async fn cancel_on_close(&self, reason: CancelReason) -> Result<()> {
        self.cancel(reason).await
    }

    /// Commits a failure once, retaining its normalized payload for a live receiver.
    ///
    /// For example, a failed native prompt calls this before releasing its attempt slot.
    pub async fn fail(&self, error: VendorError) -> Result<()> {
        let event = self.event(EventKind::Error { error })?;
        let EventKind::Error { error } = &event.kind else {
            unreachable!("normalized error event")
        };
        let status = TerminalStatus::Failed {
            code: error.code.clone(),
        };
        self.buffer.finish(vec![event], status)
    }

    /// Commits successful completion once without waiting for the consumer.
    ///
    /// For example, a native terminal response calls this before releasing admission.
    pub async fn complete(&self) -> Result<()> {
        self.buffer.finish(
            vec![self.event(EventKind::Completed)?],
            TerminalStatus::Completed,
        )
    }

    /// Waits for owner abandonment, even when the vendor is silent.
    ///
    /// Drivers select this alongside native output to stop a dropped stream promptly.
    pub async fn closed(&self) {
        self.buffer.closed().await;
    }

    /// Waits for terminal commitment, including an overflow failure.
    ///
    /// Drivers select this when publication failure must interrupt pending vendor work.
    pub async fn terminated(&self) {
        self.buffer.terminated().await;
    }

    /// Whether any producer has already committed a terminal outcome.
    ///
    /// Drivers use this to reject late callbacks after shutdown or overflow.
    pub fn is_terminal(&self) -> bool {
        self.buffer.status().is_some()
    }

    fn event(&self, kind: EventKind) -> Result<AgentEvent> {
        Ok(AgentEvent {
            session_id: self.session_id.clone(),
            turn_id: self.turn_id.clone(),
            attempt: self.attempt,
            at: self.clock.now(),
            kind: kind.normalized()?,
        })
    }

    /// Whether the host has dropped the stream.
    ///
    /// Worth checking before doing expensive work for an event nobody will read.
    pub fn is_closed(&self) -> bool {
        self.buffer.is_closed()
    }

    /// The session these events belong to.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// The turn these events belong to.
    pub fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    /// Which dispatch of that turn these events belong to.
    pub fn attempt(&self) -> &AttemptId {
        &self.attempt
    }

    /// Which session, turn and attempt these events belong to.
    pub fn operation(&self) -> OperationRef {
        OperationRef::new(self.session_id.clone(), self.turn_id.clone(), self.attempt)
    }
}

#[cfg(test)]
mod tests {
    use super::{EventReceiver, EventSink, TurnStream};
    use crate::error::Error;
    use crate::event::{EventKind, SessionId, TurnId};
    use crate::host::{Clock, SystemClock};
    use crate::operation::{AttemptId, Dispatch};
    use crate::session::CancelReason;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    #[tokio::test]
    async fn byte_pressure_commits_a_terminal_and_never_exceeds_the_budget() {
        let limits = crate::Limits {
            turn_buffer_bytes: 512,
            ..crate::Limits::default()
        };
        let (sink, mut events) = EventSink::with_limits(
            SessionId::new("s"),
            TurnId::new("t"),
            AttemptId::FIRST,
            Arc::new(SystemClock),
            &limits,
        );
        sink.emit(EventKind::TextDelta {
            text: "a".repeat(256),
        })
        .await
        .expect("first payload fits");
        assert!(events.queued_bytes() <= 512);
        let result = sink
            .emit(EventKind::TextDelta {
                text: "b".repeat(256),
            })
            .await;
        assert!(matches!(
            result,
            Err(Error::LimitExceeded {
                subject: "queued turn payload bytes",
                ..
            })
        ));
        assert!(matches!(
            events.terminal_status(),
            Some(super::TerminalStatus::Failed { .. })
        ));
        events.recv().await.expect("queued payload");
        assert_eq!(events.queued_bytes(), 0);
        assert!(
            events
                .recv()
                .await
                .expect("overflow terminal")
                .is_terminal()
        );
    }

    #[tokio::test]
    async fn terminal_status_is_observable_without_draining_and_rejects_late_events() {
        let (sink, events) = sink(1);
        sink.emit(EventKind::TextDelta {
            text: "queued".into(),
        })
        .await
        .expect("queued");
        let mut stream =
            TurnStream::accepted(TurnId::new("turn"), AttemptId::FIRST, "native", events)
                .with_dispatch(Dispatch::AcceptanceUnknown);
        let (first, second) = tokio::join!(sink.complete(), sink.cancel(CancelReason::Requested));
        first.expect("first terminal");
        second.expect("duplicate terminal is harmless");
        assert_eq!(
            stream.terminal_status(),
            Some(super::TerminalStatus::Completed)
        );
        assert_eq!(stream.dispatch(), Dispatch::AcceptanceUnknown);
        assert!(sink.is_terminal());
        sink.terminated().await;
        assert!(
            sink.emit(EventKind::TextDelta {
                text: "late".into()
            })
            .await
            .is_err()
        );
        stream.recv().await.expect("queued");
        assert!(stream.recv().await.expect("terminal").is_terminal());
        assert!(stream.recv().await.is_none());
    }

    #[tokio::test]
    async fn receiver_drop_wakes_an_owner_without_vendor_output() {
        let (sink, events) = sink(1);
        let observer = sink.clone();
        drop(sink);
        drop(events);
        observer.closed().await;
        assert!(observer.is_closed());
    }

    #[tokio::test]
    async fn reserved_failure_and_status_do_not_retain_an_unbounded_error_code() {
        let (sink, mut events) = sink(1);
        sink.fail(crate::VendorError::new(
            crate::ErrorCode::new("x".repeat(1_000_000)),
            "failure",
        ))
        .await
        .expect("terminal");
        let Some(super::TerminalStatus::Failed { code }) = events.terminal_status() else {
            panic!("expected failed status")
        };
        assert!(
            code.as_str().len() <= 1024,
            "expected a bounded terminal status code"
        );
        let EventKind::Error { error } = events.recv().await.expect("terminal").kind else {
            panic!("expected error")
        };
        assert!(
            error.code.as_str().len() <= 1024,
            "expected a bounded terminal event code"
        );
    }

    #[tokio::test]
    async fn dropping_the_last_sender_wakes_a_waiting_receiver() {
        let (sink, mut events) = sink(1);
        let copy = sink.clone();
        drop(sink);
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        drop(copy);
        assert!(events.recv().await.is_none());
    }

    /// A clock that never moves, so an event's stamp is a value a test can assert on.
    struct FrozenClock(SystemTime);

    impl Clock for FrozenClock {
        fn now(&self) -> SystemTime {
            self.0
        }
    }

    fn sink(capacity: usize) -> (EventSink, EventReceiver) {
        EventSink::new(
            SessionId::new("session-1"),
            TurnId::new("turn-1"),
            AttemptId::new(1),
            Arc::new(SystemClock),
            capacity,
        )
    }

    #[test]
    fn event_sink_debug_omits_host_routing_ids() {
        let (sink, _events) = EventSink::new(
            SessionId::new("session-id-secret"),
            TurnId::new("turn-id-secret"),
            AttemptId::FIRST,
            Arc::new(SystemClock),
            1,
        );

        let rendered = format!("{sink:?}");
        for secret in ["session-id-secret", "turn-id-secret"] {
            assert!(
                !rendered.contains(secret),
                "expected no host id in event sink diagnostics, received {rendered}"
            );
        }
    }

    /// The sink is what a harness holds; these two are what a host holds.
    ///
    /// `TurnStream` is the value returned from `start_turn`, so it is the one a host reaches for
    /// with `dbg!` or embeds in a type of its own. `native_turn_id` is the vendor's on ACP and
    /// Codex, and `review_thread_id` is the vendor's everywhere.
    #[test]
    fn stream_handle_debug_omits_the_ids_it_routes_on() {
        let (_sink, events) = EventSink::new(
            SessionId::new("session-1"),
            TurnId::new("turn-1"),
            AttemptId::FIRST,
            Arc::new(SystemClock),
            1,
        );
        let review = crate::stream::ReviewStream {
            turn: TurnStream {
                turn_id: TurnId::new("turn-id-secret"),
                attempt: AttemptId::FIRST,
                native_turn_id: String::from("native-turn-id-secret"),
                dispatch: Dispatch::Accepted,
                events,
            },
            review_thread_id: String::from("review-thread-id-secret"),
        };

        for rendered in [format!("{:?}", review.turn), format!("{review:?}")] {
            for secret in [
                "turn-id-secret",
                "native-turn-id-secret",
                "review-thread-id-secret",
            ] {
                assert!(
                    !rendered.contains(secret),
                    "expected no routing id in stream diagnostics, received {rendered}"
                );
            }
        }
        assert!(
            format!("{review:?}").contains("has_review_thread_id: true"),
            "expected the thread to be reported as present, received {review:?}"
        );
    }

    #[tokio::test]
    async fn stamps_every_event_with_its_session_turn_attempt_and_time() {
        let stamped = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let (sink, mut events) = EventSink::new(
            SessionId::new("session-1"),
            TurnId::new("turn-1"),
            AttemptId::new(2),
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
        assert_eq!(
            event.attempt,
            AttemptId::new(2),
            "expected the attempt on the event, so a late one from an abandoned dispatch is              recognisable"
        );
        assert_eq!(event.at, stamped);
        assert_eq!(event.operation(), sink.operation());
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
            .emit(EventKind::TurnStarted {
                native_turn_id: String::from("  "),
            })
            .await
            .expect_err("expected a refusal, received a send");
        assert!(
            matches!(
                error,
                Error::InvalidVendorValue {
                    field: "native turn id",
                    ..
                }
            ),
            "expected an invalid turn id, received {error:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_stream_reports_overflow_without_blocking_control() {
        let (sink, mut events) = sink(1);
        sink.emit(EventKind::TextDelta {
            text: "queued".into(),
        })
        .await
        .expect("first event");
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            sink.emit(EventKind::TextDelta {
                text: "overflow".into(),
            }),
        )
        .await;
        assert!(
            matches!(result, Ok(Err(Error::LimitExceeded { .. }))),
            "expected immediate explicit overflow, received {result:?}"
        );
        assert!(matches!(
            events.recv().await.expect("queued event").kind,
            EventKind::TextDelta { .. }
        ));
        assert!(matches!(
            events.recv().await.expect("reserved terminal").kind,
            EventKind::Error { .. }
        ));
        assert!(events.recv().await.is_none());
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
    async fn closing_commits_a_complete_terminal_behind_a_full_one_slot_stream() {
        let (sink, mut events) = sink(1);
        sink.emit(EventKind::TextDelta {
            text: "queued".into(),
        })
        .await
        .expect("queued");
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            sink.cancel_on_close(CancelReason::Shutdown),
        )
        .await;
        assert!(
            matches!(result, Ok(Ok(()))),
            "expected nonblocking terminal commit, received {result:?}"
        );
        assert!(matches!(
            events.recv().await.expect("queued").kind,
            EventKind::TextDelta { .. }
        ));
        assert!(matches!(
            events.recv().await.expect("marker").kind,
            EventKind::Cancelled { .. }
        ));
        assert_eq!(
            events.recv().await.expect("terminal").kind,
            EventKind::Completed
        );
        assert!(events.recv().await.is_none());
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
        let mut turn =
            TurnStream::accepted(TurnId::new("turn-1"), AttemptId::new(1), "native-1", events);
        assert_eq!(turn.dispatch(), Dispatch::Accepted);
        assert_eq!(turn.native_turn_id(), "native-1");

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
