//! A bounded transcript with terminal storage independent of reader progress.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::{Notify, mpsc::error::TryRecvError};

use super::TerminalStatus;
use crate::{AgentEvent, CancelToken, Error, EventKind, Limits, Result};

/// The serialized size one interaction event is budgeted at.
///
/// An `ApprovalRequested` carries a title, a detail and the options the vendor offered; 8 KiB
/// holds a realistic one whole with room for a command line and a short diff summary. It is a
/// budgeting figure, not a cap: a single event is never truncated to it.
const INTERACTION_EVENT_BYTES: usize = 8 * 1024;

/// How many bytes of the turn budget are held for interaction events.
///
/// Derived from the count reserve so the two cannot drift: [`control_event_cap`] events of
/// [`INTERACTION_EVENT_BYTES`] each, clamped to half the turn budget so a host that configures a
/// small `turn_buffer_bytes` still has payload room. At the defaults that is
/// `64 * 2 * 8 KiB = 1 MiB` of the 8 MiB budget, leaving 7 MiB for payload; the clamp does not
/// bind there because half of 8 MiB is 4 MiB.
fn control_reserve_bytes(limits: &Limits) -> usize {
    control_event_cap(limits)
        .saturating_mul(INTERACTION_EVENT_BYTES)
        .min(limits.turn_buffer_bytes / 2)
}

/// How many interaction events one turn queues, the count reserve both budgets are derived from.
fn control_event_cap(limits: &Limits) -> usize {
    limits.max_pending_requests.saturating_mul(2).max(2)
}

struct Queued {
    event: AgentEvent,
    bytes: usize,
    control: bool,
}

#[derive(Default)]
struct State {
    events: VecDeque<Queued>,
    terminal: VecDeque<AgentEvent>,
    status: Option<TerminalStatus>,
    payload_bytes: usize,
    control_bytes: usize,
    payloads: usize,
    controls: usize,
}

impl State {
    /// What a host compares against its `turn_buffer_bytes`: both classes together.
    fn queued_bytes(&self) -> usize {
        self.payload_bytes.saturating_add(self.control_bytes)
    }
}

pub(super) struct Buffer {
    state: Mutex<State>,
    limits: Limits,
    changed: Notify,
    abandoned: CancelToken,
    terminated: CancelToken,
    senders: AtomicUsize,
}

impl Buffer {
    pub(super) fn new(limits: Limits) -> (Arc<Self>, EventReceiver) {
        let buffer = Arc::new(Self {
            state: Mutex::new(State::default()),
            limits,
            changed: Notify::new(),
            abandoned: CancelToken::new(),
            terminated: CancelToken::new(),
            senders: AtomicUsize::new(1),
        });
        (Arc::clone(&buffer), EventReceiver { buffer })
    }

    pub(super) fn add_sender(&self) {
        self.senders.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn remove_sender(&self) {
        if self.senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.changed.notify_waiters();
        }
    }

    pub(super) fn push(&self, event: AgentEvent) -> Result<()> {
        let bytes = payload_bytes(&event)?;
        let control = matches!(
            event.kind,
            EventKind::ApprovalRequested { .. }
                | EventKind::ApprovalResolved { .. }
                | EventKind::QuestionAsked { .. }
                | EventKind::QuestionResolved { .. }
        );
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if self.is_closed() || state.status.is_some() {
            return Err(Error::Closed {
                subject: "turn stream",
            });
        }
        let (count, cap) = if control {
            (state.controls, control_event_cap(&self.limits))
        } else {
            (state.payloads, self.limits.turn_channel_capacity.max(1))
        };
        if count >= cap {
            return Err(Error::LimitExceeded {
                subject: "queued turn events",
                limit: cap,
                received: count.saturating_add(1),
            });
        }
        if !control {
            // Payload stops short of the interaction reserve, so an approval the vendor raises
            // under byte pressure still has room to reach the host that must answer it.
            let payload_budget = self
                .limits
                .turn_buffer_bytes
                .saturating_sub(control_reserve_bytes(&self.limits));
            let payload_total = state.payload_bytes.saturating_add(bytes);
            if payload_total > payload_budget {
                return Err(Error::LimitExceeded {
                    subject: "queued turn payload bytes",
                    limit: payload_budget,
                    received: payload_total,
                });
            }
        }
        // Interactions may spend the payload area while it is free, but the two classes together
        // never exceed the budget the host sized its memory against.
        let total = state.queued_bytes().saturating_add(bytes);
        if total > self.limits.turn_buffer_bytes {
            return Err(Error::LimitExceeded {
                subject: if control {
                    "queued turn control bytes"
                } else {
                    "queued turn payload bytes"
                },
                limit: self.limits.turn_buffer_bytes,
                received: total,
            });
        }
        if control {
            state.control_bytes += bytes;
            state.controls += 1;
        } else {
            state.payload_bytes += bytes;
            state.payloads += 1;
        }
        state.events.push_back(Queued {
            event,
            bytes,
            control,
        });
        drop(state);
        self.changed.notify_one();
        Ok(())
    }

