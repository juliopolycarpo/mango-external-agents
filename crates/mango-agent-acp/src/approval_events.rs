//! Approval events registered before a wire response can finish the prompt.

use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};

use mango_external_agents::{EventKind, EventSink, Result};

/// Synchronous registration and serial delivery keep a prompt terminal behind its approvals.
#[derive(Default)]
pub(crate) struct ApprovalEvents {
    queued: Mutex<VecDeque<EventKind>>,
    delivery: tokio::sync::Mutex<()>,
}

impl ApprovalEvents {
    pub(crate) fn push(&self, event: EventKind) {
        self.queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(event);
    }

    /// A successful wire response and its audit become visible to terminal delivery together.
    pub(crate) fn record_response<E>(
        &self,
        event: EventKind,
        respond: impl FnOnce() -> std::result::Result<(), E>,
    ) -> std::result::Result<(), E> {
        let mut queued = self.queued.lock().unwrap_or_else(PoisonError::into_inner);
        respond()?;
        queued.push_back(event);
        Ok(())
    }

    /// Delivery can wait for the host; registering an expiry and replying on the wire never does.
    pub(crate) async fn flush(&self, sink: &EventSink) -> Result<()> {
        let _delivery = self.delivery.lock().await;
        loop {
            let event = self
                .queued
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .front()
                .cloned();
            let Some(event) = event else {
                break;
            };
            sink.emit(event).await?;
            // The async gate excludes another delivery; pushes only append to the queue.
            // Keep the front until send succeeds so aborting the driver cannot lose this event.
            self.queued
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::ApprovalEvents;
    use mango_external_agents::{EventKind, EventSink, SystemClock};
    use std::future::{Future, poll_fn};
    use std::sync::Arc;
    use std::task::Poll;

    #[tokio::test]
    async fn an_interrupted_delivery_keeps_the_event_for_the_terminal_flush() {
        let (sink, mut received) = EventSink::new(
            mango_external_agents::SessionId::new("chat"),
            mango_external_agents::TurnId::new("turn"),
            Arc::new(SystemClock),
            1,
        );
        sink.emit(EventKind::TextDelta {
            text: String::from("already buffered"),
        })
        .await
        .expect("first event");
        let approvals = ApprovalEvents::default();
        approvals.push(EventKind::TextDelta {
            text: String::from("queued approval"),
        });
        let mut interrupted = Box::pin(approvals.flush(&sink));
        poll_fn(|cx| {
            assert!(
                interrupted.as_mut().poll(cx).is_pending(),
                "expected full channel to park delivery"
            );
            Poll::Ready(())
        })
        .await;
        drop(interrupted);
        received.recv().await.expect("buffered event");
        approvals.flush(&sink).await.expect("terminal flush");
        let event = received
            .try_recv()
            .expect("expected interrupted approval to survive until terminal flush");
        assert!(matches!(event.kind, EventKind::TextDelta { text } if text == "queued approval"));
    }

    struct FakeResponse {
        fails: bool,
    }

    impl FakeResponse {
        fn send(&self) -> std::result::Result<(), &'static str> {
            if self.fails {
                return Err("transport closed");
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn only_successful_wire_responses_enter_the_audit_queue() {
        let (sink, mut received) = EventSink::new(
            mango_external_agents::SessionId::new("chat"),
            mango_external_agents::TurnId::new("turn"),
            Arc::new(SystemClock),
            1,
        );
        let approvals = ApprovalEvents::default();
        for fails in [true, false] {
            let response = FakeResponse { fails };
            let result = approvals.record_response(
                EventKind::TextDelta {
                    text: String::from("resolved"),
                },
                || response.send(),
            );
            assert_eq!(result.is_err(), fails);
            approvals.flush(&sink).await.expect("flush");
            assert_eq!(
                received.try_recv().is_err(),
                fails,
                "a failed wire send must not claim an audit resolution"
            );
        }
    }
}
