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

    /// The sink admits a bounded control reserve, so delivery never waits for transcript capacity.
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
    use mango_external_agents::{EventKind, EventSink, Limits, SystemClock};
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn an_approval_progresses_when_transcript_capacity_is_full() {
        let limits = Limits {
            turn_channel_capacity: 1,
            max_pending_requests: 1,
            ..Limits::default()
        };
        let (sink, mut received) = EventSink::with_limits(
            mango_external_agents::SessionId::new("chat"),
            mango_external_agents::TurnId::new("turn"),
            mango_external_agents::AttemptId::default(),
            Arc::new(SystemClock),
            &limits,
        );
        sink.emit(EventKind::TextDelta {
            text: String::from("already buffered"),
        })
        .await
        .expect("first event");
        let approvals = ApprovalEvents::default();
        approvals.push(EventKind::ApprovalResolved {
            interaction_id: mango_external_agents::InteractionId::new("approval-1"),
            decision: mango_external_agents::ApprovalDecision::unresolved(
                "cancelled",
                mango_external_agents::DecisionSource::Cancelled,
            ),
        });
        tokio::time::timeout(Duration::from_millis(50), approvals.flush(&sink))
            .await
            .expect("approval delivery must use the control reserve")
            .expect("approval delivery");
        let transcript = received.recv().await.expect("buffered transcript");
        let approval = received.recv().await.expect("reserved approval");
        assert!(matches!(transcript.kind, EventKind::TextDelta { .. }));
        assert!(
            matches!(approval.kind, EventKind::ApprovalResolved { interaction_id, .. } if interaction_id.as_str() == "approval-1")
        );
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
            mango_external_agents::AttemptId::default(),
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