    pub(super) fn finish(&self, events: Vec<AgentEvent>, status: TerminalStatus) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.status.is_some() {
            return Ok(());
        }
        state.status = Some(status);
        if !self.is_closed() {
            state.terminal.extend(events);
        }
        drop(state);
        self.terminated.cancel();
        self.changed.notify_waiters();
        if self.is_closed() {
            return Err(Error::Closed {
                subject: "turn stream",
            });
        }
        Ok(())
    }

    pub(super) fn status(&self) -> Option<TerminalStatus> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .status
            .clone()
    }

    pub(super) fn is_closed(&self) -> bool {
        self.abandoned.is_cancelled()
    }
    pub(super) async fn closed(&self) {
        self.abandoned.cancelled().await;
    }
    pub(super) async fn terminated(&self) {
        self.terminated.cancelled().await;
    }
}

/// The receiving half of a bounded turn transcript.
///
/// Dropping it notifies its owner immediately, even when the vendor produces no further output.
pub struct EventReceiver {
    buffer: Arc<Buffer>,
}

impl EventReceiver {
    /// Reads the next queued event, including a reserved terminal, or waits for one.
    ///
    /// For example, a harness passes this receiver to `TurnStream::accepted`.
    pub async fn recv(&mut self) -> Option<AgentEvent> {
        let buffer = Arc::clone(&self.buffer);
        loop {
            let changed = buffer.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match self.try_recv() {
                Ok(event) => return Some(event),
                Err(TryRecvError::Disconnected) => return None,
                Err(TryRecvError::Empty) => changed.await,
            }
        }
    }

    /// Reads without waiting, for example when draining events during a host poll.
    pub fn try_recv(&mut self) -> std::result::Result<AgentEvent, TryRecvError> {
        let mut state = self
            .buffer
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(queued) = state.events.pop_front() {
            if queued.control {
                state.control_bytes -= queued.bytes;
                state.controls -= 1;
            } else {
                state.payload_bytes -= queued.bytes;
                state.payloads -= 1;
            }
            return Ok(queued.event);
        }
        if let Some(event) = state.terminal.pop_front() {
            return Ok(event);
        }
        if state.status.is_some() || self.buffer.senders.load(Ordering::Acquire) == 0 {
            return Err(TryRecvError::Disconnected);
        }
        Err(TryRecvError::Empty)
    }

    /// Inspects the committed terminal without consuming any transcript events.
    ///
    /// A host can reconcile a lost acknowledgement through `TurnStream::terminal_status`.
    pub fn terminal_status(&self) -> Option<TerminalStatus> {
        self.buffer.status()
    }

    /// Returns queued serialized bytes, payload and interactions together, excluding the
    /// reserved terminal.
    ///
    /// This can be compared with the host's `Limits::turn_buffer_bytes`: the two classes are
    /// budgeted apart, but their total never exceeds that one number.
    pub fn queued_bytes(&self) -> usize {
        self.buffer
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .queued_bytes()
    }
}

