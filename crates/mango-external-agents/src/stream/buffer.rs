//! A bounded transcript with terminal storage independent of reader progress.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::{Notify, mpsc::error::TryRecvError};

use super::TerminalStatus;
use crate::{AgentEvent, CancelToken, Error, EventKind, Limits, Result};

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
    bytes: usize,
    payloads: usize,
    controls: usize,
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
            (
                state.controls,
                self.limits.max_pending_requests.saturating_mul(2).max(2),
            )
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
        let total = state.bytes.saturating_add(bytes);
        if total > self.limits.turn_buffer_bytes {
            return Err(Error::LimitExceeded {
                subject: "queued turn payload bytes",
                limit: self.limits.turn_buffer_bytes,
                received: total,
            });
        }
        state.bytes = total;
        if control {
            state.controls += 1;
        } else {
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
        loop {
            let changed = self.buffer.changed.notified();
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
    pub fn try_recv(&self) -> std::result::Result<AgentEvent, TryRecvError> {
        let mut state = self
            .buffer
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(queued) = state.events.pop_front() {
            state.bytes -= queued.bytes;
            if queued.control {
                state.controls -= 1;
            } else {
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

    /// Returns queued serialized payload bytes, excluding the reserved terminal.
    ///
    /// This can be compared with the host's `Limits::turn_buffer_bytes`.
    pub fn queued_bytes(&self) -> usize {
        self.buffer
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .bytes
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
        state.bytes = 0;
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