impl Drop for EventReceiver {
    fn drop(&mut self) {
        self.buffer.abandoned.cancel();
        let mut state = self
            .buffer
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.events.clear();
        state.terminal.clear();
        state.payload_bytes = 0;
        state.control_bytes = 0;
        state.payloads = 0;
        state.controls = 0;
    }
}

/// Counts the encoded payload without allocating another copy of it.
fn payload_bytes(event: &AgentEvent) -> Result<usize> {
    #[derive(Default)]
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter::default();
    serde_json::to_writer(&mut counter, event).map_err(|_| Error::Protocol {
        expected: String::from("a serializable normalized event"),
        received: String::from("serialization failure"),
    })?;
    Ok(counter.0)
}

#[cfg(test)]
mod tests {
    use super::{Buffer, control_reserve_bytes};
    use crate::event::{ActivityKind, AgentEvent, EventKind, SessionId, TurnId};
    use crate::interaction::{Interaction, InteractionId, InteractionKind};
    use crate::operation::AttemptId;
    use crate::permission::{
        PermissionEffect, PermissionOption, PermissionRequest, PermissionScope,
    };
    use crate::{Error, Limits};
    use std::time::SystemTime;

    /// Small enough to fill with a handful of events, large enough that the half reserved for
    /// interactions still holds several of them: a payload delta is 205 serialized bytes here and
    /// an approval 452.
    const SMALL_TURN_BUDGET: usize = 4_096;

    /// A turn whose payload area fills long before any count budget is reached.
    fn limits() -> Limits {
        Limits {
            turn_buffer_bytes: SMALL_TURN_BUDGET,
            ..Limits::default()
        }
    }

    fn event(kind: EventKind) -> AgentEvent {
        AgentEvent {
            session_id: SessionId::new("chat-1"),
            turn_id: TurnId::new("turn-1"),
            attempt: AttemptId::FIRST,
            at: SystemTime::UNIX_EPOCH,
            kind,
        }
    }

    fn text_delta() -> AgentEvent {
        event(EventKind::TextDelta {
            text: "a".repeat(64),
        })
    }

    /// Every id is the same length, so each approval costs the same number of bytes.
    fn approval(id: &str) -> AgentEvent {
        event(EventKind::ApprovalRequested {
            request: PermissionRequest::new(
                Interaction::new(
                    InteractionId::new(id),
                    InteractionKind::Permission,
                    SessionId::new("chat-1"),
                    SystemTime::UNIX_EPOCH,
                ),
                ActivityKind::Command,
                "Run `ls`",
                vec![
                    PermissionOption::new("yes", PermissionEffect::Allow)
                        .with_scope(PermissionScope::Once),
                ],
            ),
        })
    }

    /// Pushes deltas until one is refused, returning that refusal.
    fn fill_payload(buffer: &Buffer) -> Error {
        for _ in 0..10_000 {
            if let Err(error) = buffer.push(text_delta()) {
                return error;
            }
        }
        panic!("expected a payload refusal within the turn budget, received room for 10,000 deltas")
    }

    /// The bug this guards: a turn whose deltas filled the byte budget could not queue the
    /// approval the vendor had just raised, so the host was never asked and could never answer.
    #[test]
    fn an_interaction_event_is_admitted_when_payload_has_filled_the_turn_budget() {
        let (buffer, events) = Buffer::new(limits());
        let refusal = fill_payload(&buffer);
        assert!(
            matches!(
                refusal,
                Error::LimitExceeded {
                    subject: "queued turn payload bytes",
                    ..
                }
            ),
            "expected the payload area to be full, received {refusal:?}"
        );

        buffer
            .push(approval("req-01"))
            .expect("expected the interaction reserve to admit an approval under byte pressure");

        assert!(
            events.queued_bytes() <= SMALL_TURN_BUDGET,
            "expected the total to stay within the turn budget of {SMALL_TURN_BUDGET}, received {}",
            events.queued_bytes()
        );
    }

    /// The reserve is a reserve, not an escape hatch: interactions past it are refused too.
    #[test]
    fn interaction_events_past_the_byte_reserve_are_refused() {
        let (buffer, events) = Buffer::new(limits());
        fill_payload(&buffer);

        let mut admitted = 0;
        let refusal = loop {
            let id = format!("req-{admitted:02}");
            match buffer.push(approval(&id)) {
                Ok(()) => admitted += 1,
                Err(error) => break error,
            }
            assert!(
                admitted < 128,
                "expected the byte reserve to refuse an approval before the count reserve did"
            );
        };

        assert!(
            admitted > 0,
            "expected the reserve to admit at least one approval, received {refusal:?}"
        );
        assert!(
            matches!(
                refusal,
                Error::LimitExceeded {
                    subject: "queued turn control bytes",
                    limit: SMALL_TURN_BUDGET,
                    ..
                }
            ),
            "expected a bounded interaction refusal naming the control subject, received {refusal:?}"
        );
        assert!(
            events.queued_bytes() <= SMALL_TURN_BUDGET,
            "expected the total to stay within the turn budget of {SMALL_TURN_BUDGET}, received {}",
            events.queued_bytes()
        );
    }

    /// Payload stops at the budget minus the reserve, so it can never eat the interaction room.
    #[test]
    fn payload_stops_short_of_the_interaction_reserve() {
        let (buffer, events) = Buffer::new(limits());
        let reserve = control_reserve_bytes(&limits());
        let payload_budget = SMALL_TURN_BUDGET - reserve;

        let refusal = fill_payload(&buffer);

        assert!(
            matches!(
                refusal,
                Error::LimitExceeded {
                    subject: "queued turn payload bytes",
                    limit,
                    ..
                } if limit == payload_budget
            ),
            "expected payload to be refused at {payload_budget}, the turn budget less the {reserve} byte interaction reserve, received {refusal:?}"
        );
        assert!(
            events.queued_bytes() <= payload_budget,
            "expected queued payload to stay under {payload_budget}, received {}",
            events.queued_bytes()
        );
    }

    /// Reading an event returns its bytes to the class that spent them, and only to that class.
    ///
    /// Asserted through what the buffer admits next rather than through the total: a release that
    /// credits the wrong counter moves bytes between the two areas while keeping the sum right.
    #[test]
    fn draining_returns_bytes_to_the_class_that_queued_them() {
        let (buffer, mut events) = Buffer::new(limits());
        // Queued first, so reading it does not require draining payload first.
        buffer
            .push(approval("req-01"))
            .expect("expected an approval");
        fill_payload(&buffer);

        let first = events.try_recv().expect("expected the approval first");
        assert!(matches!(first.kind, EventKind::ApprovalRequested { .. }));
        let refusal = buffer
            .push(text_delta())
            .expect_err("expected the freed interaction bytes to stay out of the payload area");
        assert!(
            matches!(
                refusal,
                Error::LimitExceeded {
                    subject: "queued turn payload bytes",
                    ..
                }
            ),
            "expected payload to still be full after an interaction was read, received {refusal:?}"
        );

        events.try_recv().expect("expected a queued delta");
        buffer
            .push(text_delta())
            .expect("expected the drained payload room to be reusable");

        while events.try_recv().is_ok() {}
        assert_eq!(
            events.queued_bytes(),
            0,
            "expected a drained queue to report no bytes"
        );
    }

    /// The derivation the documentation states, at the defaults and at a budget too small for it.
    #[test]
    fn the_interaction_reserve_is_derived_from_pending_requests_and_clamped_to_half_the_budget() {
        let defaults = Limits::default();
        assert_eq!(
            control_reserve_bytes(&defaults),
            1024 * 1024,
            "expected 64 pending requests to reserve 128 interaction events of {} bytes, 1 MiB of the 8 MiB default",
            super::INTERACTION_EVENT_BYTES
        );
        assert_eq!(
            defaults.turn_buffer_bytes - control_reserve_bytes(&defaults),
            7 * 1024 * 1024,
            "expected 7 MiB left for payload at the defaults"
        );

        let tiny = Limits {
            turn_buffer_bytes: 4_096,
            ..Limits::default()
        };
        assert_eq!(
            control_reserve_bytes(&tiny),
            2_048,
            "expected the clamp to hold the reserve at half of a small turn budget"
        );
    }
}
